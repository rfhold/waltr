//! Durable agent session support.

mod capture;
mod compat;
mod compression;
mod control;
mod coordinator;
mod copy;
mod create;
mod list;
mod mask;
mod materialize;
mod projection;
mod run;
mod runtime;
mod runtime_factory;
mod settings;
mod storage;
mod types;
mod watch;

pub use capture::{CompletedSessionCapture, SessionCaptureError, SessionCaptureLimits};
pub use compat::EphemeralSessionRun;
#[allow(clippy::redundant_pub_crate)]
pub(crate) use control::{SessionControl, SessionOperation, SessionRunContext};
pub use coordinator::SessionCoordinator;
pub use copy::{SessionCopy, SessionCopyError};
pub use create::{
    SessionCreateRequest, SessionStart, SessionStartMessage, SessionStartMessageError,
};
#[allow(clippy::redundant_pub_crate)]
pub(crate) use list::{SessionIncarnation, SessionListBroadcaster, SessionPublicationClaim};
#[allow(unused_imports)]
pub use list::{SessionListEvent, SessionListSubscription, SessionListUpdate};
pub use mask::{
    ActiveSessionMask, MAX_ACTIVE_SESSION_MASKS, MAX_SESSION_MASK_KEY_BYTES,
    MAX_SESSION_MASK_MUTATION_ITEMS, MAX_SESSION_MASK_REPLACEMENT_BYTES, ReplacementUserContent,
    SessionMask, SessionMaskAddition, SessionMaskError, SessionMaskId, SessionMaskMutation,
    SessionMaskMutationOutcome, SessionMaskSubtraction, ThroughCursor,
};
pub use materialize::{SessionContextMode, SessionMaterializedContext};
pub use projection::{
    SessionSyntheticTimelineEvent, SessionTimelineMode, SessionTimelinePage,
    SessionTimelinePresentation, SessionTimelineProjectedEvent, SessionTimelineProjectionEvent,
};
pub use run::{
    AgentRunLimits, AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntimeError, MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES,
    MAX_SESSION_COORDINATION_REASON_CODE_BYTES, RunConflictPolicy,
    SessionCompletionCoordinationFuture, SessionCompletionCoordinationHandler,
    SessionCompletionCoordinationPreparation, SessionCoordinationPhase, SessionCoordinationReason,
    SessionCoordinationReasonError, SessionCoordinationState, SessionPauseError, SessionPauseId,
    SessionPauseLease,
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
    SessionJournalEvent, SessionMetrics, SessionRecord, SessionRollbackTarget, SessionSnapshot,
    SessionStatus, SessionSummary, Suspension,
};
pub(super) use watch::SessionTimelineBroadcaster;
pub use watch::{
    SessionTimelineEvent, SessionTimelineProjectionSubscription, SessionTimelineProjectionUpdate,
    SessionTimelineProjectionUpdateEvent, SessionTimelineSubscription, SessionTimelineUpdate,
};
