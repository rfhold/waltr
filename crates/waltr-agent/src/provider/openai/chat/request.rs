//! `OpenAI` Chat Completions request encoding.

use serde_json::{Value, json};

use crate::stream::{CompletionRequest, ToolCall, ToolDefinition, ToolResult, TurnItem};

pub(super) fn chat_body(model: &str, request: &CompletionRequest<'_>) -> Value {
    json!({
        "model": model,
        "stream": true,
        "store": false,
        "stream_options": { "include_usage": true },
        "messages": chat_messages(request),
        "tools": chat_tools(&request.tools),
    })
}

pub(super) fn chat_messages(request: &CompletionRequest<'_>) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.push(json!({ "role": "user", "content": request.prompt }));

    let mut pending_calls: Vec<Value> = Vec::new();
    for item in request.turns.iter().flat_map(|turn| turn.items.iter()) {
        match item {
            TurnItem::Thinking(_) => {}
            TurnItem::ToolCall(call) => pending_calls.push(chat_tool_call(call)),
            TurnItem::ToolResult(result) => {
                flush_assistant_tool_calls(&mut messages, &mut pending_calls);
                messages.push(chat_tool_result(result));
            }
        }
    }
    flush_assistant_tool_calls(&mut messages, &mut pending_calls);
    messages
}

fn flush_assistant_tool_calls(messages: &mut Vec<Value>, calls: &mut Vec<Value>) {
    if !calls.is_empty() {
        messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": std::mem::take(calls),
        }));
    }
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
    use serde_json::json;

    use crate::{CompletionRequest, CompletionTurn, ToolCall, ToolResult, TurnItem};

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

    #[test]
    fn chat_messages_replay_turns_as_alternating_assistant_and_tool_messages() {
        let mut request = CompletionRequest::new("hello");
        request.turns = vec![
            CompletionTurn {
                turn: 0,
                metadata: None,
                items: vec![
                    TurnItem::ToolCall(ToolCall::new("call-0", "echo", json!({ "value": "a" }))),
                    TurnItem::ToolResult(ToolResult::new("call-0", "echo", "a")),
                ],
            },
            CompletionTurn {
                turn: 1,
                metadata: None,
                items: vec![
                    TurnItem::ToolCall(ToolCall::new("call-1", "echo", json!({ "value": "b" }))),
                    TurnItem::ToolResult(ToolResult::new("call-1", "echo", "b")),
                ],
            },
        ];

        let messages = chat_messages(&request);

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call-0");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-0");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["tool_calls"][0]["id"], "call-1");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call-1");
    }
}
