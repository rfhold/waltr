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

use crate::timeline::{AgentTimelineEvent, PendingTimelineEvent, TimelineCursor};

use super::types::{Revision, SessionId, SessionJournalEvent, SessionRecord};

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
    /// Create a new empty session record.
    fn create(
        &self,
        id: SessionId,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Load a session record.
    fn load(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = SessionStoreResult<SessionRecord>> + Send;

    /// Append pending timeline events and return them with assigned cursors.
    fn append_timeline(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        events: Vec<PendingTimelineEvent>,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send;

    /// Append internal journal events if the revision matches.
    fn append_journal(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        events: Vec<SessionJournalEvent>,
    ) -> impl Future<Output = SessionStoreResult<Revision>> + Send;

    /// Read timeline events after `cursor`, or from the start when no cursor is supplied.
    fn timeline_after(
        &self,
        id: &SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send;

    /// Read timeline events before `cursor`, preserving persisted order.
    fn timeline_before(
        &self,
        id: &SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> impl Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send;
}

pub(super) fn append_timeline_to_record(
    record: &mut SessionRecord,
    id: &SessionId,
    events: Vec<PendingTimelineEvent>,
) -> SessionStoreResult<Vec<AgentTimelineEvent>> {
    let mut cursor = record
        .last_timeline_cursor()
        .unwrap_or(TimelineCursor::ZERO);
    let mut assigned = Vec::with_capacity(events.len());
    for event in events {
        if event.session_id != *id {
            return Err(SessionStoreError::SessionIdMismatch {
                expected: id.clone(),
                actual: event.session_id,
            });
        }
        cursor = cursor.next();
        assigned.push(event.with_cursor(cursor));
    }
    record.append_journal_events(assigned.iter().cloned().map(SessionJournalEvent::Timeline));
    Ok(assigned)
}

pub(super) fn timeline_after_slice(
    record: &SessionRecord,
    cursor: Option<TimelineCursor>,
    limit: usize,
) -> Vec<AgentTimelineEvent> {
    if limit == 0 {
        return Vec::new();
    }
    record
        .timeline_events()
        .filter(|event| cursor.map_or(true, |cursor| event.cursor > cursor))
        .take(limit)
        .cloned()
        .collect()
}

pub(super) fn timeline_before_slice(
    record: &SessionRecord,
    cursor: Option<TimelineCursor>,
    limit: usize,
) -> Vec<AgentTimelineEvent> {
    if limit == 0 {
        return Vec::new();
    }
    let events = record
        .timeline_events()
        .filter(|event| cursor.map_or(true, |cursor| event.cursor < cursor))
        .cloned()
        .collect::<Vec<_>>();
    let start = events.len().saturating_sub(limit);
    events[start..].to_vec()
}
