//! `OpenAI` Chat Completions streaming provider.

mod parser;
mod request;

use serde_json::{Value, json};

use crate::Result;
use crate::stream::{CompletionEvent, CompletionRequest, LlmStream, ToolCall, ToolResult};

use super::{CHAT_COMPLETIONS_NAME, PartialToolCall, api_error, chat_tools, stream_openai_http};

/// `OpenAI` Chat Completions HTTP streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiChatCompletions {
    api_key: String,
    model: String,
    completions_url: String,
}

impl OpenAiChatCompletions {
    /// Create an `OpenAI` Chat Completions provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            completions_url: "https://api.openai.com/v1/chat/completions".to_owned(),
        }
    }

    /// Set the API base URL used by this provider.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        self.completions_url = format!(
            "{}/v1/chat/completions",
            base_url.as_ref().trim_end_matches('/')
        );
        self
    }
}

impl LlmStream for OpenAiChatCompletions {
    fn name(&self) -> &'static str {
        CHAT_COMPLETIONS_NAME
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let body = json!({
                "model": self.model,
                "stream": true,
                "store": false,
                "messages": chat_messages(&request),
                "tools": chat_tools(&request.tools),
            });
            let mut parser = ChatParser::default();

            stream_openai_http(
                CHAT_COMPLETIONS_NAME,
                &self.completions_url,
                &self.api_key,
                body,
                move |data| parser.parse_event(data),
            )
            .await
        })
    }
}

fn chat_messages(request: &CompletionRequest<'_>) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.push(json!({ "role": "user", "content": request.prompt }));
    if request.tool_calls.is_empty() {
        return messages;
    }

    messages.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": request
            .tool_calls
            .iter()
            .map(chat_tool_call)
            .collect::<Vec<_>>()
    }));

    messages.extend(request.tool_results.iter().map(chat_tool_result));
    messages
}

fn chat_tool_call(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": call.arguments.to_string(),
        }
    })
}

fn chat_tool_result(result: &ToolResult) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": result.call_id,
        "content": result.content,
    })
}

#[derive(Debug, Default)]
struct ChatParser {
    tool_calls: Vec<PartialToolCall>,
}

impl ChatParser {
    fn parse_event(&mut self, data: &str) -> Result<Vec<CompletionEvent>> {
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

#[cfg(test)]
mod tests {
    use crate::CompletionRequest;

    use super::chat_messages;

    #[test]
    fn chat_messages_prepend_system_prompt() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let messages = chat_messages(&request);

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "system text");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hello");
    }
}
