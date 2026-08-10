//! Provider-neutral LLM streaming interface.

use std::{fmt, future::Future, ops::Deref, pin::Pin, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::Result;

/// Boxed future returned by the streaming trait.
pub type BoxStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BoxCompletionStream>> + Send + 'a>>;

/// Boxed stream of provider-neutral completion events.
pub type BoxCompletionStream = Pin<Box<dyn Stream<Item = Result<CompletionEvent>> + Send>>;

/// Provider-neutral completion stream event.
#[allow(clippy::large_enum_variant)]
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
    /// Non-cache/base-priced input tokens consumed by the request.
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
    /// Full input tokens represented by the provider usage report, including
    /// cache reads and writes.
    #[must_use]
    pub fn total_input_tokens(self) -> u64 {
        self.input_tokens.unwrap_or_default()
            + self.cache_read_tokens.unwrap_or_default()
            + self.cache_write_tokens.unwrap_or_default()
            + self.image_input_tokens.unwrap_or_default()
    }

    /// Full context tokens represented by the provider usage report, including
    /// cache reads, cache writes, non-cache input, image input, and output.
    #[must_use]
    pub fn context_tokens(self) -> u64 {
        self.total_input_tokens() + self.output_tokens.unwrap_or_default()
    }

    /// Total non-cache input and output tokens used for base token accounting.
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

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
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

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
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
    /// User prompts to replay with the provider turn they started.
    pub prompt_turns: Vec<PromptTurn>,
    /// Completed agent loop turns to replay, in turn order.
    pub turns: Vec<CompletionTurn>,
}

/// A user prompt and the provider turn it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTurn {
    /// Provider turn the prompt started.
    pub turn: u64,
    /// User prompt text.
    pub prompt: String,
}

/// Provider-neutral chronological replay item for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionReplayItem<'a> {
    /// A prior provider turn item.
    Turn(&'a TurnItem),
    /// The current user prompt that should follow prior replayed turns.
    Prompt(&'a str),
}

impl<'a> CompletionRequest<'a> {
    /// Create a request for a prompt with no tools.
    #[must_use]
    pub const fn new(prompt: &'a str) -> Self {
        Self {
            prompt,
            system_prompt: None,
            tools: Vec::new(),
            prompt_turns: Vec::new(),
            turns: Vec::new(),
        }
    }

    /// Request conversation items in provider-neutral chronological order.
    pub fn replay_items(&'a self) -> Vec<CompletionReplayItem<'a>> {
        if self.prompt_turns.is_empty() {
            return std::iter::once(CompletionReplayItem::Prompt(self.prompt))
                .chain(
                    self.turns
                        .iter()
                        .flat_map(|turn| turn.items.iter().map(CompletionReplayItem::Turn)),
                )
                .collect();
        }

        let mut items = Vec::new();
        let mut prompts = self.prompt_turns.iter().peekable();
        for turn in &self.turns {
            while prompts
                .peek()
                .is_some_and(|prompt_turn| prompt_turn.turn <= turn.turn)
            {
                if let Some(prompt_turn) = prompts.next() {
                    items.push(CompletionReplayItem::Prompt(&prompt_turn.prompt));
                }
            }
            items.extend(turn.items.iter().map(CompletionReplayItem::Turn));
        }
        items.extend(prompts.map(|prompt_turn| CompletionReplayItem::Prompt(&prompt_turn.prompt)));
        items
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

/// Maximum decoded bytes accepted in one provider-neutral tool result.
pub const MAX_TOOL_RESULT_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum images accepted in one provider-neutral tool result.
pub const MAX_TOOL_RESULT_IMAGES: usize = 4;

/// Invalid provider-neutral image content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolResultContentError {
    /// The result contains too many images.
    TooManyImages,
    /// One image exceeds the decoded size limit.
    ImageTooLarge,
    /// Images collectively exceed the decoded size limit.
    AggregateImageSizeExceeded,
    /// Image data is not valid standard Base64.
    InvalidBase64,
    /// Image data is not canonical padded standard Base64.
    NonCanonicalBase64,
    /// The declared media type is unsupported.
    UnsupportedMediaType,
    /// Decoded bytes do not match the declared media type.
    MediaTypeMismatch,
}

impl fmt::Display for ToolResultContentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyImages => "tool result contains too many images",
            Self::ImageTooLarge => "tool result image exceeds the size limit",
            Self::AggregateImageSizeExceeded => "tool result images exceed the size limit",
            Self::InvalidBase64 => "tool result image is not valid standard Base64",
            Self::NonCanonicalBase64 => "tool result image Base64 is not canonical",
            Self::UnsupportedMediaType => "tool result image uses an unsupported media type",
            Self::MediaTypeMismatch => "tool result image does not match its media type",
        })
    }
}

impl std::error::Error for ToolResultContentError {}

/// Validated inline image content.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct ToolResultImage {
    data: String,
    media_type: String,
    decoded_bytes: u64,
    sha256: String,
}

impl ToolResultImage {
    /// Validate standard Base64 image data and derive safe metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid encoding, unsupported media, mismatched bytes, or excess size.
    pub fn new(
        data: impl Into<String>,
        media_type: impl Into<String>,
    ) -> std::result::Result<Self, ToolResultContentError> {
        let data = data.into();
        let media_type = media_type.into();
        let max_encoded_bytes = MAX_TOOL_RESULT_IMAGE_BYTES.div_ceil(3) * 4;
        if data.len() > max_encoded_bytes {
            return Err(ToolResultContentError::ImageTooLarge);
        }
        let decoded = STANDARD
            .decode(&data)
            .map_err(|_| ToolResultContentError::InvalidBase64)?;
        if decoded.len() > MAX_TOOL_RESULT_IMAGE_BYTES {
            return Err(ToolResultContentError::ImageTooLarge);
        }
        if STANDARD.encode(&decoded) != data {
            return Err(ToolResultContentError::NonCanonicalBase64);
        }
        let magic_matches = match media_type.as_str() {
            "image/png" => decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => decoded.starts_with(&[0xff, 0xd8, 0xff]),
            "image/webp" => {
                decoded.len() >= 12 && &decoded[..4] == b"RIFF" && &decoded[8..12] == b"WEBP"
            }
            _ => return Err(ToolResultContentError::UnsupportedMediaType),
        };
        if !magic_matches {
            return Err(ToolResultContentError::MediaTypeMismatch);
        }
        let digest = Sha256::digest(&decoded);
        let mut sha256 = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(sha256, "{byte:02x}");
        }
        Ok(Self {
            data,
            media_type,
            decoded_bytes: decoded.len() as u64,
            sha256,
        })
    }

    /// Standard padded Base64 image data.
    #[must_use]
    pub fn data(&self) -> &str {
        &self.data
    }

    /// Validated image media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Decoded image size.
    #[must_use]
    pub const fn decoded_bytes(&self) -> u64 {
        self.decoded_bytes
    }

    /// Lowercase SHA-256 digest of decoded image data.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

impl fmt::Debug for ToolResultImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResultImage")
            .field("media_type", &self.media_type)
            .field("decoded_bytes", &self.decoded_bytes)
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for ToolResultImage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PersistedImage {
            data: String,
            media_type: String,
            #[serde(default, rename = "decoded_bytes")]
            _decoded_bytes: Option<u64>,
            #[serde(default, rename = "sha256")]
            _sha256: Option<String>,
        }

        let persisted = PersistedImage::deserialize(deserializer)?;
        Self::new(persisted.data, persisted.media_type).map_err(serde::de::Error::custom)
    }
}

/// One ordered provider-neutral tool-result content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentBlock {
    /// Plain text returned by the tool.
    Text {
        /// Text content.
        text: String,
    },
    /// A validated inline image returned by the tool.
    Image(ToolResultImage),
}

/// Ordered provider-neutral tool-result content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultContent {
    blocks: Vec<ToolResultContentBlock>,
    text_projection: String,
}

impl ToolResultContent {
    /// Create content containing one text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::from_validated_blocks(vec![ToolResultContentBlock::Text { text: text.into() }])
    }

    /// Create content from ordered validated blocks.
    ///
    /// # Errors
    ///
    /// Returns an error when image count or aggregate decoded size exceeds the limits.
    pub fn from_blocks(
        blocks: Vec<ToolResultContentBlock>,
    ) -> std::result::Result<Self, ToolResultContentError> {
        let mut image_count = 0usize;
        let mut decoded_bytes = 0u64;
        for block in &blocks {
            if let ToolResultContentBlock::Image(image) = block {
                image_count = image_count.saturating_add(1);
                if image_count > MAX_TOOL_RESULT_IMAGES {
                    return Err(ToolResultContentError::TooManyImages);
                }
                decoded_bytes = decoded_bytes
                    .checked_add(image.decoded_bytes())
                    .ok_or(ToolResultContentError::AggregateImageSizeExceeded)?;
                if decoded_bytes > MAX_TOOL_RESULT_IMAGE_BYTES as u64 {
                    return Err(ToolResultContentError::AggregateImageSizeExceeded);
                }
            }
        }
        Ok(Self::from_validated_blocks(blocks))
    }

    fn from_validated_blocks(blocks: Vec<ToolResultContentBlock>) -> Self {
        let text_projection = tool_result_text_projection(&blocks);
        Self {
            blocks,
            text_projection,
        }
    }

    /// Return ordered content blocks.
    #[must_use]
    pub fn blocks(&self) -> &[ToolResultContentBlock] {
        &self.blocks
    }

    /// Return validated image blocks.
    pub fn images(&self) -> impl Iterator<Item = &ToolResultImage> {
        self.blocks.iter().filter_map(|block| match block {
            ToolResultContentBlock::Image(image) => Some(image),
            ToolResultContentBlock::Text { .. } => None,
        })
    }

    /// Append text without disturbing existing image blocks.
    pub fn push_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.blocks.last_mut() {
            Some(ToolResultContentBlock::Text { text: existing }) => existing.push_str(text),
            _ => self.blocks.push(ToolResultContentBlock::Text {
                text: text.to_owned(),
            }),
        }
        self.text_projection = tool_result_text_projection(&self.blocks);
    }

    /// Return a safe text projection that replaces image data with metadata.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text_projection
    }
}

impl Default for ToolResultContent {
    fn default() -> Self {
        Self::from_validated_blocks(Vec::new())
    }
}

impl Deref for ToolResultContent {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for ToolResultContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for ToolResultContent {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for ToolResultContent {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl Serialize for ToolResultContent {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.blocks.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolResultContent {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ContentVisitor;

        impl<'de> serde::de::Visitor<'de> for ContentVisitor {
            type Value = ToolResultContent;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("legacy text or an array of tool-result content blocks")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ToolResultContent::text(value))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ToolResultContent::text(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut blocks = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(16));
                let mut image_count = 0usize;
                let mut decoded_bytes = 0u64;
                while let Some(block) = sequence.next_element::<ToolResultContentBlock>()? {
                    if let ToolResultContentBlock::Image(image) = &block {
                        image_count = image_count.saturating_add(1);
                        if image_count > MAX_TOOL_RESULT_IMAGES {
                            return Err(serde::de::Error::custom(
                                ToolResultContentError::TooManyImages,
                            ));
                        }
                        decoded_bytes = decoded_bytes
                            .checked_add(image.decoded_bytes())
                            .ok_or_else(|| {
                                serde::de::Error::custom(
                                    ToolResultContentError::AggregateImageSizeExceeded,
                                )
                            })?;
                        if decoded_bytes > MAX_TOOL_RESULT_IMAGE_BYTES as u64 {
                            return Err(serde::de::Error::custom(
                                ToolResultContentError::AggregateImageSizeExceeded,
                            ));
                        }
                    }
                    blocks.push(block);
                }
                Ok(ToolResultContent::from_validated_blocks(blocks))
            }
        }

        deserializer.deserialize_any(ContentVisitor)
    }
}

fn tool_result_text_projection(blocks: &[ToolResultContentBlock]) -> String {
    let mut projection = String::new();
    for block in blocks {
        if !projection.is_empty() {
            projection.push('\n');
        }
        match block {
            ToolResultContentBlock::Text { text } => projection.push_str(text),
            ToolResultContentBlock::Image(image) => {
                use std::fmt::Write as _;
                let _ = write!(
                    projection,
                    "[image: {}, {} bytes, sha256:{}]",
                    image.media_type(),
                    image.decoded_bytes(),
                    image.sha256()
                );
            }
        }
    }
    projection
}

/// Provider-neutral tool result returned by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResult {
    /// Provider tool call identifier.
    pub call_id: String,
    /// Tool name that produced the result.
    pub name: String,
    /// Ordered tool result content returned to the model.
    pub content: ToolResultContent,
    /// Whether tool execution reported an error.
    #[serde(default)]
    pub is_error: bool,
}

impl<'de> Deserialize<'de> for ToolResult {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PersistedToolResult {
            call_id: String,
            name: String,
            content: ToolResultContent,
            #[serde(default)]
            is_error: Option<bool>,
        }

        let persisted = PersistedToolResult::deserialize(deserializer)?;
        let is_error = persisted.is_error.unwrap_or_else(|| {
            persisted.content.starts_with("Tool error:")
                || persisted.content.starts_with("MCP tool error:")
        });
        Ok(Self {
            call_id: persisted.call_id,
            name: persisted.name,
            content: persisted.content,
            is_error,
        })
    }
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
            content: ToolResultContent::text(content),
            is_error: false,
        }
    }

    /// Create a tool result from ordered content blocks.
    #[must_use]
    pub fn from_content(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: ToolResultContent,
        is_error: bool,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            content,
            is_error,
        }
    }

    /// Mark this result as an execution error.
    #[must_use]
    pub const fn with_error(mut self) -> Self {
        self.is_error = true;
        self
    }

    /// Return the safe text projection used by non-image presentation surfaces.
    #[must_use]
    pub fn text_projection(&self) -> &str {
        self.content.as_str()
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
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    use super::{
        CompletionRequest, CompletionTurn, MAX_TOOL_RESULT_IMAGE_BYTES, MAX_TOOL_RESULT_IMAGES,
        ProviderTurnMetadata, ThinkingBlock, ThinkingBlockKind, TokenUsage, ToolResult,
        ToolResultContent, ToolResultContentBlock, ToolResultContentError, ToolResultImage,
        TurnItem,
    };

    #[test]
    fn new_request_initializes_empty_turns() {
        let request = CompletionRequest::new("hello");

        assert!(request.turns.is_empty());
    }

    #[test]
    fn token_usage_separates_base_total_from_full_context() {
        let usage = TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            cache_read_tokens: Some(100),
            cache_write_tokens: Some(20),
            image_input_tokens: Some(3),
            ..Default::default()
        };

        assert_eq!(usage.total_tokens(), 17);
        assert_eq!(usage.total_input_tokens(), 133);
        assert_eq!(usage.context_tokens(), 137);
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
    fn tool_result_deserializes_legacy_text_and_serializes_ordered_blocks() {
        let result: ToolResult = serde_json::from_value(json!({
            "call_id": "call-1",
            "name": "camera",
            "content": "legacy text"
        }))
        .expect("legacy tool result");

        assert_eq!(result.content, "legacy text");
        assert!(!result.is_error);
        assert_eq!(
            serde_json::to_value(result).expect("serialized tool result")["content"][0],
            json!({ "type": "text", "text": "legacy text" })
        );
    }

    #[test]
    fn tool_result_preserves_images_but_projects_only_safe_metadata() {
        let result = ToolResult::from_content(
            "call-1",
            "camera",
            ToolResultContent::from_blocks(vec![
                ToolResultContentBlock::Text {
                    text: "snapshot".to_owned(),
                },
                ToolResultContentBlock::Image(
                    ToolResultImage::new("iVBORw0KGgo=", "image/png").expect("valid image"),
                ),
            ])
            .expect("bounded content"),
            false,
        );

        assert_eq!(result.content.blocks().len(), 2);
        assert!(result.text_projection().contains("image/png"));
        assert!(result.text_projection().contains("8 bytes"));
        assert!(!result.text_projection().contains("iVBOR"));
        let round_trip: ToolResult =
            serde_json::from_value(serde_json::to_value(&result).expect("serialized tool result"))
                .expect("round-trip tool result");
        assert_eq!(round_trip, result);
    }

    #[test]
    fn tool_result_image_boundary_rejects_invalid_size_count_and_aggregate() {
        let oversized = "A".repeat(MAX_TOOL_RESULT_IMAGE_BYTES.div_ceil(3) * 4 + 1);
        assert_eq!(
            ToolResultImage::new(oversized, "image/png"),
            Err(ToolResultContentError::ImageTooLarge)
        );

        let image = ToolResultImage::new("iVBORw0KGgo=", "image/png").expect("valid image");
        let excessive = vec![ToolResultContentBlock::Image(image); MAX_TOOL_RESULT_IMAGES + 1];
        assert_eq!(
            ToolResultContent::from_blocks(excessive),
            Err(ToolResultContentError::TooManyImages)
        );

        let mut large_png = vec![0; MAX_TOOL_RESULT_IMAGE_BYTES / 2 + 1];
        large_png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let encoded = STANDARD.encode(large_png);
        let first = ToolResultImage::new(&encoded, "image/png").expect("first image");
        let second = ToolResultImage::new(encoded, "image/png").expect("second image");
        assert_eq!(
            ToolResultContent::from_blocks(vec![
                ToolResultContentBlock::Image(first),
                ToolResultContentBlock::Image(second),
            ]),
            Err(ToolResultContentError::AggregateImageSizeExceeded)
        );
    }

    #[test]
    fn tool_result_deserialization_revalidates_data_and_derives_metadata() {
        let result: ToolResult = serde_json::from_value(json!({
            "call_id": "call-image",
            "name": "camera",
            "content": [{
                "type": "image",
                "data": "iVBORw0KGgo=",
                "media_type": "image/png",
                "decoded_bytes": 999,
                "sha256": "forged"
            }],
            "is_error": false
        }))
        .expect("validated persisted image");

        assert!(result.text_projection().contains("8 bytes"));
        assert!(!result.text_projection().contains("999"));
        assert!(!result.text_projection().contains("forged"));
        let invalid = serde_json::from_value::<ToolResult>(json!({
            "call_id": "call-image",
            "name": "camera",
            "content": [{
                "type": "image",
                "data": "not-base64",
                "media_type": "image/png"
            }]
        }));
        assert!(invalid.is_err());
    }

    #[test]
    fn tool_result_deserialization_stops_at_the_image_count_limit() {
        let mut content = (0..=MAX_TOOL_RESULT_IMAGES)
            .map(|_| {
                json!({
                    "type": "image",
                    "data": "iVBORw0KGgo=",
                    "media_type": "image/png"
                })
            })
            .collect::<Vec<_>>();
        content.push(json!({
            "type": "image",
            "data": "must-not-be-deserialized"
        }));

        let error = serde_json::from_value::<ToolResult>(json!({
            "call_id": "call-image",
            "name": "camera",
            "content": content
        }))
        .expect_err("excess image count must fail incrementally");

        assert!(error.to_string().contains("too many images"));
        assert!(!error.to_string().contains("MIME type"));
        assert!(!error.to_string().contains("must-not-be-deserialized"));
    }

    #[test]
    fn tool_result_debug_never_exposes_image_data() {
        let result = ToolResult::from_content(
            "call-image",
            "camera",
            ToolResultContent::from_blocks(vec![ToolResultContentBlock::Image(
                ToolResultImage::new("iVBORw0KGgo=", "image/png").expect("valid image"),
            )])
            .expect("bounded content"),
            false,
        );
        let mut request = CompletionRequest::new("look");
        request.turns = vec![CompletionTurn {
            turn: 0,
            items: vec![TurnItem::ToolResult(result.clone())],
            metadata: None,
        }];

        for debug in [format!("{result:?}"), format!("{request:?}")] {
            assert!(!debug.contains("iVBOR"));
            assert!(debug.contains("image/png"));
            assert!(debug.contains("decoded_bytes"));
        }
    }

    #[test]
    fn legacy_error_text_infers_structured_error_state() {
        for content in ["Tool error: denied", "MCP tool error: rejected"] {
            let result: ToolResult = serde_json::from_value(json!({
                "call_id": "call-1",
                "name": "tool",
                "content": content
            }))
            .expect("legacy error result");
            assert!(result.is_error);
        }
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
