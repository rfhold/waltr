//! Agent library for Waltr.

mod agent;
mod auth;
mod error;
pub mod middleware;
mod model;
mod outbound_http;
pub mod prompt;
pub mod provider;
mod resource;
mod stream;
pub mod timeline;
mod transient_input;

pub use agent::{
    ActiveSessionMask, AgentHostContext, AgentHostRuntimeFactory, AgentRunBuilder, AgentRunLimits,
    AgentRuntime, AgentRuntimeBuilder, AgentRuntimeProvider, AgentSession,
    AgentSessionActiveRunState, AgentSessionRunCancellation, AgentSessionRunDisposition,
    AgentSessionRunError, AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome,
    AgentSessionRunSubmission, AgentSessionRuntime, AgentSessionRuntimeError,
    AgentSessionRuntimeFactory, AgentTool, BoxAgentTimelineStream,
    CancelReason, CompletedSessionCapture, ContextIdentity,
    CredentialRefreshCoordinator, CredentialRefreshError, CredentialStore, FileCredentialStore,
    FilesystemSessionStore, HostConfigurationUpdate, HostContext, HostContextFuture,
    HostContextState, HostDispatchBinding, HostEffectExecutor, HostEffectFuture, HostEffectResult,
    HostPromptFuture, HostPromptRender, HostRequest, HostRuntimeFuture, HostRuntimeRetireFuture,
    HostSearchFuture, IdleHostEffectError, InferenceProviderSettings, InferenceSettings,
    MAX_ACTIVE_SESSION_MASKS,
    MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES, MAX_SESSION_COORDINATION_REASON_CODE_BYTES,
    MAX_SESSION_MASK_KEY_BYTES, MAX_SESSION_MASK_MUTATION_ITEMS,
    MAX_SESSION_MASK_REPLACEMENT_BYTES, MemorySessionStore, MetadataSetCondition,
    MetadataSetOutcome, OAUTH_CREDENTIAL_VERSION, OAuthCredential, OAuthCredentialContext,
    OAuthCredentialRecord, OAuthCredentialStore, OAuthCredentialStoreFuture, OAuthRefreshClaim,
    OAuthRefreshClaimResult, OAuthTokenSet, OpenDalSessionStore, PendingWork, PendingWorkStatus,
    ProviderRetryConfig, RefreshMode, ReplacementUserContent, RequestId, RequestRecord,
    ResumeInput, Revision, RunConflictPolicy, RuntimeResumeOutput, S3SessionStore,
    ExecutionSequence, ExecutionToken, SessionCommitReceipt, TimelineAppendReceipt,
    S3SessionWriteMode, SandboxDispatch, ScyllaSessionStore, SemanticSessionEventSink,
    SessionCapabilities, SessionCaptureError, SessionCaptureLimits,
    SessionCompletionAdmissionFinalizer, SessionCompletionAdmissionHandler,
    SessionCompletionCoordinationFuture, SessionCompletionCoordinationHandler,
    SessionCompletionCoordinationPreparation, SessionCompletionFinalizationFuture,
    SessionCompletionPreparation, SessionCompletionPreparationFuture,
    SessionCompletionWorkerFuture, SessionContextCapture, SessionContextMode,
    SessionCoordinationPhase, SessionCoordinationReason, SessionCoordinationReasonError,
    SessionCoordinationState, SessionCoordinator, SessionCopy, SessionCopyError,
    SessionCreateRequest, SessionEvent, SessionEventSink, SessionEventSinkFuture, SessionId,
    SessionJournalEvent, SessionListEvent, SessionListSubscription, SessionListUpdate, SessionMask,
    SessionMaskAddition, SessionMaskError, SessionMaskId, SessionMaskMutation,
    SessionMaskMutationOutcome, SessionMaskSubtraction, SessionMaterializedContext, SessionMetrics,
    SessionOAuthJwtMetadata, SessionOAuthJwtToken, SessionOAuthProviderSettings,
    SessionOAuthTokenField, SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle,
    SessionPauseError, SessionPauseId, SessionPauseLease, SessionProgress, SessionProgressPhase,
    SessionPromptDisposition, SessionRecord, SessionRunCompletionSnapshot, SessionRunStage,
    SessionSemanticEvent, SessionSemanticHub, SessionSemanticKind, SessionSemanticSubscription,
    SessionSnapshot, SessionStart, SessionStartMessage, SessionStartMessageError, SessionStatus,
    SessionStore, SessionStoreError, SessionStoreResult, SessionSummary,
    SessionSyntheticTimelineEvent, SessionTimelineEvent, SessionTimelineMode, SessionTimelinePage,
    SessionTimelinePresentation, SessionTimelineProjectedEvent, SessionTimelineProjectionEvent,
    SessionTimelineProjectionSubscription, SessionTimelineProjectionUpdate,
    SessionTimelineProjectionUpdateEvent, SessionTimelineSubscription, SessionTimelineUpdate,
    StoredCredential, Suspension, ThroughCursor, ToolExecutionContext,
    ToolFuture, ToolGuardFuture, ToolProgressSink, ToolResultMetadata,
    completion::{
        CompletionDecision, CompletionFeedback, CompletionObserver, CompletionObserverError,
        CompletionObserverFuture, CompletionProposal, CompletionToolEvidence, CompletionValidation,
        MAX_COMPLETION_DURATION, MAX_COMPLETION_EVIDENCE_BYTES, MAX_COMPLETION_FEEDBACK_BYTES,
        MAX_COMPLETION_TURNS,
    },
    ItemMetadata, MetadataError, MetadataTarget, MetadataPredicate, MetadataUpdate,
    SessionMutation, SessionMutationOutcome, MAX_METADATA_KEY_BYTES, MAX_METADATA_VALUE_BYTES,
    MAX_METADATA_KEYS, MAX_METADATA_ITEMS, MAX_METADATA_MUTATION_ENTRIES, MAX_METADATA_BYTES,
    MAX_SESSION_MUTATION_BYTES,
    opendal_postgresql_session_store,
    owned_runs::{OwnedExecutionError, SessionExecutionOwner},
    run_postgresql_migrations,
};
pub use auth::{AuthTarget, AuthenticationFailure, AuthenticationFailureReason};
pub use error::{LlmError, Result};
pub use model::{ModelApi, ModelId, ModelRef, ProviderId};
pub use prompt::SystemPromptBuilder;
pub use resource::{
    SessionTemplateKind, SessionTemplateOption, SessionTemplateSearchRequest,
    SessionTemplateSearchTarget,
};
pub use stream::{
    BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionReplayItem, CompletionRequest,
    CompletionRequestBuilder, CompletionTurn, LlmStream, MAX_TOOL_RESULT_IMAGE_BYTES,
    MAX_TOOL_RESULT_IMAGES, ModelCost, PromptTurn, ProviderTurnMetadata, ThinkingBlock,
    ThinkingBlockKind, ThinkingDelta, TokenCost, TokenUsage, ToolCall, ToolDefinition, ToolResult,
    ToolResultContent, ToolResultContentBlock, ToolResultContentError, ToolResultImage, TurnItem,
};
pub use timeline::{
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_METRICS, CHANNEL_THINKING,
    CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_ERROR, CHANNEL_TOOL_PROGRESS_INFO,
    CHANNEL_TOOL_RESULT, CHANNEL_USER_TEXT, PendingTimelineEvent, PendingTimelineItemId,
    TimelineChannel, TimelineContent, TimelineContentId, TimelineCursor, TimelineDirection,
    TimelineEventKind, TimelineItemId, TimelineItemText, TimelineQuery, TimelineStatus,
};
pub use transient_input::{
    AdmittedTransientInput, MAX_TRANSIENT_ATTACHMENT_BYTES, MAX_TRANSIENT_BODY_BYTES,
    MAX_TRANSIENT_SOURCE_BYTES, TransientAttachment, TransientInput, TransientInputApi,
    TransientInputBudget, TransientInputKind, TransientModelCapabilities, TransientPromptTarget,
};
