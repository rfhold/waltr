//! `OpenAI` Responses parser.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::stream::{CompletionEvent, ThinkingBlock, ThinkingBlockKind, ThinkingDelta};
use crate::{ProviderTurnMetadata, Result, TokenUsage};

use crate::provider::openai::shared::{PartialToolCall, RESPONSES_NAME, api_error};

#[derive(Debug)]
pub(in crate::provider::openai) struct ResponsesParser {
    provider: &'static str,
    url: String,
    model: String,
    tool_calls: HashMap<String, PartialToolCall>,
    tool_call_aliases: HashMap<String, String>,
    reasoning_item: Option<Value>,
    streamed_reasoning_summaries: HashSet<String>,
}

impl ResponsesParser {
    pub(in crate::provider::openai) fn new(
        provider: &'static str,
        url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            url: url.into(),
            model: model.into(),
            tool_calls: HashMap::new(),
            tool_call_aliases: HashMap::new(),
            reasoning_item: None,
            streamed_reasoning_summaries: HashSet::new(),
        }
    }

    pub(in crate::provider::openai) fn parse_event(
        &mut self,
        data: &str,
    ) -> Result<Vec<CompletionEvent>> {
        let value: Value = serde_json::from_str(data)?;
        self.map_event(&value)
    }

    fn map_event(&mut self, value: &Value) -> Result<Vec<CompletionEvent>> {
        match value["type"].as_str() {
            Some("response.output_text.delta") => Ok(value["delta"]
                .as_str()
                .map(|delta| vec![CompletionEvent::Delta(delta.to_owned())])
                .unwrap_or_default()),
            Some("response.output_item.added") => {
                self.output_item_added(value);
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
            Some("response.reasoning_summary_text.delta") => {
                Ok(self.reasoning_summary_delta(value))
            }
            Some("response.reasoning_summary_text.done") => Ok(self.reasoning_summary_done(value)),
            Some("response.function_call_arguments.delta") => {
                let delta = value["delta"].as_str().unwrap_or_default();
                if let Some(key) = self.resolve_tool_call_key(value)
                    && let Some(call) = self.tool_calls.get_mut(&key)
                {
                    call.arguments.push_str(delta);
                }
                Ok(Vec::new())
            }
            Some("response.function_call_arguments.done") => self.function_call_done(value),
            Some("response.completed") => Ok(self.response_completed(&value["response"])),
            Some("error") => Err(api_error(
                RESPONSES_NAME,
                value.get("error").unwrap_or(value),
            )),
            _ => Ok(Vec::new()),
        }
    }

    fn output_item_added(&mut self, value: &Value) {
        let item = &value["item"];
        if item["type"].as_str() == Some("function_call") {
            if let Some(key) = response_item_key(item).or_else(|| tool_call_key(value)) {
                if let Some(alias) = tool_call_key(value)
                    && alias != key
                {
                    self.tool_call_aliases.insert(alias, key.clone());
                }
                self.tool_calls
                    .insert(key, partial_tool_call_from_response_item(item));
            }
        } else if item["type"].as_str() == Some("reasoning") {
            self.reasoning_item = Some(item.clone());
        }
    }

    fn reasoning_summary_done(&mut self, value: &Value) -> Vec<CompletionEvent> {
        value["text"]
            .as_str()
            .map(|text| {
                if let Some(item) = self.reasoning_item.as_mut() {
                    item["summary"] = json!([{ "type": "summary_text", "text": text }]);
                }
                if self
                    .streamed_reasoning_summaries
                    .contains(&reasoning_summary_key(value))
                {
                    return Vec::new();
                }
                vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default()
    }

    fn reasoning_summary_delta(&mut self, value: &Value) -> Vec<CompletionEvent> {
        value["delta"]
            .as_str()
            .map(|text| {
                if !text.is_empty() {
                    self.streamed_reasoning_summaries
                        .insert(reasoning_summary_key(value));
                }
                vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default()
    }

    fn function_call_done(&mut self, value: &Value) -> Result<Vec<CompletionEvent>> {
        let Some(key) = self.resolve_tool_call_key(value) else {
            return Ok(Vec::new());
        };
        if let Some(arguments) = value["arguments"].as_str()
            && let Some(call) = self.tool_calls.get_mut(&key)
        {
            arguments.clone_into(&mut call.arguments);
        }
        let event = self
            .tool_calls
            .remove(&key)
            .map(PartialToolCall::into_event)
            .transpose()?
            .map(|event| vec![event])
            .unwrap_or_default();
        self.tool_call_aliases.retain(|_, primary| primary != &key);
        Ok(event)
    }

    fn response_completed(&self, response: &Value) -> Vec<CompletionEvent> {
        let usage = response
            .get("usage")
            .filter(|usage| usage.is_object())
            .map(|usage| TokenUsage {
                input_tokens: usage["input_tokens"].as_u64(),
                output_tokens: usage["output_tokens"].as_u64(),
            });
        vec![
            CompletionEvent::ProviderMetadata(ProviderTurnMetadata {
                provider: self.provider.to_owned(),
                url: self.url.clone(),
                model: self.model.clone(),
                usage,
                resume: json!({ "response_id": response["id"] }),
            }),
            CompletionEvent::Finished,
        ]
    }

    fn resolve_tool_call_key(&self, value: &Value) -> Option<String> {
        let key = tool_call_key(value)?;
        self.tool_call_aliases.get(&key).cloned().or(Some(key))
    }
}

fn response_item_key(item: &Value) -> Option<String> {
    item["id"]
        .as_str()
        .or_else(|| item["call_id"].as_str())
        .map(str::to_owned)
}

fn tool_call_key(value: &Value) -> Option<String> {
    value["item_id"].as_str().map(str::to_owned).or_else(|| {
        value["output_index"]
            .as_u64()
            .map(|index| index.to_string())
    })
}

fn reasoning_summary_key(value: &Value) -> String {
    let item_key = tool_call_key(value).unwrap_or_else(|| "unknown".to_owned());
    let summary_index = value["summary_index"].as_u64().unwrap_or_default();
    format!("{item_key}:{summary_index}")
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

    #[test]
    fn reasoning_summary_done_without_delta_emits_visible_thinking() {
        let mut parser = ResponsesParser::new(
            RESPONSES_NAME,
            "https://example.test/v1/responses",
            "gpt-test",
        );

        let events = parser
            .parse_event(
                r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"**PLAN** inspect"}"#,
            )
            .expect("reasoning summary done parses");

        assert_eq!(
            events,
            vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "**PLAN** inspect".to_owned(),
            })]
        );
    }

    #[test]
    fn reasoning_summary_done_after_delta_does_not_duplicate_visible_thinking() {
        let mut parser = ResponsesParser::new(
            RESPONSES_NAME,
            "https://example.test/v1/responses",
            "gpt-test",
        );

        let first = parser
            .parse_event(
                r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"**PLAN** "}"#,
            )
            .expect("first reasoning delta parses");
        let second = parser
            .parse_event(
                r#"{"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"inspect"}"#,
            )
            .expect("second reasoning delta parses");
        let done = parser
            .parse_event(
                r#"{"type":"response.reasoning_summary_text.done","item_id":"rs_1","summary_index":0,"text":"**PLAN** inspect"}"#,
            )
            .expect("reasoning summary done parses");

        assert_eq!(
            first,
            vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "**PLAN** ".to_owned(),
            })]
        );
        assert_eq!(
            second,
            vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                text: "inspect".to_owned(),
            })]
        );
        assert!(done.is_empty());
    }

    #[test]
    fn parses_multiple_function_calls_by_item_identity() {
        let mut parser = ResponsesParser::new(
            RESPONSES_NAME,
            "https://example.test/v1/responses",
            "gpt-test",
        );

        assert!(
            parser
                .parse_event(
                    r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item_1","call_id":"call-1","name":"first","arguments":""}}"#,
                )
                .expect("first item parses")
                .is_empty()
        );
        assert!(
            parser
                .parse_event(
                    r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item_2","call_id":"call-2","name":"second","arguments":""}}"#,
                )
                .expect("second item parses")
                .is_empty()
        );
        assert!(
            parser
                .parse_event(
                    r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"a\":"}"#,
                )
                .expect("first delta parses")
                .is_empty()
        );
        assert!(
            parser
                .parse_event(
                    r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"b\":"}"#,
                )
                .expect("second delta parses")
                .is_empty()
        );

        let first = parser
            .parse_event(
                r#"{"type":"response.function_call_arguments.done","item_id":"item_1","arguments":"{\"a\":1}"}"#,
            )
            .expect("first done parses");
        let second = parser
            .parse_event(
                r#"{"type":"response.function_call_arguments.done","item_id":"item_2","arguments":"{\"b\":2}"}"#,
            )
            .expect("second done parses");

        let [CompletionEvent::ToolCall(first)] = first.as_slice() else {
            panic!("expected first tool call, got {first:?}");
        };
        let [CompletionEvent::ToolCall(second)] = second.as_slice() else {
            panic!("expected second tool call, got {second:?}");
        };
        assert_eq!(first.id, "call-1");
        assert_eq!(first.name, "first");
        assert_eq!(first.arguments, json!({ "a": 1 }));
        assert_eq!(second.id, "call-2");
        assert_eq!(second.name, "second");
        assert_eq!(second.arguments, json!({ "b": 2 }));
    }
}
