//! Agent library for Waltr.

mod agent;
mod error;
pub mod mcp;
pub mod middleware;
mod model;
pub mod models;
mod permission;
pub mod prompt;
pub mod provider;
mod stream;
pub mod tools;

pub use agent::{
    Agent, AgentBuilder, AgentEvent, AgentRunBuilder, AgentSession, AgentSessionRuntime,
    AgentSessions, AgentState, AgentTool, AgentToolContext, AgentTranscriptEvent,
    BoxAgentEventStream, CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, FilesystemSessionStore, MemorySessionStore,
    OpenDalSessionStore, PendingWork, PendingWorkStatus, RequestId, RequestRecord, ResumeInput,
    Revision, RuntimeResumeOutput, S3SessionStore, ScyllaSessionStore, SessionEvent, SessionId,
    SessionRecord, SessionSnapshot, SessionStatus, SessionStore, SessionStoreError,
    SessionStoreResult, Suspension, opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use error::{LlmError, Result};
pub use model::{ModelApi, ModelId, ModelRef, ProviderId};
pub use permission::{
    PermissionClaim, PermissionClassRule, PermissionCommandPattern, PermissionCommandRule,
    PermissionDirectoryRule, PermissionEffect, PermissionEngine, PermissionLevel,
    PermissionOperation, PermissionPathPattern, PermissionPattern, PermissionPolicy,
    PermissionResource, PermissionResourcePattern, PermissionToolRule, PermissionToolSelector,
};
#[cfg(feature = "shell")]
pub use prompt::ShellPromptConfig;
pub use prompt::{
    AGENTS_INSTRUCTION_FILE, AgentsInstruction, AgentsPromptConfig, BASE_SYSTEM_PROMPT,
    FilesystemPromptConfig, SystemPromptBuilder,
};
pub use stream::{
    BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
    CompletionRequestBuilder, CompletionTurn, LlmStream, ProviderTurnMetadata, ThinkingBlock,
    ThinkingBlockKind, ThinkingDelta, TokenUsage, ToolCall, ToolDefinition, ToolResult, TurnItem,
};
pub use tools::filesystem;
pub use tools::question;
pub use tools::skills::{SkillDefinition, SkillsConfig};

#[cfg(feature = "shell")]
pub use tools::shell;
