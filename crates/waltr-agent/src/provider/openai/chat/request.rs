//! `OpenAI` Chat Completions request encoding.

use serde_json::{Value, json};

use crate::stream::{CompletionRequest, ToolCall, ToolDefinition, ToolResult};

pub(super) fn chat_messages(request: &CompletionRequest<'_>) -> Vec<Value> {
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

pub(super) fn chat_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            })
        })
        .collect()
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
