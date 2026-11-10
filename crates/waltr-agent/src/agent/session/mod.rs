//! Durable agent session support.

mod capture;
mod compat;
mod context;
mod control;
mod coordinator;
mod copy;
mod create;
mod list;
mod mask;
mod materialize;
mod metadata;
mod progress;
mod revision;
mod projection;
mod run;
mod runtime;
mod runtime_factory;
mod settings;
mod storage;
mod types;
mod watch;

pub use capture::{CompletedSessionCapture, SessionCaptureError, SessionCaptureLimits};
pub use metadata::{ItemMetadata, MetadataError, MetadataTarget, MetadataPredicate, MetadataUpdate,
    SessionMutation, SessionMutationOutcome, MAX_METADATA_KEY_BYTES, MAX_METADATA_VALUE_BYTES,
    MAX_METADATA_KEYS, MAX_METADATA_ITEMS, MAX_METADATA_MUTATION_ENTRIES, MAX_METADATA_BYTES,
    MAX_SESSION_MUTATION_BYTES};
pub use compat::EphemeralSessionRun;
pub use context::{ContextIdentity, SessionContextCapture};
#[allow(clippy::redundant_pub_crate)]
pub(crate) use control::{SessionControl, SessionOperation, SessionRunContext};
pub use coordinator::{IdleHostEffectError, SessionCoordinator};
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
pub use progress::{
    SessionCapabilities, SessionProgress, SessionProgressPhase, SessionPromptDisposition,
    SessionRunStage,
};
pub use revision::{ExecutionSequence, ExecutionToken, SessionCommitReceipt, TimelineAppendReceipt};
pub use projection::{
    SessionSyntheticTimelineEvent, SessionTimelineMode, SessionTimelinePage,
    SessionTimelinePresentation, SessionTimelineProjectedEvent, SessionTimelineProjectionEvent,
};
pub use run::{
    AgentRunLimits, AgentSessionActiveRunState, AgentSessionRunCancellation,
    AgentSessionRunDisposition, AgentSessionRunError, AgentSessionRunHandle, AgentSessionRunInput,
    AgentSessionRunOutcome, AgentSessionRunSubmission, AgentSessionRuntimeError,
    MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES, MAX_SESSION_COORDINATION_REASON_CODE_BYTES,
    RunConflictPolicy, SessionCompletionCoordinationFuture, SessionCompletionCoordinationHandler,
    SessionCompletionCoordinationPreparation, SessionCoordinationPhase, SessionCoordinationReason,
    SessionCoordinationReasonError, SessionCoordinationState, SessionPauseError, SessionPauseId,
    SessionPauseLease,
};
pub(super) use run::{RunOutcomeSender, RunResult};
pub use runtime::{AgentSessionRuntime, RuntimeResumeOutput};
pub use runtime_factory::{
    AgentRuntimeProvider, AgentSessionRuntimeFactory, CredentialRefreshCoordinator,
    CredentialRefreshError, RefreshMode,
};
pub use settings::{
    CredentialStore, FileCredentialStore, InferenceProviderSettings, InferenceSettings,
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
    AgentSession, CancelReason, MetadataSetCondition, MetadataSetOutcome, PendingWork,
    PendingWorkStatus, RequestId, RequestRecord, ResumeInput, Revision, SandboxDispatch, SessionId,
    SessionJournalEvent, SessionMetrics, SessionRecord, SessionRollbackTarget, SessionSnapshot,
    SessionStatus, SessionSummary, Suspension,
};
pub(super) use watch::SessionTimelineBroadcaster;
pub use watch::{
    SessionTimelineEvent, SessionTimelineProjectionSubscription, SessionTimelineProjectionUpdate,
    SessionTimelineProjectionUpdateEvent, SessionTimelineSubscription, SessionTimelineUpdate,
};

#[cfg(test)]
fn opaque_test_configuration(name: &str, sources: Vec<String>) -> serde_json::Value {
    serde_json::json!({"name": name, "sources": sources})
}
