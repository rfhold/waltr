//! Durable agent session support.

mod compat;
mod compression;
mod control;
mod copy;
mod coordinator;
mod create;
mod list;
mod run;
mod runtime;
mod runtime_factory;
mod settings;
mod storage;
mod title;
mod types;
mod watch;

pub use compat::EphemeralSessionRun;
#[allow(clippy::redundant_pub_crate)]
pub(crate) use control::{SessionControl, SessionOperation, SessionRunContext};
pub use copy::{SessionCopy, SessionCopyError};
pub use coordinator::SessionCoordinator;
pub use create::{
    SessionCreateRequest, SessionStart, SessionStartMessage, SessionStartMessageError,
};
#[allow(clippy::redundant_pub_crate)]
pub(crate) use list::{SessionIncarnation, SessionListBroadcaster, SessionPublicationClaim};
#[allow(unused_imports)]
pub use list::{SessionListEvent, SessionListSubscription, SessionListUpdate};
pub use run::{
    AgentRunLimits, AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntimeError, RunConflictPolicy,
};
pub(super) use run::{RunOutcomeSender, RunResult};
pub use runtime::{AgentSessionRuntime, RuntimeResumeOutput};
pub use runtime_factory::{
    AgentRuntimeProvider, AgentSessionRuntimeFactory, CredentialRefreshCoordinator,
    CredentialRefreshError, McpConnectionKind, McpConnectionResolveFuture,
    McpConnectionResolveRequest, McpResolvedConnection, RefreshMode,
};
pub use settings::{
    CredentialStore, FileCredentialStore, InferenceProviderSettings, InferenceSettings,
    McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration, McpOAuthRegistrationMethod,
    OAUTH_CREDENTIAL_VERSION, OAuthCredential, OAuthCredentialContext, OAuthCredentialRecord,
    OAuthCredentialStore, OAuthCredentialStoreFuture, OAuthRefreshClaim, OAuthRefreshClaimResult,
    OAuthTokenSet, SessionOAuthJwtMetadata, SessionOAuthJwtToken, SessionOAuthProviderSettings,
    SessionOAuthTokenField, SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle,
    StoredCredential,
};
pub use storage::{
    FilesystemSessionStore, MemorySessionStore, OpenDalSessionStore, S3SessionStore,
    S3SessionWriteMode, ScyllaSessionStore, SessionStore, SessionStoreError, SessionStoreResult,
    opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use types::{
    AgentSession, CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, MetadataSetCondition, MetadataSetOutcome, PendingWork,
    PendingWorkStatus, RequestId, RequestRecord, ResumeInput, Revision, SessionId,
    SessionJournalEvent, SessionMetrics, SessionRecord,
    SessionRollbackTarget, SessionSnapshot, SessionStatus, SessionSummary, Suspension,
};
pub(super) use watch::SessionTimelineBroadcaster;
pub use watch::{SessionTimelineEvent, SessionTimelineSubscription, SessionTimelineUpdate};
