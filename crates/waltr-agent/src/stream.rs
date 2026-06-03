//! Provider-neutral LLM streaming interface.

use std::{fmt, future::Future, pin::Pin};

use futures_util::Stream;
use serde_json::Value;

use crate::Result;

/// Boxed future returned by the streaming trait.
pub type BoxStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BoxCompletionStream>> + Send + 'a>>;

/// Boxed stream of provider-neutral completion events.
pub type BoxCompletionStream = Pin<Box<dyn Stream<Item = Result<CompletionEvent>> + Send>>;

/// Provider-neutral completion stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionEvent {
    /// A text delta emitted by the provider.
    Delta(String),
    /// A thinking/reasoning text delta emitted by the provider.
    ThinkingDelta(ThinkingDelta),
    /// A completed provider-specific thinking/reasoning block for replay.
    ThinkingBlock(ThinkingBlock),
    /// The provider requested a tool call.
    ToolCall(ToolCall),
    /// The provider signaled that the completion is finished.
    Finished,
}

/// Provider-neutral completion request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest<'a> {
    /// User prompt.
    pub prompt: &'a str,
    /// Optional system prompt.
    pub system_prompt: Option<&'a str>,
    /// Tools made available to the model.
    pub tools: Vec<ToolDefinition>,
    /// Tool calls requested by earlier agent loop turns.
    pub tool_calls: Vec<ToolCall>,
    /// Tool results returned by earlier agent loop turns.
    pub tool_results: Vec<ToolResult>,
    /// Provider-specific thinking/reasoning blocks returned by earlier turns.
    pub thinking_blocks: Vec<ThinkingBlock>,
}

impl<'a> CompletionRequest<'a> {
    /// Create a request for a prompt with no tools.
    #[must_use]
    pub const fn new(prompt: &'a str) -> Self {
        Self {
            prompt,
            system_prompt: None,
            tools: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            thinking_blocks: Vec::new(),
        }
    }
}

/// Provider-neutral thinking/reasoning text delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingDelta {
    /// Incremental thinking/reasoning text suitable for live display.
    pub text: String,
}

/// Provider-specific thinking/reasoning block preserved for replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingBlock {
    /// Provider family that produced this block.
    pub kind: ThinkingBlockKind,
    /// Displayable thinking/reasoning text, when available.
    pub text: String,
    /// Provider-native payload needed for continuation.
    pub payload: Value,
}

/// Provider family for replayable thinking/reasoning payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingBlockKind {
    /// Anthropic Messages thinking block.
    Anthropic,
    /// `OpenAI` Responses reasoning item.
    OpenAiResponses,
}

/// Builder for a completion request.
pub struct CompletionRequestBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    provider: &'a P,
    request: CompletionRequest<'a>,
}

impl<'a, P> CompletionRequestBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    /// Create a request builder for a provider and prompt.
    #[must_use]
    pub const fn new(provider: &'a P, prompt: &'a str) -> Self {
        Self {
            provider,
            request: CompletionRequest::new(prompt),
        }
    }

    /// Add one tool to the request.
    #[must_use]
    pub fn with_tool(mut self, tool: ToolDefinition) -> Self {
        self.request.tools.push(tool);
        self
    }

    /// Add multiple tools to the request.
    #[must_use]
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = ToolDefinition>) -> Self {
        self.request.tools.extend(tools);
        self
    }

    /// Set the system prompt for the request.
    #[must_use]
    pub const fn with_system_prompt(mut self, system_prompt: &'a str) -> Self {
        self.request.system_prompt = Some(system_prompt);
        self
    }

    /// Stream the request.
    #[must_use]
    pub fn stream(self) -> BoxStreamFuture<'a> {
        self.provider.stream_request(self.request)
    }
}

impl<P> fmt::Debug for CompletionRequestBuilder<'_, P>
where
    P: LlmStream + ?Sized,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletionRequestBuilder")
            .field("provider", &self.provider.name())
            .field("request", &self.request)
            .finish()
    }
}

/// Provider-neutral tool definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human-readable tool description.
    pub description: String,
    /// JSON schema for tool input.
    pub input_schema: Value,
}

impl ToolDefinition {
    /// Create a tool definition.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Provider-neutral tool call emitted by a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Provider tool call identifier.
    pub id: String,
    /// Tool name requested by the model.
    pub name: String,
    /// Raw JSON arguments supplied by the model.
    pub arguments: Value,
}

/// Provider-neutral tool result returned by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Provider tool call identifier.
    pub call_id: String,
    /// Tool name that produced the result.
    pub name: String,
    /// Tool result content returned to the model.
    pub content: String,
}

impl ToolResult {
    /// Create a tool result.
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content: content.into(),
        }
    }
}

impl ToolCall {
    /// Create a tool call event.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// Bare-bones interface for testing LLM text streaming providers.
pub trait LlmStream: Send + Sync {
    /// Provider name used in diagnostics.
    fn name(&self) -> &'static str;

    /// Start building a streaming request.
    fn prompt<'a>(&'a self, prompt: &'a str) -> CompletionRequestBuilder<'a, Self>
    where
        Self: Sized,
    {
        CompletionRequestBuilder::new(self, prompt)
    }

    /// Stream a single user prompt to completion.
    fn stream_prompt<'a>(&'a self, prompt: &'a str) -> BoxStreamFuture<'a> {
        self.stream_request(CompletionRequest::new(prompt))
    }

    /// Stream a completion request.
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::CompletionRequest;

    #[test]
    fn new_request_initializes_empty_thinking_blocks() {
        let request = CompletionRequest::new("hello");

        assert!(request.thinking_blocks.is_empty());
    }
}
