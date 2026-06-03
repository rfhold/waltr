//! Anthropic Messages streaming provider.

mod parser;
mod request;
mod thinking;

use std::collections::HashMap;

use futures_util::{Stream, StreamExt, stream};
use serde_json::{Value, json};

use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, LlmError, LlmStream, Result,
    ThinkingBlock, ThinkingBlockKind, ThinkingDelta, ToolCall, ToolDefinition,
    stream::BoxStreamFuture,
};

const PROVIDER: &str = "anthropic";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MAX_TOKENS: u32 = 256;

/// Anthropic Messages API streaming provider.
#[derive(Debug, Clone)]
pub struct AnthropicMessages {
    api_key: String,
    model: String,
    messages_url: String,
    max_tokens: u32,
    thinking: Option<AnthropicThinkingConfig>,
    client: reqwest::Client,
}

/// Anthropic Messages thinking configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicThinkingConfig {
    /// Manual thinking budget mode.
    Enabled {
        /// Maximum thinking tokens. Must be less than `max_tokens`.
        budget_tokens: u32,
        /// Optional display mode.
        display: Option<AnthropicThinkingDisplay>,
    },
    /// Adaptive thinking mode used by newer Claude models.
    Adaptive {
        /// Optional display mode.
        display: Option<AnthropicThinkingDisplay>,
    },
}

/// Anthropic thinking display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingDisplay {
    /// Stream summarized thinking when available.
    Summarized,
    /// Omit thinking text while preserving signatures for continuation.
    Omitted,
}

impl AnthropicMessages {
    /// Create a Messages API provider for the given API key and model.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            messages_url: ANTHROPIC_MESSAGES_URL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: None,
            client: reqwest::Client::new(),
        }
    }

    /// Set the API base URL used by this provider.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        self.messages_url = format!("{}/v1/messages", base_url.as_ref().trim_end_matches('/'));
        self
    }

    /// Set the maximum number of tokens to sample.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Enable Anthropic thinking for requests from this provider.
    #[must_use]
    pub const fn with_thinking(mut self, config: AnthropicThinkingConfig) -> Self {
        self.thinking = Some(config);
        self
    }
}

impl LlmStream for AnthropicMessages {
    fn name(&self) -> &'static str {
        "anthropic_messages"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut request_builder = self
                .client
                .post(&self.messages_url)
                .header("anthropic-version", "2023-06-01")
                .json(&messages_request(
                    &self.model,
                    self.max_tokens,
                    self.thinking.as_ref(),
                    &request,
                ));
            if !self.api_key.is_empty() {
                request_builder = request_builder.header("x-api-key", &self.api_key);
            }
            let response = request_builder.send().await?;

            let status = response.status();
            if !status.is_success() {
                let text = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
                return Err(LlmError::Api {
                    provider: PROVIDER,
                    message: format!("{status}: {text}"),
                });
            }

            Ok(completion_stream(response.bytes_stream()))
        })
    }
}

fn messages_request(
    model: &str,
    max_tokens: u32,
    thinking: Option<&AnthropicThinkingConfig>,
    request: &CompletionRequest<'_>,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": anthropic_messages(request),
        "tools": anthropic_tools(&request.tools),
    });
    if let Some(system_prompt) = request.system_prompt {
        body["system"] = json!(system_prompt);
    }
    if let Some(thinking) = thinking {
        body["thinking"] = anthropic_thinking(thinking);
    }
    body
}

fn anthropic_thinking(config: &AnthropicThinkingConfig) -> Value {
    let mut thinking = match config {
        AnthropicThinkingConfig::Enabled {
            budget_tokens,
            display: _,
        } => json!({ "type": "enabled", "budget_tokens": budget_tokens }),
        AnthropicThinkingConfig::Adaptive { display: _ } => json!({ "type": "adaptive" }),
    };
    let display = match config {
        AnthropicThinkingConfig::Enabled { display, .. }
        | AnthropicThinkingConfig::Adaptive { display } => *display,
    };
    if let Some(display) = display {
        thinking["display"] = json!(match display {
            AnthropicThinkingDisplay::Summarized => "summarized",
            AnthropicThinkingDisplay::Omitted => "omitted",
        });
    }
    thinking
}

fn anthropic_messages(request: &CompletionRequest<'_>) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "user", "content": request.prompt })];
    if request.tool_calls.is_empty() {
        return messages;
    }

    let mut assistant_content = request
        .thinking_blocks
        .iter()
        .filter(|block| block.kind == ThinkingBlockKind::Anthropic)
        .map(|block| block.payload.clone())
        .collect::<Vec<_>>();
    assistant_content.extend(request.tool_calls.iter().map(|call| {
        json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.arguments,
        })
    }));
    messages.push(json!({
        "role": "assistant",
        "content": assistant_content
    }));
    messages.push(json!({
        "role": "user",
        "content": request
            .tool_results
            .iter()
            .map(|result| json!({
                "type": "tool_result",
                "tool_use_id": result.call_id,
                "content": result.content,
            }))
            .collect::<Vec<_>>()
    }));
    messages
}

fn anthropic_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect()
}

fn completion_stream<S, B>(bytes_stream: S) -> BoxCompletionStream
where
    S: Stream<Item = std::result::Result<B, reqwest::Error>> + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    let bytes_stream = Box::pin(bytes_stream);
    Box::pin(stream::unfold(
        (bytes_stream, Vec::new(), AnthropicParser::default(), false),
        |(mut bytes_stream, mut buffer, mut parser, done)| async move {
            if done {
                return None;
            }

            loop {
                if let Some(result) = next_event(&mut buffer, &mut parser) {
                    match result {
                        Ok(Some(event)) => {
                            let done = matches!(event, CompletionEvent::Finished);
                            return Some((Ok(event), (bytes_stream, buffer, parser, done)));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            return Some((Err(error), (bytes_stream, buffer, parser, true)));
                        }
                    }
                }

                match bytes_stream.next().await {
                    Some(Ok(bytes)) => buffer.extend_from_slice(bytes.as_ref()),
                    Some(Err(error)) => {
                        return Some((Err(error.into()), (bytes_stream, buffer, parser, true)));
                    }
                    None => {
                        if buffer.is_empty() {
                            return None;
                        }

                        let result = parse_sse_event(std::mem::take(&mut buffer), &mut parser);
                        match result {
                            Ok(Some(event)) => {
                                let done = matches!(event, CompletionEvent::Finished);
                                return Some((Ok(event), (bytes_stream, buffer, parser, done)));
                            }
                            Ok(None) => return None,
                            Err(error) => {
                                return Some((Err(error), (bytes_stream, buffer, parser, true)));
                            }
                        }
                    }
                }
            }
        },
    ))
}

fn next_event(
    buffer: &mut Vec<u8>,
    parser: &mut AnthropicParser,
) -> Option<Result<Option<CompletionEvent>>> {
    let (index, delimiter_len) = event_delimiter(buffer)?;
    let mut event = buffer.split_off(index + delimiter_len);
    std::mem::swap(buffer, &mut event);
    event.truncate(index);
    Some(parse_sse_event(event, parser))
}

fn event_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        })
}

fn parse_sse_event(
    event: Vec<u8>,
    parser: &mut AnthropicParser,
) -> Result<Option<CompletionEvent>> {
    let event = String::from_utf8(event).map_err(|error| LlmError::Protocol {
        provider: PROVIDER,
        message: format!("invalid UTF-8 SSE event: {error}"),
    })?;

    let data = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");

    if data.is_empty() {
        return Ok(None);
    }

    parser.map_event(&serde_json::from_str(&data)?)
}

#[derive(Debug, Default)]
struct AnthropicParser {
    tool_call: Option<AnthropicToolCall>,
    thinking_blocks: HashMap<usize, AnthropicThinkingBlock>,
}

impl AnthropicParser {
    #[allow(clippy::too_many_lines)]
    fn map_event(&mut self, value: &Value) -> Result<Option<CompletionEvent>> {
        match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = value["index"]
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .unwrap_or(0);
                let block = &value["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    self.tool_call = Some(AnthropicToolCall {
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                        input: block["input"].to_string(),
                    });
                } else if block["type"].as_str() == Some("thinking") {
                    self.thinking_blocks.insert(
                        index,
                        AnthropicThinkingBlock {
                            text: block["thinking"].as_str().unwrap_or_default().to_owned(),
                            signature: block["signature"].as_str().map(str::to_owned),
                        },
                    );
                }
                Ok(None)
            }
            Some("content_block_delta") => {
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => Ok(delta["text"]
                        .as_str()
                        .map(|text| CompletionEvent::Delta(text.to_owned()))),
                    Some("input_json_delta") => {
                        if let Some(tool_call) = self.tool_call.as_mut() {
                            if tool_call.input == "null" || tool_call.input == "{}" {
                                tool_call.input.clear();
                            }
                            tool_call
                                .input
                                .push_str(delta["partial_json"].as_str().unwrap_or_default());
                        }
                        Ok(None)
                    }
                    Some("thinking_delta") => {
                        let index = value["index"]
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .unwrap_or(0);
                        let text = delta["thinking"].as_str().unwrap_or_default().to_owned();
                        self.thinking_blocks
                            .entry(index)
                            .or_default()
                            .text
                            .push_str(&text);
                        Ok(Some(CompletionEvent::ThinkingDelta(ThinkingDelta { text })))
                    }
                    Some("signature_delta") => {
                        let index = value["index"]
                            .as_u64()
                            .and_then(|index| usize::try_from(index).ok())
                            .unwrap_or(0);
                        self.thinking_blocks.entry(index).or_default().signature =
                            delta["signature"].as_str().map(str::to_owned);
                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            Some("content_block_stop") => {
                let index = value["index"]
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .unwrap_or(0);
                if let Some(block) = self.thinking_blocks.remove(&index) {
                    return Ok(Some(CompletionEvent::ThinkingBlock(block.into_event())));
                }
                self.tool_call
                    .take()
                    .map(AnthropicToolCall::into_event)
                    .transpose()
            }
            Some("message_stop") => Ok(Some(CompletionEvent::Finished)),
            Some("error") => Err(LlmError::Api {
                provider: PROVIDER,
                message: anthropic_error_message(value),
            }),
            _ => Ok(None),
        }
    }
}

#[derive(Debug, Default)]
struct AnthropicThinkingBlock {
    text: String,
    signature: Option<String>,
}

impl AnthropicThinkingBlock {
    fn into_event(self) -> ThinkingBlock {
        let mut payload = json!({
            "type": "thinking",
            "thinking": self.text,
        });
        if let Some(signature) = self.signature {
            payload["signature"] = json!(signature);
        }
        ThinkingBlock {
            kind: ThinkingBlockKind::Anthropic,
            text: payload["thinking"].as_str().unwrap_or_default().to_owned(),
            payload,
        }
    }
}

#[derive(Debug)]
struct AnthropicToolCall {
    id: String,
    name: String,
    input: String,
}

impl AnthropicToolCall {
    fn into_event(self) -> Result<CompletionEvent> {
        let arguments = if self.input.trim().is_empty() || self.input == "null" {
            json!({})
        } else {
            serde_json::from_str(&self.input)?
        };
        Ok(CompletionEvent::ToolCall(ToolCall::new(
            self.id, self.name, arguments,
        )))
    }
}

fn anthropic_error_message(value: &Value) -> String {
    let Some(error) = value.get("error") else {
        return value.to_string();
    };

    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let error_type = error.get("type").and_then(Value::as_str).map(str::to_owned);

    match (error_type, message) {
        (Some(error_type), Some(message)) => format!("{error_type}: {message}"),
        (None, Some(message)) => message,
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::CompletionRequest;

    use super::{AnthropicThinkingConfig, AnthropicThinkingDisplay, messages_request};

    #[test]
    fn request_includes_system_prompt_when_set() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = messages_request("claude-test", 256, None, &request);

        assert_eq!(body["system"], "system text");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn request_includes_thinking_when_configured() {
        let request = CompletionRequest::new("hello");

        let body = messages_request(
            "claude-test",
            256,
            Some(&AnthropicThinkingConfig::Enabled {
                budget_tokens: 1024,
                display: Some(AnthropicThinkingDisplay::Summarized),
            }),
            &request,
        );

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["thinking"]["display"], "summarized");
    }
}
