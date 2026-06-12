use std::collections::HashMap;

use serde_json::{Value, json};

use crate::{
    CompletionEvent, LlmError, ProviderTurnMetadata, Result, ThinkingDelta, TokenUsage, ToolCall,
};

use super::{PROVIDER, thinking::AnthropicThinkingBlock};

#[derive(Debug)]
pub(super) struct AnthropicParser {
    provider: &'static str,
    url: String,
    model: String,
    tool_call: Option<AnthropicToolCall>,
    thinking_blocks: HashMap<usize, AnthropicThinkingBlock>,
    message_id: Option<String>,
    input_tokens: Option<u64>,
}

impl AnthropicParser {
    pub(super) fn new(
        provider: &'static str,
        url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            url: url.into(),
            model: model.into(),
            tool_call: None,
            thinking_blocks: HashMap::new(),
            message_id: None,
            input_tokens: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn map_event(&mut self, value: &Value) -> Result<Option<CompletionEvent>> {
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
            Some("message_start") => {
                let message = &value["message"];
                self.message_id = message["id"].as_str().map(str::to_owned);
                self.input_tokens = message["usage"]["input_tokens"].as_u64();
                Ok(None)
            }
            Some("message_delta") => Ok(Some(CompletionEvent::ProviderMetadata(
                ProviderTurnMetadata {
                    provider: self.provider.to_owned(),
                    url: self.url.clone(),
                    model: self.model.clone(),
                    usage: Some(TokenUsage {
                        input_tokens: self.input_tokens.take(),
                        output_tokens: value["usage"]["output_tokens"].as_u64(),
                    }),
                    resume: json!({ "message_id": self.message_id.take() }),
                },
            ))),
            Some("message_stop") => Ok(Some(CompletionEvent::Finished)),
            Some("error") => Err(LlmError::Api {
                provider: PROVIDER,
                message: anthropic_error_message(value),
            }),
            _ => Ok(None),
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
    use serde_json::json;

    use crate::provider::anthropic::ANTHROPIC_MESSAGES_NAME;

    use super::*;

    #[test]
    fn message_delta_emits_provider_metadata_with_usage_and_message_id() {
        let mut parser = AnthropicParser::new(
            ANTHROPIC_MESSAGES_NAME,
            "https://example.test/v1/messages",
            "claude-test",
        );

        let started = parser
            .map_event(&json!({
                "type": "message_start",
                "message": { "id": "msg_123", "usage": { "input_tokens": 7 } },
            }))
            .expect("message_start parses");
        assert!(started.is_none());

        let event = parser
            .map_event(&json!({
                "type": "message_delta",
                "usage": { "output_tokens": 9 },
            }))
            .expect("message_delta parses")
            .expect("message_delta emits metadata");

        let CompletionEvent::ProviderMetadata(metadata) = event else {
            panic!("expected provider metadata, got {event:?}");
        };
        assert_eq!(metadata.provider, ANTHROPIC_MESSAGES_NAME);
        assert_eq!(metadata.url, "https://example.test/v1/messages");
        assert_eq!(metadata.model, "claude-test");
        assert_eq!(
            metadata.usage,
            Some(TokenUsage {
                input_tokens: Some(7),
                output_tokens: Some(9),
            })
        );
        assert_eq!(metadata.resume, json!({ "message_id": "msg_123" }));
    }
}
