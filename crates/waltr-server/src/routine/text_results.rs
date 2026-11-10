//! Explicit durable business text, independent of interpreter, media and SQL types.
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};
use tokio_util::sync::CancellationToken;

pub(crate) const PUBLISH_CREDIT: u32 = 8 * 1024 * 1024;
const PROCESS_CREDIT: usize = 64 * 1024 * 1024;

pub(crate) fn process_credit() -> std::sync::Arc<tokio::sync::Semaphore> {
    static CREDIT: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    CREDIT
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(PROCESS_CREDIT)))
        .clone()
}

pub(crate) async fn reserve(
    bytes: u32,
    cancellation: &CancellationToken,
) -> Result<tokio::sync::OwnedSemaphorePermit, TextResultError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(TextResultError::Cancelled),
        permit = process_credit().acquire_many_owned(bytes) => permit.map_err(|_| TextResultError::Unavailable),
    }
}

/// Maximum UTF-8 octets in one selected business result.
pub const MAX_TEXT_BYTES: usize = 1_048_576;
/// Maximum sum of canonical encoded envelopes in one invocation.
pub const MAX_ENCODED_BYTES: usize = 8_388_608;
/// Maximum publication count per invocation.
pub const MAX_RESULTS: usize = 32;
/// Transport limit, not a character boundary or structured-value allowance.
pub const MAX_CHUNK_BYTES: usize = 65_536;

/// Owned, object-safe operation future; dropping before commit releases local resources.
pub type TextFuture<T> = Pin<Box<dyn Future<Output = Result<T, TextResultError>> + Send + 'static>>;
/// A source read borrows only the supplied bounded destination and source owner.
pub type TextSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<usize, TextResultError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Payload-free boundary errors. Never automatically retry an uncertain publication.
pub enum TextResultError {
    /// Missing invocation, foreign principal, or foreign/unknown result identity.
    Denied,
    /// Invalid UTF-8, metadata, offset, or transport bounds.
    Invalid,
    /// Immutable name already published, or invocation no longer running.
    Conflict,
    /// Per-result, aggregate, or finite count limit reached.
    Quota,
    /// Cancelled before the commit boundary; no publication occurred.
    Cancelled,
    /// Dependency failure before commit.
    Unavailable,
    /// Commit acknowledgment lost; inspect authorized descriptors, do not replay.
    Uncertain,
}
impl std::fmt::Display for TextResultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for TextResultError {}

#[derive(Clone)]
/// Host-supplied authenticated identity and service-generated durable invocation ID.
/// Construction validates bounds, not authority: every store operation must authorize it.
pub struct InvocationTextScope {
    principal: String,
    durable_invocation_id: String,
}
impl InvocationTextScope {
    /// No guest ID, correlation token, or result handle may substitute for these fields.
    pub fn new(principal: String, durable_invocation_id: String) -> Result<Self, TextResultError> {
        if principal.is_empty()
            || principal.len() > 1024
            || principal.capacity() > 4096
            || durable_invocation_id.capacity() > 1024
            || !bounded_identifier(&durable_invocation_id, 256)
        {
            return Err(TextResultError::Invalid);
        }
        Ok(Self {
            principal,
            durable_invocation_id,
        })
    }
    /// Exact authenticated principal, compared against durable caller provenance.
    pub fn principal(&self) -> &str {
        &self.principal
    }
    /// Service-owned durable invocation identity.
    pub fn invocation_id(&self) -> &str {
        &self.durable_invocation_id
    }
}
impl std::fmt::Debug for InvocationTextScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvocationTextScope")
            .finish_non_exhaustive()
    }
}

pub(crate) fn bounded_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Host-owned text source, without filesystem/URL fallback. Writes at most destination.len()
/// bytes; zero means EOF. UTF-8 may split across reads. No source-owned Vec crosses the port.
/// Dropping a pending read must be safe. Drop must synchronously release source ownership;
/// producers requiring async cancel/join must be joined by their resource owner beforehand.
/// This interface neither spawns nor claims to join detached producers.
pub trait TextSource: Send + 'static {
    /// Destination is always at most 64 KiB and is charged before the read starts.
    fn read<'a>(&'a mut self, destination: &'a mut [u8]) -> TextSourceFuture<'a>;
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Bounded metadata only. A result ID is an identity, never an authorization grant.
pub struct TextResultDescriptor {
    /// Descriptor format version.
    pub version: u32,
    /// UUID assigned by the store.
    pub id: String,
    /// Explicit selected name: 1..=64 ASCII identifier bytes.
    pub name: String,
    /// Lowercase SHA-256 of original UTF-8 octets.
    pub sha256: String,
    /// Original UTF-8 octet count.
    pub utf8_bytes: u64,
    /// Exact canonical envelope charge, including this field and text escaping.
    pub encoded_charge: u64,
    /// Optional host-validated helper identity, 1..=128 ASCII identifier bytes.
    /// Provenance only; it grants no helper-session access.
    pub helper_id: Option<String>,
}
impl std::fmt::Debug for TextResultDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextResultDescriptor")
            .field("utf8_bytes", &self.utf8_bytes)
            .field("encoded_charge", &self.encoded_charge)
            .finish_non_exhaustive()
    }
}

/// One bounded raw-octet read, retaining its process credit until Drop.
/// Chunk boundaries may bisect a codepoint; concatenate octets before decoding UTF-8.
pub struct TextChunk {
    bytes: Vec<u8>,
    _credit: tokio::sync::OwnedSemaphorePermit,
}
impl TextChunk {
    /// Build a bounded credited chunk in an alternative adapter, reserving before copying.
    pub async fn copy_from(
        bytes: &[u8],
        cancellation: CancellationToken,
    ) -> Result<Self, TextResultError> {
        if bytes.len() > MAX_CHUNK_BYTES {
            return Err(TextResultError::Invalid);
        }
        let credit = reserve(MAX_CHUNK_BYTES as u32, &cancellation).await?;
        Ok(Self::new(bytes.to_vec(), credit))
    }
    pub(crate) fn new(bytes: Vec<u8>, credit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self {
            bytes,
            _credit: credit,
        }
    }
    /// Borrowed raw UTF-8 octets; no replacement decoding or transfer out of credit ownership.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::fmt::Debug for TextChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextChunk")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Consumer-owned store. Results are selected business effects, not automatic task history.
pub trait TextResultStore: Send + Sync {
    /// Atomic immutable publication, only while the authorized invocation is running.
    /// Later invocation failure does not roll back a committed business effect.
    /// Cancellation is honored before commit; once commit starts, await its true outcome.
    /// Dropping the operation during commit leaves an unknown outcome: inspect authorized
    /// descriptors rather than replaying. Dropping a source is not an async producer join.
    #[allow(
        clippy::too_many_arguments,
        reason = "durable identity, result metadata, and source ownership remain explicit"
    )]
    fn publish(
        &self,
        scope: InvocationTextScope,
        name: String,
        helper_id: Option<String>,
        source: Box<dyn TextSource>,
        cancellation: CancellationToken,
    ) -> TextFuture<TextResultDescriptor>;
    /// At most 32 bounded descriptors, sorted by selected name. Never returns content.
    fn list(&self, scope: InvocationTextScope) -> TextFuture<Vec<TextResultDescriptor>>;
    /// Raw byte offset, max 1..=64 KiB; offset==size returns empty, offset>size is invalid.
    /// A guessed or cross-invocation result ID returns Denied before content allocation.
    #[allow(
        clippy::too_many_arguments,
        reason = "durable scope and bounded read coordinates remain explicit"
    )]
    fn read(
        &self,
        scope: InvocationTextScope,
        id: String,
        offset: u64,
        max: u32,
        cancellation: CancellationToken,
    ) -> TextFuture<TextChunk>;
}

#[cfg(test)]
mod tests;
