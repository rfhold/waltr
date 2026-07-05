//! Generic Model Context Protocol primitives, client transports, and server helpers.

mod capabilities;
/// MCP client connection and transport helpers.
pub mod client;
mod error;
mod log;
mod prompt;
/// JSON-RPC and MCP wire protocol helpers.
pub mod protocol;
mod resource;
/// MCP server handler traits and stdio/HTTP serving helpers.
pub mod server;
mod tool;
mod trace;
mod types;

pub use capabilities::{
    McpPromptsCapability, McpResourcesCapability, McpServerCapabilities, McpToolsCapability,
};
pub use error::{Error, Result};
pub use log::McpLogLevel;
pub use prompt::{
    McpPromptArgument, McpPromptDefinition, format_prompt_result, parse_prompt_definition,
};
pub use resource::{
    McpCompletionResult, McpResourceDefinition, McpResourceTemplateDefinition,
    format_resource_result, parse_completion_result, parse_resource_definition,
    parse_resource_template_definition,
};
pub use tool::{McpToolDefinition, format_tool_result, parse_tool_definition};
pub use trace::McpTracePropagation;
pub use types::{
    McpCompletionRequest, McpProgressNotification, McpPromptList, McpPromptResult, McpResourceList,
    McpResourceResult, McpResourceTemplateList, McpRoot, McpToolCall, McpToolList, McpToolResult,
};
