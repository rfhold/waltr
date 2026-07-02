//! Agent library for Waltr.

mod agent;
mod error;
pub mod mcp;
pub mod middleware;
mod model;
mod permission;
pub mod prompt;
pub mod provider;
mod stream;
pub mod timeline;
pub mod tools;

pub use agent::{
    AgentRunBuilder, AgentRuntime, AgentRuntimeBuilder, AgentSession, AgentSessionActiveRunState,
    AgentSessionRunDisposition, AgentSessionRunError, AgentSessionRunHandle, AgentSessionRunInput,
    AgentSessionRunOutcome, AgentSessionRunSubmission, AgentSessionRuntime,
    AgentSessionRuntimeError, AgentSessionRuntimeFactory, AgentTool, BoxAgentTimelineStream,
    CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestKind, ExternalRequestStatus,
    ExternalResponse, FileCredentialStore, FilesystemSessionStore, MemorySessionStore,
    OAuthCredential, OpenDalSessionStore, PendingWork, PendingWorkStatus, ProviderRetryConfig,
    RequestId, RequestRecord, ResumeInput, Revision, RunConflictPolicy, RuntimeResumeOutput,
    S3SessionStore, ScyllaSessionStore, SessionCoordinator, SessionId, SessionJournalEvent,
    SessionListEvent, SessionListSubscription, SessionListUpdate, SessionMcpServerRuntimeKind,
    SessionMcpServerRuntimeSettings, SessionMetrics, SessionOAuthProviderSettings,
    SessionOAuthTokenField, SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle,
    SessionProviderRuntimeSettings, SessionRecord, SessionRuntimeSettings, SessionSnapshot,
    SessionStatus, SessionStore, SessionStoreError, SessionStoreResult, SessionSummary,
    SessionTimelineEvent, SessionTimelineSubscription, SessionTimelineUpdate, StoredCredential,
    Suspension, ToolExecutionContext, ToolProgressSink, opendal_postgresql_session_store,
    run_postgresql_migrations,
};
pub use error::{LlmError, Result};
pub use model::{ModelApi, ModelId, ModelRef, ProviderId};
pub use permission::{
    PermissionClaim, PermissionClassRule, PermissionCommandPattern, PermissionCommandRule,
    PermissionDenyMode, PermissionDirectoryRule, PermissionEffect, PermissionEngine,
    PermissionLevel, PermissionOperation, PermissionPathPattern, PermissionPattern,
    PermissionPolicy, PermissionResource, PermissionResourcePattern, PermissionResponse,
    PermissionToolRule, PermissionToolSelector,
};
#[cfg(feature = "shell")]
pub use prompt::ShellPromptConfig;
pub use prompt::{
    AGENTS_INSTRUCTION_FILE, AgentsInstruction, AgentsPromptConfig, BASE_SYSTEM_PROMPT,
    FilesystemPromptConfig, SystemPromptBuilder,
};
pub use stream::{
    BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionReplayItem, CompletionRequest,
    CompletionRequestBuilder, CompletionTurn, LlmStream, ModelCost, PromptTurn,
    ProviderTurnMetadata, ThinkingBlock, ThinkingBlockKind, ThinkingDelta, TokenCost, TokenUsage,
    ToolCall, ToolDefinition, ToolResult, TurnItem,
};
pub use timeline::{
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CHANNEL_METRICS,
    CHANNEL_THINKING, CHANNEL_TOOL_CALL, CHANNEL_TOOL_PROGRESS_ERROR, CHANNEL_TOOL_PROGRESS_INFO,
    CHANNEL_TOOL_RESULT, CHANNEL_USER_TEXT, PendingTimelineEvent, TimelineChannel, TimelineContent,
    TimelineContentId, TimelineCursor, TimelineDirection, TimelineEventKind, TimelineItemId,
    TimelineItemText, TimelineQuery, TimelineStatus,
};
pub use tools::filesystem;
pub use tools::question;
pub use tools::skills::{SkillDefinition, SkillsConfig};
pub use tools::task;

#[cfg(feature = "shell")]
pub use tools::shell;
