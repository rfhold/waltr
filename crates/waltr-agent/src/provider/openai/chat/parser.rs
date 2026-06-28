//! `OpenAI` Chat Completions parser.

use serde_json::{Value, json};

use crate::stream::CompletionEvent;
use crate::{ProviderTurnMetadata, Result, TokenUsage};

use super::super::shared::{
    CHAT_COMPLETIONS_NAME, PartialToolCall, api_error, chargeable_input_tokens,
};

#[derive(Debug)]
pub(super) struct ChatParser {
    provider: &'static str,
    url: String,
    model: String,
    tool_calls: Vec<PartialToolCall>,
}

impl ChatParser {
    pub(super) fn new(
        provider: &'static str,
        url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            url: url.into(),
            model: model.into(),
            tool_calls: Vec::new(),
        }
    }

    pub(super) fn parse_event(&mut self, data: &str) -> Result<Vec<CompletionEvent>> {
        parse_chat_event(
            data,
            self.provider,
            &self.url,
            &self.model,
            &mut self.tool_calls,
        )
    }
}

fn parse_chat_event(
    data: &str,
    provider: &str,
    url: &str,
    model: &str,
    tool_calls: &mut Vec<PartialToolCall>,
) -> Result<Vec<CompletionEvent>> {
    if data == "[DONE]" {
        return Ok(vec![CompletionEvent::Finished]);
    }

    let value: Value = serde_json::from_str(data)?;
    if let Some(error) = value.get("error") {
        return Err(api_error(CHAT_COMPLETIONS_NAME, error));
    }

    // The usage chunk (`stream_options.include_usage`) has an empty `choices`
    // array and arrives before `[DONE]`.
    if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
        return Ok(vec![CompletionEvent::ProviderMetadata(
            ProviderTurnMetadata {
                provider: provider.to_owned(),
                url: url.to_owned(),
                model: model.to_owned(),
                usage: Some({
                    let input_tokens = usage["prompt_tokens"].as_u64();
                    let cache_read_tokens =
                        usage["prompt_tokens_details"]["cached_tokens"].as_u64();
                    TokenUsage {
                        input_tokens: chargeable_input_tokens(input_tokens, cache_read_tokens),
                        output_tokens: usage["completion_tokens"].as_u64(),
                        cache_read_tokens,
                        image_input_tokens: usage["prompt_tokens_details"]["image_tokens"].as_u64(),
                        ..Default::default()
                    }
                }),
                cost: None,
                resume: json!({ "completion_id": value["id"] }),
            },
        )]);
    }

    let Some(choice) = value["choices"].get(0) else {
        return Ok(Vec::new());
    };

    let mut events = Vec::new();
    if let Some(delta) = choice["delta"]["content"].as_str() {
        events.push(CompletionEvent::Delta(delta.to_owned()));
    }

    if let Some(deltas) = choice["delta"]["tool_calls"].as_array() {
        for delta in deltas {
            let index = delta["index"]
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .unwrap_or(0);
            if tool_calls.len() <= index {
                tool_calls.resize_with(index + 1, PartialToolCall::default);
            }
            let call = &mut tool_calls[index];
            if let Some(id) = delta["id"].as_str() {
                id.clone_into(&mut call.id);
            }
            if let Some(name) = delta["function"]["name"].as_str() {
                name.clone_into(&mut call.name);
            }
            if let Some(arguments) = delta["function"]["arguments"].as_str() {
                call.arguments.push_str(arguments);
            }
        }
    }

    if choice["finish_reason"].as_str() == Some("tool_calls") {
        events.extend(
            std::mem::take(tool_calls)
                .into_iter()
                .map(PartialToolCall::into_event)
                .collect::<Result<Vec<_>>>()?,
        );
    }

    Ok(events)
}

#[allow(dead_code)]
fn parse_chat_text_event(data: &str) -> Result<Vec<CompletionEvent>> {
    if data == "[DONE]" {
        return Ok(vec![CompletionEvent::Finished]);
    }

    let value: Value = serde_json::from_str(data)?;
    Ok(value["choices"]
        .get(0)
        .and_then(|choice| choice["delta"]["content"].as_str())
        .map(|delta| vec![CompletionEvent::Delta(delta.to_owned())])
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_chunk_emits_provider_metadata_before_done() {
        let mut parser = ChatParser::new(
            CHAT_COMPLETIONS_NAME,
            "https://example.test/v1/chat/completions",
            "gpt-test",
        );

        let events = parser
            .parse_event(
                r#"{"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3}}"#,
            )
            .expect("usage chunk parses");

        assert_eq!(
            events,
            vec![CompletionEvent::ProviderMetadata(ProviderTurnMetadata {
                provider: CHAT_COMPLETIONS_NAME.to_owned(),
                url: "https://example.test/v1/chat/completions".to_owned(),
                model: "gpt-test".to_owned(),
                usage: Some(TokenUsage {
                    input_tokens: Some(11),
                    output_tokens: Some(3),
                    ..Default::default()
                }),
                cost: None,
                resume: json!({ "completion_id": "chatcmpl-1" }),
            })]
        );

        let done = parser.parse_event("[DONE]").expect("done parses");
        assert_eq!(done, vec![CompletionEvent::Finished]);
    }

    #[test]
    fn usage_chunk_reports_cached_tokens_separately_from_chargeable_input() {
        let mut parser = ChatParser::new(
            CHAT_COMPLETIONS_NAME,
            "https://example.test/v1/chat/completions",
            "gpt-test",
        );

        let events = parser
            .parse_event(
                r#"{"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":7}}}"#,
            )
            .expect("usage chunk parses");

        let CompletionEvent::ProviderMetadata(metadata) = &events[0] else {
            panic!("expected provider metadata, got {events:?}");
        };
        assert_eq!(metadata.usage.expect("usage").input_tokens, Some(4));
        assert_eq!(metadata.usage.expect("usage").cache_read_tokens, Some(7));
    }
}
