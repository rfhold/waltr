//! Immutable Routine payloads. Unreferenced objects are retained; no deletion or replay API.
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type ObjectFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ObjectError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectError { Invalid, Missing, Corrupt, Unavailable }
impl std::fmt::Display for ObjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Routine object {self:?}") }
}
impl std::error::Error for ObjectError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace { Program, Text }
impl Namespace {
    fn name(self) -> &'static str { match self { Self::Program => "program", Self::Text => "text" } }
    fn limit(self) -> usize { match self { Self::Program => 131072, Self::Text => 1048576 } }
}

/// Host-internal reference, never an authorization capability or caller-provided path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRef { pub namespace: Namespace, pub sha256: String, pub bytes: u64 }
impl ObjectRef {
    pub fn new(namespace: Namespace, sha256: String, bytes: u64) -> Result<Self, ObjectError> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) || bytes > namespace.limit() as u64 {
            return Err(ObjectError::Invalid);
        }
        Ok(Self { namespace, sha256, bytes })
    }
    pub fn for_bytes(namespace: Namespace, bytes: &[u8]) -> Result<Self, ObjectError> {
        Self::new(namespace, digest(bytes), bytes.len() as u64)
    }
    fn key(&self) -> String { format!("{}/{}", self.namespace.name(), self.sha256) }
    fn verify(&self, bytes: &[u8]) -> Result<(), ObjectError> {
        Self::new(self.namespace, self.sha256.clone(), self.bytes)?;
        if bytes.len() as u64 != self.bytes || digest(bytes) != self.sha256 { return Err(ObjectError::Corrupt); }
        Ok(())
    }
}
fn digest(bytes: &[u8]) -> String { Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect() }

/// Implementations must never replace an existing object and must verify every read.
/// Successful put means durable bytes and verified integrity before SQL publication.
pub trait ImmutableObjects: Send + Sync {
    fn put<'a>(&'a self, reference: &'a ObjectRef, bytes: &'a [u8]) -> ObjectFuture<'a, ()>;
    fn get<'a>(&'a self, reference: &'a ObjectRef) -> ObjectFuture<'a, Vec<u8>>;
}

#[derive(Clone)]
pub struct FilesystemObjects { root: PathBuf }
impl FilesystemObjects {
    /// Root must be durable host-private storage outside the sandbox workspace.
    pub fn new(root: PathBuf) -> Result<Self, ObjectError> {
        if !root.is_absolute() { return Err(ObjectError::Invalid); }
        Ok(Self { root })
    }
}
impl ImmutableObjects for FilesystemObjects {
    fn put<'a>(&'a self, reference: &'a ObjectRef, bytes: &'a [u8]) -> ObjectFuture<'a, ()> {
        Box::pin(async move {
            reference.verify(bytes)?;
            let directory = self.root.join(reference.namespace.name());
            let mut builder = tokio::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder.create(&directory).await.map_err(|_| ObjectError::Unavailable)?;
            let path = self.root.join(reference.key());
            let temporary = directory.join(format!(".pending-{}", uuid::Uuid::new_v4()));
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await.map_err(|_| ObjectError::Unavailable)?;
            file.write_all(bytes).await.map_err(|_| ObjectError::Unavailable)?;
            file.sync_all().await.map_err(|_| ObjectError::Unavailable)?;
            drop(file);
            match tokio::fs::hard_link(&temporary, &path).await {
                Ok(()) => {},
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
                Err(_) => return Err(ObjectError::Unavailable),
            }
            tokio::fs::remove_file(&temporary).await.map_err(|_| ObjectError::Unavailable)?;
            // Persist the namespace and its parent directory entries before publishing SQL.
            for dir in directory.ancestors() {
                tokio::fs::File::open(dir).await.map_err(|_| ObjectError::Unavailable)?.sync_all().await.map_err(|_| ObjectError::Unavailable)?;
            }
            self.get(reference).await.map(|_| ())
        })
    }
    fn get<'a>(&'a self, reference: &'a ObjectRef) -> ObjectFuture<'a, Vec<u8>> {
        Box::pin(async move {
            ObjectRef::new(reference.namespace, reference.sha256.clone(), reference.bytes)?;
            let file = tokio::fs::File::open(self.root.join(reference.key())).await.map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { ObjectError::Missing } else { ObjectError::Unavailable })?;
            let mut bytes = Vec::with_capacity(reference.bytes as usize);
            file.take(reference.bytes + 1).read_to_end(&mut bytes).await.map_err(|_| ObjectError::Unavailable)?;
            reference.verify(&bytes)?;
            Ok(bytes)
        })
    }
}

/// S3 uses conditional creation, never a mutable session-store write path.
pub struct S3Objects { client: aws_sdk_s3::Client, bucket: String, prefix: String }
impl S3Objects {
    pub fn new(client: aws_sdk_s3::Client, bucket: String, prefix: String) -> Result<Self, ObjectError> {
        if bucket.trim().is_empty() || prefix.is_empty() || prefix.starts_with('/') || prefix.split('/').any(|p| p.is_empty() || p == "." || p == "..") { return Err(ObjectError::Invalid); }
        Ok(Self { client, bucket, prefix })
    }
    fn key(&self, reference: &ObjectRef) -> String { format!("{}/routine-objects/{}", self.prefix, reference.key()) }
}
impl ImmutableObjects for S3Objects {
    fn put<'a>(&'a self, reference: &'a ObjectRef, bytes: &'a [u8]) -> ObjectFuture<'a, ()> {
        Box::pin(async move {
            reference.verify(bytes)?;
            let outcome = self.client.put_object().bucket(&self.bucket).key(self.key(reference)).if_none_match("*")
                .body(aws_sdk_s3::primitives::ByteStream::from(bytes.to_vec())).send().await;
            if let Err(error) = outcome {
                if error.raw_response().map(|r| r.status().as_u16()) != Some(412) { return Err(ObjectError::Unavailable); }
            }
            self.get(reference).await.map(|_| ())
        })
    }
    fn get<'a>(&'a self, reference: &'a ObjectRef) -> ObjectFuture<'a, Vec<u8>> {
        Box::pin(async move {
            ObjectRef::new(reference.namespace, reference.sha256.clone(), reference.bytes)?;
            let result = self.client.get_object().bucket(&self.bucket).key(self.key(reference)).send().await.map_err(|e| if e.raw_response().map(|r| r.status().as_u16()) == Some(404) { ObjectError::Missing } else { ObjectError::Unavailable })?;
            if result.content_length() != Some(reference.bytes as i64) { return Err(ObjectError::Corrupt); }
            let mut bytes = Vec::with_capacity(reference.bytes as usize);
            result.body.into_async_read().take(reference.bytes + 1).read_to_end(&mut bytes).await.map_err(|_| ObjectError::Unavailable)?;
            reference.verify(&bytes)?;
            Ok(bytes)
        })
    }
}

/// Standalone environment adapter; embedded callers construct a filesystem store explicitly.
pub fn from_env(workspace: &std::path::Path) -> Result<Arc<dyn ImmutableObjects>, ObjectError> {
    let get = |name: &str| std::env::var(name).map_err(|_| ObjectError::Invalid);
    let backend = match std::env::var("WALTR_ROUTINE_OBJECT_STORE") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "filesystem".into(),
        Err(_) => return Err(ObjectError::Invalid),
    };
    match backend.as_str() {
        "filesystem" => {
            let root = if let Some(root) = std::env::var_os("WALTR_ROUTINE_OBJECT_ROOT") { PathBuf::from(root) } else {
                crate::persistence::standalone_data_directory().map_err(|_| ObjectError::Invalid)?.join("routine-objects")
            };
            if crate::workspace_paths::overlaps(workspace, &root).map_err(|_| ObjectError::Invalid)? { return Err(ObjectError::Invalid); }
            Ok(Arc::new(FilesystemObjects::new(root)?))
        },
        "s3" => {
            let endpoint = get("WALTR_ROUTINE_OBJECT_S3_ENDPOINT")?;
            let endpoint = url::Url::parse(&endpoint).map_err(|_| ObjectError::Invalid)?;
            if !(endpoint.scheme() == "https" || (endpoint.scheme() == "http" && endpoint.host_str().is_some_and(|h| h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())))) || !endpoint.username().is_empty() || endpoint.password().is_some() || endpoint.query().is_some() || endpoint.fragment().is_some() || endpoint.path() != "/" { return Err(ObjectError::Invalid); }
            let region = get("WALTR_ROUTINE_OBJECT_S3_REGION")?;
            let bucket = get("WALTR_ROUTINE_OBJECT_S3_BUCKET")?;
            let prefix = get("WALTR_ROUTINE_OBJECT_S3_PREFIX")?;
            let key = get("WALTR_ROUTINE_OBJECT_S3_ACCESS_KEY_ID")?;
            let secret = get("WALTR_ROUTINE_OBJECT_S3_SECRET_ACCESS_KEY")?;
            if [region.as_str(), key.as_str(), secret.as_str()].iter().any(|s| s.trim().is_empty()) { return Err(ObjectError::Invalid); }
            let force = match get("WALTR_ROUTINE_OBJECT_S3_FORCE_PATH_STYLE")?.as_str() { "true" => true, "false" => false, _ => return Err(ObjectError::Invalid) };
            let client = crate::session::storage::s3_client(endpoint.to_string(), region, key, secret, force);
            Ok(Arc::new(S3Objects::new(client, bucket, prefix)?))
        },
        _ => Err(ObjectError::Invalid),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn test_objects() -> Arc<dyn ImmutableObjects> {
    Arc::new(FilesystemObjects::new(crate::test_support::temp_root().join("objects")).unwrap())
}
