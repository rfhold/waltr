//! Durable agent session support.

mod compat;
mod compression;
mod coordinator;
mod run;
mod runtime;
mod runtime_factory;
mod settings;
mod storage;
mod title;
mod types;
mod watch;

pub use compat::EphemeralSessionRun;
pub use coordinator::SessionCoordinator;
pub use run::{
    AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntimeError, RunConflictPolicy,
};
pub(super) use run::{RunOutcomeSender, RunResult};
pub use runtime::{AgentSessionRuntime, RuntimeResumeOutput};
pub use runtime_factory::AgentSessionRuntimeFactory;
pub use settings::{
    FileCredentialStore, OAuthCredential, SessionMcpServerRuntimeKind,
    SessionMcpServerRuntimeSettings, SessionOAuthProviderSettings, SessionOAuthTokenField,
    SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle, SessionProviderRuntimeSettings,
    SessionRuntimeSettings, StoredCredential,
};
pub use storage::{
    FilesystemSessionStore, MemorySessionStore, OpenDalSessionStore, S3SessionStore,
    ScyllaSessionStore, SessionStore, SessionStoreError, SessionStoreResult,
    opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use types::{
    AgentSession, CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, PendingWork, PendingWorkStatus, RequestId,
    RequestRecord, ResumeInput, Revision, SessionId, SessionJournalEvent, SessionMetrics,
    SessionRecord, SessionRollbackTarget, SessionSnapshot, SessionStatus, SessionSummary,
    Suspension,
};
pub(super) use watch::{SessionListBroadcaster, SessionTimelineBroadcaster};
#[allow(unused_imports)]
pub use watch::{
    SessionListEvent, SessionListSubscription, SessionListUpdate, SessionTimelineEvent,
    SessionTimelineSubscription, SessionTimelineUpdate,
};
