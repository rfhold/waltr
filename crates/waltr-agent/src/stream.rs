//! Provider-neutral LLM streaming interface.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use futures_util::Stream;
use serde::{Deserialize, Serialize};
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
    /// Provider response metadata for the turn (usage, resume hints).
    ProviderMetadata(ProviderTurnMetadata),
    /// The provider signaled that the completion is finished.
    Finished,
}

/// Provider response metadata captured for one turn.
///
/// Metadata is an optimization hint: replay correctness never depends on it,
/// and providers that surface none are valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTurnMetadata {
    /// Provider family that produced this metadata.
    pub provider: String,
    /// Resolved provider endpoint URL that produced this metadata.
    pub url: String,
    /// Provider model that produced this metadata.
    pub model: String,
    /// Token usage reported by the provider, when available.
    pub usage: Option<TokenUsage>,
    /// Cost computed from the usage and model pricing active for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<TokenCost>,
    /// Opaque provider-native resume payload (response identifiers, etc.).
    pub resume: Value,
}

/// Typed token usage reported by a provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input/prompt tokens consumed by the request.
    pub input_tokens: Option<u64>,
    /// Output/completion tokens produced by the response.
    pub output_tokens: Option<u64>,
    /// Input tokens served from provider cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to provider cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Image input tokens consumed by the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input_tokens: Option<u64>,
    /// Image input count consumed by the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input_count: Option<u64>,
}

impl TokenUsage {
    /// Total non-cache input and output tokens reported by the provider.
    #[must_use]
    pub fn total_tokens(self) -> u64 {
        self.input_tokens.unwrap_or_default()
            + self.output_tokens.unwrap_or_default()
            + self.image_input_tokens.unwrap_or_default()
    }
}

/// Model pricing used to compute durable per-turn cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    /// Input token price per million tokens in `currency`.
    pub input_per_million_tokens: Option<f64>,
    /// Output token price per million tokens in `currency`.
    pub output_per_million_tokens: Option<f64>,
    /// Cache-read token price per million tokens in `currency`.
    pub cache_read_per_million_tokens: Option<f64>,
    /// Cache-write token price per million tokens in `currency`.
    pub cache_write_per_million_tokens: Option<f64>,
    /// Image input token price per million tokens in `currency`.
    pub image_input_per_million_tokens: Option<f64>,
    /// Image input price per image in `currency`.
    pub image_input_per_image: Option<f64>,
    /// ISO-ish currency code for the configured rates.
    pub currency: String,
}

impl ModelCost {
    /// Compute deterministic micro-unit cost for one provider usage report.
    #[must_use]
    pub fn cost_for_usage(&self, usage: TokenUsage) -> Option<TokenCost> {
        let input_micros = rate_cost_micros(usage.input_tokens, self.input_per_million_tokens);
        let output_micros = rate_cost_micros(usage.output_tokens, self.output_per_million_tokens);
        let cache_read_micros =
            rate_cost_micros(usage.cache_read_tokens, self.cache_read_per_million_tokens);
        let cache_write_micros = rate_cost_micros(
            usage.cache_write_tokens,
            self.cache_write_per_million_tokens,
        );
        let image_input_micros =
            rate_cost_micros(
                usage.image_input_tokens,
                self.image_input_per_million_tokens,
            ) + image_cost_micros(usage.image_input_count, self.image_input_per_image);
        let total_micros = input_micros
            + output_micros
            + cache_read_micros
            + cache_write_micros
            + image_input_micros;
        (total_micros > 0).then(|| TokenCost {
            currency: if self.currency.is_empty() {
                "USD".to_owned()
            } else {
                self.currency.clone()
            },
            input_micros,
            output_micros,
            cache_read_micros,
            cache_write_micros,
            image_input_micros,
            total_micros,
        })
    }
}

fn rate_cost_micros(tokens: Option<u64>, per_million_tokens: Option<f64>) -> u64 {
    let Some(tokens) = tokens else { return 0 };
    let Some(rate) = per_million_tokens else {
        return 0;
    };
    if !rate.is_finite() || rate <= 0.0 {
        return 0;
    }
    ((tokens as f64) * rate).round() as u64
}

fn image_cost_micros(images: Option<u64>, per_image: Option<f64>) -> u64 {
    let Some(images) = images else { return 0 };
    let Some(rate) = per_image else { return 0 };
    if !rate.is_finite() || rate <= 0.0 {
        return 0;
    }
    ((images as f64) * rate * 1_000_000.0).round() as u64
}

/// Cost computed for a provider turn, stored in currency micro-units.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCost {
    /// Currency code for all micro-unit fields.
    pub currency: String,
    /// Input token cost in micro-units.
    pub input_micros: u64,
    /// Output token cost in micro-units.
    pub output_micros: u64,
    /// Cache-read token cost in micro-units.
    pub cache_read_micros: u64,
    /// Cache-write token cost in micro-units.
    pub cache_write_micros: u64,
    /// Image input cost in micro-units.
    pub image_input_micros: u64,
    /// Total turn cost in micro-units.
    pub total_micros: u64,
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
    /// Completed agent loop turns to replay, in turn order.
    pub turns: Vec<CompletionTurn>,
}

impl<'a> CompletionRequest<'a> {
    /// Create a request for a prompt with no tools.
    #[must_use]
    pub const fn new(prompt: &'a str) -> Self {
        Self {
            prompt,
            system_prompt: None,
            tools: Vec::new(),
            turns: Vec::new(),
        }
    }

    /// Tool calls across all replayed turns, in emission order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.turns.iter().flat_map(CompletionTurn::tool_calls)
    }

    /// Assistant text across all replayed turns, in emission order.
    pub fn assistant_texts(&self) -> impl Iterator<Item = &str> {
        self.turns.iter().flat_map(CompletionTurn::assistant_texts)
    }

    /// Tool results across all replayed turns, in emission order.
    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.turns.iter().flat_map(CompletionTurn::tool_results)
    }

    /// Thinking blocks across all replayed turns, in emission order.
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &ThinkingBlock> {
        self.turns.iter().flat_map(CompletionTurn::thinking_blocks)
    }

    /// Clone this request, retaining provider metadata only when it matches the
    /// exact provider identity that will encode the request.
    #[must_use]
    pub fn for_provider_identity(&self, provider: &str, url: &str, model: &str) -> Self {
        let mut request = self.clone();
        for turn in &mut request.turns {
            if turn.metadata.as_ref().is_some_and(|metadata| {
                metadata.provider != provider || metadata.url != url || metadata.model != model
            }) {
                turn.metadata = None;
            }
        }
        request
    }
}

/// One completed provider turn, preserving item emission order for replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionTurn {
    /// Zero-based agent loop turn index.
    pub turn: u64,
    /// Items produced during the turn, in emission order.
    pub items: Vec<TurnItem>,
    /// Provider response metadata recorded for the turn, when surfaced.
    pub metadata: Option<ProviderTurnMetadata>,
}

impl CompletionTurn {
    /// Create an empty turn record.
    #[must_use]
    pub const fn new(turn: u64) -> Self {
        Self {
            turn,
            items: Vec::new(),
            metadata: None,
        }
    }

    /// Find or create the record for `turn`, keeping the list ordered by turn.
    pub fn entry(turns: &mut Vec<Self>, turn: u64) -> &mut Self {
        let position = match turns.binary_search_by_key(&turn, |record| record.turn) {
            Ok(position) => position,
            Err(position) => {
                turns.insert(position, Self::new(turn));
                position
            }
        };
        &mut turns[position]
    }

    /// Thinking blocks recorded during the turn, in emission order.
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &ThinkingBlock> {
        self.items.iter().filter_map(|item| match item {
            TurnItem::Thinking(block) => Some(block),
            _ => None,
        })
    }

    /// Assistant text segments recorded during the turn, in emission order.
    pub fn assistant_texts(&self) -> impl Iterator<Item = &str> {
        self.items.iter().filter_map(|item| match item {
            TurnItem::AssistantText(text) => Some(text.as_str()),
            _ => None,
        })
    }

    /// Append assistant text to the latest assistant segment, preserving item order.
    pub fn append_assistant_text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        match self.items.last_mut() {
            Some(TurnItem::AssistantText(text)) => text.push_str(delta),
            _ => self.items.push(TurnItem::AssistantText(delta.to_owned())),
        }
    }

    /// Replace the latest assistant text segment with a snapshot value.
    pub fn set_assistant_text_snapshot(&mut self, text: &str) {
        match self.items.last_mut() {
            Some(TurnItem::AssistantText(existing)) => {
                existing.clear();
                existing.push_str(text);
            }
            _ => self.items.push(TurnItem::AssistantText(text.to_owned())),
        }
    }

    /// Tool calls recorded during the turn, in emission order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.items.iter().filter_map(|item| match item {
            TurnItem::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// Tool results recorded during the turn, in emission order.
    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.items.iter().filter_map(|item| match item {
            TurnItem::ToolResult(result) => Some(result),
            _ => None,
        })
    }
}

/// One replayable item produced during a provider turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnItem {
    /// Assistant text emitted by the provider.
    AssistantText(String),
    /// Provider thinking/reasoning block.
    Thinking(ThinkingBlock),
    /// Tool call requested by the model.
    ToolCall(ToolCall),
    /// Tool result returned by the host.
    ToolResult(ToolResult),
}

/// Provider-neutral thinking/reasoning text delta.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingDelta {
    /// Incremental thinking/reasoning text suitable for live display.
    pub text: String,
}

/// Provider-specific thinking/reasoning block preserved for replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    /// Provider family that produced this block.
    pub kind: ThinkingBlockKind,
    /// Displayable thinking/reasoning text, when available.
    pub text: String,
    /// Provider-native payload needed for continuation.
    pub payload: Value,
}

/// Provider family for replayable thinking/reasoning payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider tool call identifier.
    pub id: String,
    /// Tool name requested by the model.
    pub name: String,
    /// Raw JSON arguments supplied by the model.
    pub arguments: Value,
}

/// Provider-neutral tool result returned by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Project the provider-native wire payload for a request without sending it.
    ///
    /// Providers that implement this must guarantee `stream_request` sends
    /// exactly this payload for the same input. Returns `None` when the
    /// provider has no native projection.
    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let _ = request;
        None
    }

    /// Stream a completion request.
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a>;
}

impl<T> LlmStream for Arc<T>
where
    T: LlmStream + ?Sized,
{
    fn name(&self) -> &'static str {
        self.as_ref().name()
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        self.as_ref().project_request(request)
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        self.as_ref().stream_request(request)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CompletionRequest, CompletionTurn, ProviderTurnMetadata, ThinkingBlock, ThinkingBlockKind,
        TurnItem,
    };

    #[test]
    fn new_request_initializes_empty_turns() {
        let request = CompletionRequest::new("hello");

        assert!(request.turns.is_empty());
    }

    #[test]
    fn turn_entry_creates_and_orders_records() {
        let mut turns = Vec::new();

        CompletionTurn::entry(&mut turns, 1)
            .items
            .push(TurnItem::Thinking(ThinkingBlock {
                kind: ThinkingBlockKind::Anthropic,
                text: "later".to_owned(),
                payload: json!({}),
            }));
        CompletionTurn::entry(&mut turns, 0)
            .items
            .push(TurnItem::Thinking(ThinkingBlock {
                kind: ThinkingBlockKind::Anthropic,
                text: "earlier".to_owned(),
                payload: json!({}),
            }));
        CompletionTurn::entry(&mut turns, 1)
            .items
            .push(TurnItem::ToolResult(super::ToolResult::new(
                "call-1", "echo", "ok",
            )));

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn, 0);
        assert_eq!(turns[1].turn, 1);
        assert_eq!(turns[1].items.len(), 2);
        assert_eq!(turns[1].thinking_blocks().count(), 1);
        assert_eq!(turns[1].tool_results().count(), 1);
    }

    #[test]
    fn assistant_text_deltas_merge_until_another_turn_item() {
        let mut turn = CompletionTurn::new(0);

        turn.append_assistant_text("hello ");
        turn.append_assistant_text("world");
        turn.items.push(TurnItem::ToolResult(super::ToolResult::new(
            "call-1", "echo", "ok",
        )));
        turn.append_assistant_text("again");

        assert_eq!(
            turn.items,
            [
                TurnItem::AssistantText("hello world".to_owned()),
                TurnItem::ToolResult(super::ToolResult::new("call-1", "echo", "ok")),
                TurnItem::AssistantText("again".to_owned()),
            ]
        );
        assert_eq!(
            turn.assistant_texts().collect::<Vec<_>>(),
            ["hello world", "again"]
        );
    }

    #[test]
    fn provider_identity_filter_keeps_exact_metadata_match() {
        let mut request =
            request_with_metadata("openai_responses", "https://a.test/v1/responses", "gpt-a");

        request = request.for_provider_identity(
            "openai_responses",
            "https://a.test/v1/responses",
            "gpt-a",
        );

        assert!(request.turns[0].metadata.is_some());
    }

    #[test]
    fn provider_identity_filter_drops_url_or_model_mismatch() {
        let request =
            request_with_metadata("openai_responses", "https://a.test/v1/responses", "gpt-a");

        let wrong_url = request.for_provider_identity(
            "openai_responses",
            "https://b.test/v1/responses",
            "gpt-a",
        );
        let wrong_model = request.for_provider_identity(
            "openai_responses",
            "https://a.test/v1/responses",
            "gpt-b",
        );

        assert!(wrong_url.turns[0].metadata.is_none());
        assert!(wrong_model.turns[0].metadata.is_none());
    }

    fn request_with_metadata(provider: &str, url: &str, model: &str) -> CompletionRequest<'static> {
        let mut request = CompletionRequest::new("hello");
        request.turns = vec![CompletionTurn {
            turn: 0,
            items: Vec::new(),
            metadata: Some(ProviderTurnMetadata {
                provider: provider.to_owned(),
                url: url.to_owned(),
                model: model.to_owned(),
                usage: None,
                cost: None,
                resume: json!({ "id": "resume-1" }),
            }),
        }];
        request
    }
}
