//! Durable agent session support.

mod compat;
mod compression;
mod factory;
mod runtime;
mod storage;
mod title;
mod types;
mod watch;

pub use compat::EphemeralSessionRun;
pub use factory::AgentSessions;
pub use runtime::{AgentSessionRuntime, RuntimeResumeOutput};
pub use storage::{
    FilesystemSessionStore, MemorySessionStore, OpenDalSessionStore, S3SessionStore,
    ScyllaSessionStore, SessionStore, SessionStoreError, SessionStoreResult,
    opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use types::{
    AgentSession, CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, PendingWork, PendingWorkStatus, RequestId,
    RequestRecord, ResumeInput, Revision, SessionId, SessionJournalEvent, SessionRecord,
    SessionSnapshot, SessionStatus, SessionSummary, Suspension,
};
pub(crate) use watch::{SessionListBroadcaster, SessionTimelineBroadcaster};
pub use watch::{
    SessionListEvent, SessionListSubscription, SessionListUpdate, SessionTimelineSubscription,
    SessionTimelineUpdate,
};
