//! `OpenAI` Chat Completions parser.

use serde_json::Value;

use crate::Result;
use crate::stream::CompletionEvent;

use super::super::shared::{CHAT_COMPLETIONS_NAME, PartialToolCall, api_error};

#[derive(Debug, Default)]
pub(super) struct ChatParser {
    tool_calls: Vec<PartialToolCall>,
}

impl ChatParser {
    pub(super) fn parse_event(&mut self, data: &str) -> Result<Vec<CompletionEvent>> {
        parse_chat_event(data, &mut self.tool_calls)
    }
}

fn parse_chat_event(
    data: &str,
    tool_calls: &mut Vec<PartialToolCall>,
) -> Result<Vec<CompletionEvent>> {
    if data == "[DONE]" {
        return Ok(vec![CompletionEvent::Finished]);
    }

    let value: Value = serde_json::from_str(data)?;
    if let Some(error) = value.get("error") {
        return Err(api_error(CHAT_COMPLETIONS_NAME, error));
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
