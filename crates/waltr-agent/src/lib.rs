//! Agent library for Waltr.

mod agent;
mod auth;
pub mod definition;
mod error;
pub mod mcp;
pub mod middleware;
mod model;
mod permission;
pub mod prompt;
pub mod provider;
mod resource;
mod stream;
mod template;
pub mod timeline;
pub mod tools;

pub use agent::{
    AgentRunBuilder, AgentRuntime, AgentRuntimeBuilder, AgentRuntimeProvider, AgentSession,
    AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntime, AgentSessionRuntimeError, AgentSessionRuntimeFactory, AgentTool,
    BoxAgentTimelineStream, CancelReason, CredentialRefreshCoordinator, CredentialRefreshError,
    CredentialStore, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, FileCredentialStore, FilesystemSessionStore,
    InferenceProviderSettings, InferenceSettings, McpConnectionKind, McpConnectionResolveFuture,
    McpConnectionResolveRequest, McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration,
    McpOAuthRegistrationMethod, McpResolvedConnection, MemorySessionStore,
    OAUTH_CREDENTIAL_VERSION, OAuthCredential, OAuthCredentialContext, OAuthCredentialRecord,
    OAuthCredentialStore, OAuthCredentialStoreFuture, OAuthRefreshClaim, OAuthRefreshClaimResult,
    OAuthTokenSet, OpenDalSessionStore, PendingWork, PendingWorkStatus, ProviderRetryConfig,
    RefreshMode, RequestId, RequestRecord, ResumeInput, Revision, RunConflictPolicy,
    RuntimeResumeOutput, S3SessionStore, S3SessionWriteMode, ScyllaSessionStore,
    SessionCoordinator, SessionId, SessionJournalEvent, SessionListEvent, SessionListSubscription,
    SessionListUpdate, SessionMetrics, SessionOAuthJwtMetadata, SessionOAuthJwtToken,
    SessionOAuthProviderSettings, SessionOAuthTokenField, SessionOAuthTokenFieldTarget,
    SessionOAuthTokenRequestStyle, SessionRecord, SessionSnapshot, SessionStatus, SessionStore,
    SessionStoreError, SessionStoreResult, SessionSummary, SessionTimelineEvent,
    SessionTimelineSubscription, SessionTimelineUpdate, StoredCredential, Suspension,
    ToolExecutionContext, ToolProgressSink, opendal_postgresql_session_store,
    run_postgresql_migrations,
};
pub use auth::{
    AuthTarget, AuthenticationFailure, AuthenticationFailureReason, McpAuthorizationContext,
};
pub use definition::{
    AGENT_DEFINITION_API_VERSION, AgentCatalogEntry, AgentDefinitionBundle, AgentDefinitionCatalog,
    AgentDefinitionError, AgentDefinitionRegistry, AgentDefinitionResourceKind, AgentFeature,
    AgentPermissionSelector, AgentSwitchAudit, AgentsFeatureConfig, FilesystemFeatureConfig,
    FilesystemFeatureProfile, McpFeatureConfig, ResolvedAgent, ResolvedAgentFeatures,
    ResolvedPermission, ResolvedPermissionEntry, ResolvedPermissionResources,
    SESSION_WORKSPACE_ROOT, ShellFeatureConfig, SkillsFeatureConfig, TaskFeatureConfig,
};
pub use error::{LlmError, Result};
pub use model::{ModelApi, ModelId, ModelRef, ProviderId};
pub use permission::{
    PermissionClaim, PermissionClassRule, PermissionCommandPattern, PermissionCommandRule,
    PermissionDenyMode, PermissionDirectoryRule, PermissionEffect, PermissionEngine,
    PermissionInputCondition, PermissionInputRule, PermissionInputScalar,
    PermissionInputValueMatcher, PermissionJsonPointer, PermissionLevel, PermissionOperation,
    PermissionPathPattern, PermissionPattern, PermissionPolicy, PermissionResource,
    PermissionResourcePattern, PermissionResponse, PermissionToolRule, PermissionToolSelector,
};
#[cfg(feature = "shell")]
pub use prompt::ShellPromptConfig;
pub use prompt::{
    AGENTS_INSTRUCTION_FILE, AgentsInstruction, AgentsPromptConfig, BASE_SYSTEM_PROMPT,
    FilesystemPromptConfig, SystemPromptBuilder,
};
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
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CHANNEL_METRICS,
    CHANNEL_THINKING, CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_ERROR, CHANNEL_TOOL_PROGRESS_INFO,
    CHANNEL_TOOL_RESULT, CHANNEL_USER_TEXT, PendingTimelineEvent, PendingTimelineItemId,
    TimelineChannel, TimelineContent, TimelineContentId, TimelineCursor, TimelineDirection,
    TimelineEventKind, TimelineItemId, TimelineItemText, TimelineQuery, TimelineStatus,
};
pub use tools::filesystem;
pub use tools::question;
pub use tools::skills::{SkillDefinition, SkillsConfig};
pub use tools::task;

#[cfg(feature = "shell")]
pub use tools::shell;
