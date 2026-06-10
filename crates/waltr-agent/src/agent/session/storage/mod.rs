//! Agent session storage interface.

#![allow(async_fn_in_trait, missing_docs)]

mod filesystem;
mod memory;
mod opendal;
mod postgresql;
mod s3;
mod scylla;

use std::{error::Error, fmt, future::Future};

pub use filesystem::FilesystemSessionStore;
pub use memory::MemorySessionStore;
pub use opendal::OpenDalSessionStore;
pub use postgresql::{opendal_postgresql_session_store, run_postgresql_migrations};
pub use s3::S3SessionStore;
pub use scylla::ScyllaSessionStore;

use super::types::{
    ExternalRequest, RequestRecord, Revision, SessionEvent, SessionId, SessionRecord,
    SessionSnapshot,
};

/// Result type returned by session stores.
pub type SessionStoreResult<T> = Result<T, SessionStoreError>;

/// Storage failure returned by session stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStoreError {
    /// The requested session does not exist.
    NotFound { id: SessionId },
    /// A session with the requested id already exists.
    AlreadyExists { id: SessionId },
    /// The caller expected a stale revision.
    StaleRevision {
        id: SessionId,
        expected: Revision,
        actual: Revision,
    },
    /// The record being saved belongs to a different session.
    SessionIdMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    /// The in-process store lock was poisoned.
    LockPoisoned,
    /// The backend returned an implementation-specific error.
    Backend { message: String },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { id } => write!(formatter, "session not found: {}", id.as_str()),
            Self::AlreadyExists { id } => {
                write!(formatter, "session already exists: {}", id.as_str())
            }
            Self::StaleRevision {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "stale session revision for {}: expected {}, actual {}",
                id.as_str(),
                expected.get(),
                actual.get()
            ),
            Self::SessionIdMismatch { expected, actual } => write!(
                formatter,
                "session id mismatch: expected {}, actual {}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::LockPoisoned => formatter.write_str("session store lock poisoned"),
            Self::Backend { message } => {
                write!(formatter, "session store backend error: {message}")
            }
        }
    }
}

impl Error for SessionStoreError {}

/// Durable agent session storage contract.
pub trait SessionStore: Send + Sync {
    /// Create a new session record.
    fn create(&self, record: SessionRecord) -> impl Future<Output = SessionStoreResult<()>> + Send;

    /// Load a session record.
    fn load(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Append journal events if the current revision matches the caller's expectation.
    fn append(
        &self,
        id: &SessionId,
        events: Vec<SessionEvent>,
        expected_revision: Revision,
    ) -> impl Future<Output = SessionStoreResult<Revision>> + Send;

    /// Save a compact snapshot if the current revision matches the caller's expectation.
    fn save_snapshot(
        &self,
        id: &SessionId,
        snapshot: SessionSnapshot,
        expected_revision: Revision,
    ) -> impl Future<Output = SessionStoreResult<Revision>> + Send;

    /// Record a provider request before dispatch.
    fn record_request(
        &self,
        id: &SessionId,
        request: RequestRecord,
        expected_revision: Revision,
    ) -> impl Future<Output = SessionStoreResult<Revision>> + Send;

    /// Record an external request before the runtime emits a suspension.
    fn record_external_request(
        &self,
        id: &SessionId,
        request: ExternalRequest,
        expected_revision: Revision,
    ) -> impl Future<Output = SessionStoreResult<Revision>> + Send;
}
