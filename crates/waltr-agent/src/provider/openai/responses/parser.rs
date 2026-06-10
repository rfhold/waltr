//! `OpenAI` Responses parser.

use serde_json::{Value, json};

use crate::stream::{CompletionEvent, ThinkingBlock, ThinkingBlockKind, ThinkingDelta};
use crate::{ProviderTurnMetadata, Result, TokenUsage};

use crate::provider::openai::shared::{PartialToolCall, RESPONSES_NAME, api_error};

#[derive(Debug)]
pub(super) struct ResponsesParser {
    provider: &'static str,
    url: String,
    model: String,
    tool_call: Option<PartialToolCall>,
    reasoning_item: Option<Value>,
}

impl ResponsesParser {
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
            reasoning_item: None,
        }
    }

    pub(super) fn parse_event(&mut self, data: &str) -> Result<Vec<CompletionEvent>> {
        let value: Value = serde_json::from_str(data)?;
        parse_responses_event_value(
            &value,
            self.provider,
            &self.url,
            &self.model,
            &mut self.tool_call,
            &mut self.reasoning_item,
        )
    }
}

fn parse_responses_event_value(
    value: &Value,
    provider: &str,
    url: &str,
    model: &str,
    tool_call: &mut Option<PartialToolCall>,
    reasoning_item: &mut Option<Value>,
) -> Result<Vec<CompletionEvent>> {
    match value["type"].as_str() {
        Some("response.output_text.delta") => Ok(value["delta"]
            .as_str()
            .map(|delta| vec![CompletionEvent::Delta(delta.to_owned())])
            .unwrap_or_default()),
        Some("response.output_item.added") => {
            let item = &value["item"];
            if item["type"].as_str() == Some("function_call") {
                let call = partial_tool_call_from_response_item(item);
                *tool_call = Some(call);
            } else if item["type"].as_str() == Some("reasoning") {
                *reasoning_item = Some(item.clone());
            }
            Ok(Vec::new())
        }
        Some("response.output_item.done") => {
            let item = &value["item"];
            if item["type"].as_str() == Some("reasoning") {
                return Ok(vec![CompletionEvent::ThinkingBlock(openai_thinking_block(
                    item,
                ))]);
            }
            Ok(Vec::new())
        }
        Some("response.reasoning_summary_text.delta") => Ok(value["delta"]
            .as_str()
            .map(|text| {
                vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default()),
        Some("response.reasoning_summary_text.done") => Ok(value["text"]
            .as_str()
            .map(|text| {
                if let Some(item) = reasoning_item.as_mut() {
                    item["summary"] = json!([{ "type": "summary_text", "text": text }]);
                }
                vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default()),
        Some("response.function_call_arguments.delta") => {
            let delta = value["delta"].as_str().unwrap_or_default();
            if let Some(call) = tool_call.as_mut() {
                call.arguments.push_str(delta);
            }
            Ok(Vec::new())
        }
        Some("response.function_call_arguments.done") => {
            if let Some(arguments) = value["arguments"].as_str()
                && let Some(call) = tool_call.as_mut()
            {
                arguments.clone_into(&mut call.arguments);
            }
            Ok(tool_call
                .take()
                .map(PartialToolCall::into_event)
                .transpose()?
                .map(|event| vec![event])
                .unwrap_or_default())
        }
        Some("response.completed") => {
            let response = &value["response"];
            let usage = response
                .get("usage")
                .filter(|usage| usage.is_object())
                .map(|usage| TokenUsage {
                    input_tokens: usage["input_tokens"].as_u64(),
                    output_tokens: usage["output_tokens"].as_u64(),
                });
            Ok(vec![
                CompletionEvent::ProviderMetadata(ProviderTurnMetadata {
                    provider: provider.to_owned(),
                    url: url.to_owned(),
                    model: model.to_owned(),
                    usage,
                    resume: json!({ "response_id": response["id"] }),
                }),
                CompletionEvent::Finished,
            ])
        }
        Some("error") => Err(api_error(
            RESPONSES_NAME,
            value.get("error").unwrap_or(value),
        )),
        _ => Ok(Vec::new()),
    }
}

fn openai_thinking_block(item: &Value) -> ThinkingBlock {
    ThinkingBlock {
        kind: ThinkingBlockKind::OpenAiResponses,
        text: openai_reasoning_text(item),
        payload: item.clone(),
    }
}

fn openai_reasoning_text(item: &Value) -> String {
    item["summary"]
        .as_array()
        .map(|summary| {
            summary
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

fn partial_tool_call_from_response_item(item: &Value) -> PartialToolCall {
    PartialToolCall {
        id: item["call_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .unwrap_or_default()
            .to_owned(),
        name: item["name"].as_str().unwrap_or_default().to_owned(),
        arguments: item["arguments"].as_str().unwrap_or_default().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_completed_emits_provider_metadata_then_finished() {
        let mut parser = ResponsesParser::new(
            RESPONSES_NAME,
            "https://example.test/v1/responses",
            "gpt-test",
        );

        let events = parser
            .parse_event(
                r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            )
            .expect("completed event parses");

        assert_eq!(
            events,
            vec![
                CompletionEvent::ProviderMetadata(ProviderTurnMetadata {
                    provider: RESPONSES_NAME.to_owned(),
                    url: "https://example.test/v1/responses".to_owned(),
                    model: "gpt-test".to_owned(),
                    usage: Some(TokenUsage {
                        input_tokens: Some(5),
                        output_tokens: Some(2),
                    }),
                    resume: json!({ "response_id": "resp_1" }),
                }),
                CompletionEvent::Finished,
            ]
        );
    }

    #[test]
    fn response_completed_without_usage_omits_token_usage() {
        let mut parser = ResponsesParser::new(
            RESPONSES_NAME,
            "https://example.test/v1/responses",
            "gpt-test",
        );

        let events = parser
            .parse_event(r#"{"type":"response.completed","response":{"id":"resp_2"}}"#)
            .expect("completed event parses");

        let CompletionEvent::ProviderMetadata(metadata) = &events[0] else {
            panic!("expected provider metadata, got {events:?}");
        };
        assert_eq!(metadata.provider, RESPONSES_NAME);
        assert_eq!(metadata.url, "https://example.test/v1/responses");
        assert_eq!(metadata.model, "gpt-test");
        assert_eq!(metadata.usage, None);
        assert_eq!(metadata.resume, json!({ "response_id": "resp_2" }));
    }
}
