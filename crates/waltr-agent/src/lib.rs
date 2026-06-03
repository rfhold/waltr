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
    Agent, AgentBuilder, AgentEvent, AgentRunBuilder, AgentState, AgentTool, AgentToolContext,
    AgentTranscriptEvent, BoxAgentEventStream,
};
pub use error::{LlmError, Result};
pub use model::{ModelApi, ModelId, ModelRef, ProviderId};
pub use permission::{
    PermissionCheckContext, PermissionClaim, PermissionClassRule, PermissionCommandPattern,
    PermissionCommandRule, PermissionDecision, PermissionDirectoryRule, PermissionEffect,
    PermissionEngine, PermissionHandler, PermissionLevel, PermissionOperation,
    PermissionPathPattern, PermissionPattern, PermissionPolicy, PermissionRequest,
    PermissionResource, PermissionResourcePattern, PermissionToolRule, PermissionToolSelector,
    SharedPermissionHandler,
};
#[cfg(feature = "shell")]
pub use prompt::ShellPromptConfig;
pub use prompt::{
    AGENTS_INSTRUCTION_FILE, AgentsInstruction, AgentsPromptConfig, BASE_SYSTEM_PROMPT,
    FilesystemPromptConfig, SystemPromptBuilder,
};
pub use stream::{
    BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
    CompletionRequestBuilder, LlmStream, ThinkingBlock, ThinkingBlockKind, ThinkingDelta, ToolCall,
    ToolDefinition, ToolResult,
};
pub use tools::filesystem;
pub use tools::question;
pub use tools::skills::{SkillDefinition, SkillsConfig};

#[cfg(feature = "shell")]
pub use tools::shell;
