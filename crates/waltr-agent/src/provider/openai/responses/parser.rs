//! `OpenAI` Responses parser.

use serde_json::{Value, json};

use crate::Result;
use crate::stream::{CompletionEvent, ThinkingBlock, ThinkingBlockKind, ThinkingDelta};

use crate::provider::openai::shared::{PartialToolCall, RESPONSES_NAME, api_error};

#[derive(Debug, Default)]
pub(super) struct ResponsesParser {
    tool_call: Option<PartialToolCall>,
    reasoning_item: Option<Value>,
}

impl ResponsesParser {
    pub(super) fn parse_event(&mut self, data: &str) -> Result<Vec<CompletionEvent>> {
        let value: Value = serde_json::from_str(data)?;
        parse_responses_event_value(&value, &mut self.tool_call, &mut self.reasoning_item)
    }
}

fn parse_responses_event_value(
    value: &Value,
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
        Some("response.completed") => Ok(vec![CompletionEvent::Finished]),
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
