//! Invocation-owned temporary blobs and finite output leases.

use std::{future::Future, pin::Pin, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub mod filesystem;
#[cfg(test)]
mod tests;

/// Maximum copied chunk on either side of the store boundary.
pub const CHUNK_BYTES: usize = 65_536;
/// Fixed, nonrenewable completed-output lifetime.
pub const OUTPUT_LEASE_MS: u64 = 15 * 60 * 1000;
/// Owned, cancellable asynchronous port result.
pub type BlobFuture<T> = Pin<Box<dyn Future<Output = Result<T, BlobError>> + Send + 'static>>;

/// Stable failures without filesystem names or input contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobError {
    /// Malformed metadata, corrupt data, or failed validation.
    Invalid,
    /// Authentication or ownership mismatch.
    Denied,
    /// Wrong generation, state, expired lease, or revoked handle.
    Stale,
    /// Lost, reordered, or duplicated upload chunk.
    Sequence,
    /// Byte, object, reader, or admission budget exhausted.
    Quota,
    /// Cooperative cancellation requested.
    Cancelled,
    /// Operation deadline elapsed.
    Deadline,
    /// Storage or worker unavailable.
    Unavailable,
    /// Cleanup incomplete; accounting remains held.
    Cleanup,
    /// Platform adapter absent.
    Unsupported,
}
impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BlobError {}

/// Authentication is supplied by the host, not inferred from an opaque ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobScope {
    /// Host-authenticated principal, bounded to 256 UTF-8 bytes.
    pub principal: String,
    /// Host-authenticated invocation, bounded to 256 UTF-8 bytes.
    pub invocation: String,
}
/// Every admitted operation checks cancellation and a monotonic deadline.
#[derive(Clone, Debug)]
pub struct Wait {
    /// Caller cancellation; dropped request futures cancel their child token.
    pub cancellation: CancellationToken,
    /// Deadline for cooperative waits, not an arbitrary OS syscall bound.
    pub deadline: tokio::time::Instant,
}
impl Wait {
    /// Check cancellation before deadline.
    pub fn check(&self) -> Result<(), BlobError> {
        if self.cancellation.is_cancelled() {
            Err(BlobError::Cancelled)
        } else if tokio::time::Instant::now() >= self.deadline {
            Err(BlobError::Deadline)
        } else {
            Ok(())
        }
    }
}
/// Monotonic milliseconds within one process instance; never wall-clock time.
pub trait Clock: Send + Sync {
    /// Monotonic elapsed milliseconds.
    fn now_ms(&self) -> u64;
}
/// Real process-local monotonic clock.
#[derive(Debug)]
pub struct MonotonicClock(std::time::Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(std::time::Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Explicit image allowlist; never inferred from a display name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// WebP.
    WebP,
}
/// Immutable upload purpose; ordinary files remain inert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobKind {
    /// No parsing or extraction.
    File,
    /// Allowed MIME and fixed signature required before commit; providers decode.
    Image(ImageFormat),
}
/// Bounded display metadata and optional upload integrity assertions.
#[derive(Clone, Debug)]
pub struct UploadMetadata {
    /// At most 255 bytes, no separators, controls, URLs, or path traversal.
    pub display_name: String,
    /// At most 128 ASCII bytes; image purpose requires its exact MIME type.
    pub content_type: String,
    /// Purpose fixed at upload admission.
    pub kind: BlobKind,
    /// Reserved before creation; commit must match exactly when supplied.
    pub declared_bytes: Option<u64>,
    /// Commit must match when supplied.
    pub expected_sha256: Option<[u8; 32]>,
}
/// Immutable integrity and signature metadata, not an authority token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Display-only name.
    pub display_name: String,
    /// Display-only MIME; a file with an image MIME remains an ordinary file.
    pub content_type: String,
    /// Exact encoded length.
    pub bytes: u64,
    /// Incrementally computed digest.
    pub sha256: [u8; 32],
    /// Format whose fixed signature matched; not a claim of decodability.
    pub image: Option<ImageFormat>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Identity {
    instance: Uuid,
    token: Uuid,
}
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity(..)")
    }
}
#[derive(Debug)]
struct Signal {
    revoked: std::sync::atomic::AtomicBool,
    wake: Arc<tokio::sync::Notify>,
}
impl Signal {
    fn revoke(&self) {
        self.revoked
            .store(true, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
    }
}
impl Drop for Signal {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}
/// Dropping the last owner requests service-owned cleanup. Close joins cleanup.
#[derive(Clone, Debug)]
pub struct InvocationOwner {
    id: Identity,
    scope: BlobScope,
    signal: Arc<Signal>,
}
/// Quarantined upload authority; dropping the last handle requests cleanup.
#[derive(Clone, Debug)]
pub struct Upload {
    id: Identity,
    signal: Arc<Signal>,
}
/// Store-issued immutable object authority; not serializable or caller-constructible.
#[derive(Clone, Debug)]
pub struct CommittedBlob {
    id: Identity,
    metadata: BlobMetadata,
}
impl CommittedBlob {
    /// Immutable object information; possession still requires authentication.
    pub fn metadata(&self) -> &BlobMetadata {
        &self.metadata
    }
}
/// Principal-scoped output ownership transfer with a fixed expiry.
#[derive(Clone, Debug)]
pub struct OutputLease {
    id: Identity,
    metadata: BlobMetadata,
    expires_at_ms: u64,
}
impl OutputLease {
    /// Immutable output information.
    pub fn metadata(&self) -> &BlobMetadata {
        &self.metadata
    }
    /// Absolute process-clock expiry, anchored at transfer.
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
    /// Renewal is always denied.
    pub fn renew(&self) -> Result<(), BlobError> {
        Err(BlobError::Denied)
    }
}
/// Revocable cursor. Expiry is checked at each chunk boundary.
#[derive(Clone, Debug)]
pub struct BlobReader {
    id: Identity,
    signal: Arc<Signal>,
}
impl BlobReader {
    /// Revoke all clones and release reader credit through service-owned cleanup.
    pub fn close(&self) {
        self.signal.revoke();
    }
}
/// Host-authenticated identity for every reader operation.
#[derive(Clone, Debug)]
pub enum ReadAccess {
    /// A still-open invocation owns the committed object.
    Invocation(BlobScope),
    /// A principal owns a live output lease.
    Output {
        /// Authenticated principal, never supplied by untrusted guest data.
        principal: String,
    },
}
/// One bounded committed-object read.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadChunk {
    /// At most 64 KiB, never partial on failure.
    pub bytes: Vec<u8>,
    /// Cursor reached the exact object length.
    pub eof: bool,
}
/// Process-instance accounting snapshot, including quarantine and reservations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Reserved bytes, including uploads, leases and failed deletion quarantine.
    pub bytes: u64,
    /// All owned objects, including failed deletions.
    pub objects: usize,
    /// Live reader cursors.
    pub readers: usize,
    /// Objects pending or failing deletion, including queued work.
    pub pending_deletions: usize,
    /// Queued or active deletion jobs, not OS-thread count.
    pub deleting: usize,
}
/// Consumer-owned, object-safe boundary. IDs never replace host authentication.
pub trait BlobStore: Send + Sync {
    /// Issue one owner per authenticated invocation.
    fn open_invocation(&self, scope: BlobScope, wait: Wait) -> BlobFuture<InvocationOwner>;
    /// Reserve declared bytes and create a quarantined object.
    fn begin(
        &self,
        scope: BlobScope,
        owner: InvocationOwner,
        metadata: UploadMetadata,
        wait: Wait,
    ) -> BlobFuture<Upload>;
    /// Append one ordered chunk; do not retry automatically after any failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "scope, sequence, and owned payload remain separate"
    )]
    fn append(
        &self,
        scope: BlobScope,
        upload: Upload,
        sequence: u64,
        bytes: Vec<u8>,
        wait: Wait,
    ) -> BlobFuture<()>;
    /// Verify integrity and optional fixed image signature, then freeze the object.
    fn commit(&self, scope: BlobScope, upload: Upload, wait: Wait) -> BlobFuture<CommittedBlob>;
    /// Open a cursor on an invocation-owned committed object.
    fn reader(&self, scope: BlobScope, blob: CommittedBlob, wait: Wait) -> BlobFuture<BlobReader>;
    /// Atomically replace invocation consumption with fixed output-lease ownership.
    fn transfer_output(
        &self,
        scope: BlobScope,
        blob: CommittedBlob,
        wait: Wait,
    ) -> BlobFuture<OutputLease>;
    /// Open a principal-scoped cursor without extending expiry.
    fn output_reader(
        &self,
        principal: String,
        lease: OutputLease,
        wait: Wait,
    ) -> BlobFuture<BlobReader>;
    /// Read one chunk, rechecking ownership, expiry, cancellation and deadline.
    fn read(&self, access: ReadAccess, reader: BlobReader, wait: Wait) -> BlobFuture<ReadChunk>;
    /// Revoke untransferred objects and wait for verified cleanup.
    fn close_invocation(
        &self,
        scope: BlobScope,
        owner: InvocationOwner,
        wait: Wait,
    ) -> BlobFuture<()>;
    /// Inspect retained accounting without returning paths or content.
    fn usage(&self, wait: Wait) -> BlobFuture<Usage>;
    /// Retry quarantined deletions and wait; failure never releases accounting.
    fn retry_cleanup(&self, wait: Wait) -> BlobFuture<()>;
    /// Starts service-owned shutdown even if the returned waiter is dropped.
    fn shutdown(&self) -> BlobFuture<()>;
}
