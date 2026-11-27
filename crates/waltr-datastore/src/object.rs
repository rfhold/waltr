//! Direct object access. Content lives at `<root-or-prefix>/<namespace>/<key>`.
//! Waltr receipts live separately at `<root-or-prefix>/.waltr-object-receipts/v1/`.
//! Prefix listings include the named object and its descendants, if any.

use std::fmt;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep, sleep_until, timeout};

use crate::{Actor, Attribution, Error, MAX_OBJECT_BYTES, MAX_PAGE, Namespace, object_key};

#[cfg(unix)]
mod filesystem;
#[cfg(windows)]
#[path = "object/windows.rs"]
mod filesystem;
mod s3;

const RECEIPTS: &str = ".waltr-object-receipts";
const RECEIPT_VERSION: &str = "v1";
const RECEIPT_BYTES: usize = 2048;
const OPEN_OPERATIONS: usize = 16;
const SCAN_LIMIT: usize = 100_000;
const OPERATION_TIME: Duration = Duration::from_secs(120);

type Reader = Box<dyn AsyncRead + Unpin + Send>;

/// Shared adapter and limits for one configured object backend.
#[derive(Clone)]
pub struct Objects {
    backend: Backend,
    operations: Arc<Semaphore>,
}

#[derive(Clone)]
enum Backend {
    #[cfg(any(unix, windows))]
    Filesystem(filesystem::Filesystem),
    S3(s3::S3),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
/// The mutation recorded in a Waltr receipt.
pub enum ObjectOperation {
    /// Published a new key.
    Create,
    /// Replaced the contents of a key.
    Replace,
    /// Removed the contents of a key.
    Delete,
}

/// Evidence of a Waltr mutation, not proof of current authorship.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WaltrWrite {
    /// The recorded mutation.
    pub operation: ObjectOperation,
    /// Proven actor information for that mutation.
    pub attribution: Attribution,
}

/// The descriptor of a key present in the authoritative backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObjectInfo {
    /// The relative UTF-8 object key.
    pub key: String,
    /// Byte count seen at the descriptor lookup.
    pub size: u64,
    /// Authorship only when the receipt proves continuity with the current object.
    pub attribution: Attribution,
    /// The newest retained Waltr receipt, independent of current authorship.
    pub last_known_waltr_write: Option<WaltrWrite>,
}

/// A bounded, lexically ordered page.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObjectPage {
    /// Metadata for the keys on this page.
    pub items: Vec<ObjectInfo>,
    /// Pass this key as `after` with the same namespace and prefix.
    pub next_after: Option<String>,
}

/// An authoritative descriptor and its staged read or open backend stream.
pub struct ObjectRead {
    /// Metadata for the object opened by this read.
    pub info: ObjectInfo,
    /// Bounded byte stream.
    pub body: ObjectBody,
}

/// An `AsyncRead` byte stream with an operation deadline.
pub struct ObjectBody {
    state: Arc<Mutex<BodyState>>,
    deadline: Pin<Box<Sleep>>,
    expiry: JoinHandle<()>,
}

enum BodyState {
    Open(tokio::io::Take<Reader>, OwnedSemaphorePermit),
    Closed,
    Expired,
}

impl ObjectBody {
    fn new(body: Reader, size: u64, permit: OwnedSemaphorePermit, deadline: Instant) -> Self {
        let state = Arc::new(Mutex::new(BodyState::Open(body.take(size), permit)));
        let weak = Arc::downgrade(&state);
        let expiry = tokio::spawn(async move {
            sleep_until(deadline).await;
            if let Some(state) = weak.upgrade()
                && let Ok(mut state) = state.lock()
                && matches!(*state, BodyState::Open(..))
            {
                *state = BodyState::Expired;
            }
        });
        Self {
            state,
            deadline: Box::pin(sleep_until(deadline)),
            expiry,
        }
    }
}

impl Drop for ObjectBody {
    fn drop(&mut self) {
        self.expiry.abort();
    }
}

impl fmt::Debug for Objects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Objects").finish_non_exhaustive()
    }
}

impl fmt::Debug for ObjectBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectBody").finish_non_exhaustive()
    }
}

impl fmt::Debug for ObjectRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectRead").finish_non_exhaustive()
    }
}

impl AsyncRead for ObjectBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::future::Future;
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let expired = self.deadline.as_mut().poll(cx).is_ready();
        let Ok(mut state) = self.state.lock() else {
            return Poll::Ready(Err(io::ErrorKind::Other.into()));
        };
        let polled = match &mut *state {
            BodyState::Closed => return Poll::Ready(Ok(())),
            BodyState::Expired => Err(io::ErrorKind::TimedOut.into()),
            BodyState::Open(..) if expired => Err(io::ErrorKind::TimedOut.into()),
            BodyState::Open(inner, _permit) => {
                let filled = buf.filled().len();
                match Pin::new(&mut *inner).poll_read(cx, buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(err)) => Err(err),
                    Poll::Ready(Ok(())) if inner.limit() == 0 => Ok(()),
                    Poll::Ready(Ok(())) if filled == buf.filled().len() => {
                        Err(io::ErrorKind::UnexpectedEof.into())
                    }
                    Poll::Ready(Ok(())) => return Poll::Ready(Ok(())),
                }
            }
        };
        *state = BodyState::Closed;
        self.expiry.abort();
        Poll::Ready(polled)
    }
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    write: WaltrWrite,
    identity: Option<String>,
}

impl Receipt {
    fn new(
        operation: ObjectOperation,
        actor: &Actor,
        previous: Attribution,
        identity: Option<String>,
    ) -> Self {
        let editor = actor.as_str().to_owned();
        let created_by = match operation {
            ObjectOperation::Create => Some(editor.clone()),
            ObjectOperation::Replace | ObjectOperation::Delete => previous.created_by,
        };
        Self {
            write: WaltrWrite {
                operation,
                attribution: Attribution {
                    created_by,
                    last_edited_by: Some(editor),
                },
            },
            identity,
        }
    }

    fn serialize(&self) -> Result<Vec<u8>, Error> {
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Unavailable)?;
        if bytes.len() > RECEIPT_BYTES {
            return Err(Error::Limit);
        }
        Ok(bytes)
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > RECEIPT_BYTES {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

fn object_info(
    key: &str,
    size: u64,
    identity: Option<&str>,
    receipt: Option<Receipt>,
) -> ObjectInfo {
    let attribution = match receipt.as_ref() {
        Some(receipt) if identity.is_some() && receipt.identity.as_deref() == identity => {
            receipt.write.attribution.clone()
        }
        _ => Attribution::default(),
    };
    ObjectInfo {
        key: key.to_owned(),
        size,
        attribution,
        last_known_waltr_write: receipt.map(|receipt| receipt.write),
    }
}

fn receipt_name(key: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(69);
    for byte in Sha256::digest(key) {
        name.push(char::from(HEX[usize::from(byte >> 4)]));
        name.push(char::from(HEX[usize::from(byte & 15)]));
    }
    name.push_str(".json");
    name
}

fn under_prefix(key: &str, prefix: Option<&str>) -> bool {
    prefix.is_none_or(|prefix| {
        key == prefix
            || key
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

/// Concurrency policy for replacing existing S3 datastore objects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum S3ReplacePolicy {
    /// Reject a replacement if the stored ETag changes.
    #[default]
    Conditional,
    /// Allow a replacement to overwrite a concurrent write.
    AllowStaleOverwrite,
}

impl Objects {
    /// Initialize the configured root. Its parent path is trusted configuration.
    pub fn filesystem(root: impl AsRef<Path>) -> Result<Self, Error> {
        #[cfg(any(unix, windows))]
        {
            Ok(Self::new(Backend::Filesystem(
                filesystem::Filesystem::open(root.as_ref())?,
            )))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = root;
            Err(Error::Unavailable)
        }
    }

    /// Use a dedicated bucket or a dedicated prefix in an ordinary S3 bucket.
    pub fn s3(
        client: aws_sdk_s3::Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Result<Self, Error> {
        Self::s3_with_replace_policy(client, bucket, prefix, S3ReplacePolicy::Conditional)
    }

    /// Configure S3 replacement semantics without changing create or delete.
    pub fn s3_with_replace_policy(
        client: aws_sdk_s3::Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        replace_policy: S3ReplacePolicy,
    ) -> Result<Self, Error> {
        Ok(Self::new(Backend::S3(s3::S3::new(
            client,
            bucket.into(),
            prefix.into(),
            replace_policy,
        )?)))
    }

    fn new(backend: Backend) -> Self {
        Self {
            backend,
            operations: Arc::new(Semaphore::new(OPEN_OPERATIONS)),
        }
    }

    fn admit(&self, ns: &Namespace) -> Result<OwnedSemaphorePermit, Error> {
        if !ns.as_str().starts_with("object/") {
            return Err(Error::Invalid);
        }
        self.operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Limit)
    }

    /// List a subtree, optionally after a key from an earlier page.
    pub async fn list(
        &self,
        ns: &Namespace,
        prefix: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<ObjectPage, Error> {
        if limit == 0 || limit > MAX_PAGE {
            return Err(Error::Limit);
        }
        if let Some(prefix) = prefix {
            object_key(prefix)?;
        }
        if let Some(after) = after {
            object_key(after)?;
            if !under_prefix(after, prefix) {
                return Err(Error::Invalid);
            }
        }
        let _permit = self.admit(ns)?;
        let mut items = timeout(OPERATION_TIME, async {
            match &self.backend {
                #[cfg(any(unix, windows))]
                Backend::Filesystem(backend) => backend.list(ns, prefix, after, limit + 1).await,
                Backend::S3(backend) => backend.list(ns, prefix, after, limit + 1).await,
            }
        })
        .await
        .map_err(|_| Error::Unavailable)??;
        let next_after = if items.len() > limit {
            items.pop();
            items.last().map(|item| item.key.clone())
        } else {
            None
        };
        Ok(ObjectPage { items, next_after })
    }

    /// Resolve a key without reading its content.
    pub async fn stat(&self, ns: &Namespace, key: &str) -> Result<ObjectInfo, Error> {
        object_key(key)?;
        let _permit = self.admit(ns)?;
        timeout(OPERATION_TIME, async {
            match &self.backend {
                #[cfg(any(unix, windows))]
                Backend::Filesystem(backend) => backend.stat(ns, key).await,
                Backend::S3(backend) => backend.stat(ns, key).await,
            }
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    /// Open a fresh byte stream. Objects larger than the transfer limit return `Error::Limit`.
    pub async fn read(&self, ns: &Namespace, key: &str) -> Result<ObjectRead, Error> {
        object_key(key)?;
        let permit = self.admit(ns)?;
        let deadline = Instant::now() + OPERATION_TIME;
        let (info, body) = timeout(OPERATION_TIME, async {
            match &self.backend {
                #[cfg(any(unix, windows))]
                Backend::Filesystem(backend) => backend.read(ns, key).await,
                Backend::S3(backend) => backend.read(ns, key).await,
            }
        })
        .await
        .map_err(|_| Error::Unavailable)??;
        if info.size > MAX_OBJECT_BYTES as u64 {
            return Err(Error::Limit);
        }
        let body = ObjectBody::new(body, info.size, permit, deadline);
        Ok(ObjectRead { info, body })
    }

    /// Publish the body only if the destination does not already exist.
    pub async fn create(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        body: impl AsyncRead + Unpin + Send,
    ) -> Result<ObjectInfo, Error> {
        self.put(ns, key, actor, body, ObjectOperation::Create)
            .await
    }

    /// Replace the contents of a present key.
    pub async fn replace(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        body: impl AsyncRead + Unpin + Send,
    ) -> Result<ObjectInfo, Error> {
        self.put(ns, key, actor, body, ObjectOperation::Replace)
            .await
    }

    async fn put(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        body: impl AsyncRead + Unpin + Send,
        operation: ObjectOperation,
    ) -> Result<ObjectInfo, Error> {
        object_key(key)?;
        let permit = self.admit(ns)?;
        match &self.backend {
            #[cfg(any(unix, windows))]
            Backend::Filesystem(backend) => {
                backend.put(ns, key, actor, body, operation, permit).await
            }
            Backend::S3(backend) => backend.put(ns, key, actor, body, operation, permit).await,
        }
    }

    /// Remove a present key and record the actor.
    pub async fn delete(&self, ns: &Namespace, key: &str, actor: &Actor) -> Result<(), Error> {
        object_key(key)?;
        let permit = self.admit(ns)?;
        match &self.backend {
            #[cfg(any(unix, windows))]
            Backend::Filesystem(backend) => backend.delete(ns, key, actor, permit).await,
            Backend::S3(backend) => backend.delete(ns, key, actor, permit).await,
        }
    }
}

#[cfg(all(test, any(unix, windows)))]
#[path = "object/tests.rs"]
mod tests;

async fn spool(
    body: impl AsyncRead + Unpin,
    target: &mut (impl AsyncWrite + Unpin),
) -> Result<u64, Error> {
    timeout(OPERATION_TIME, async {
        let size = tokio::io::copy(&mut body.take(MAX_OBJECT_BYTES as u64 + 1), target)
            .await
            .map_err(|_| Error::Unavailable)?;
        if size > MAX_OBJECT_BYTES as u64 {
            return Err(Error::Limit);
        }
        Ok(size)
    })
    .await
    .map_err(|_| Error::Unavailable)?
}
