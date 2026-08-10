//! `OpenAI` Chat Completions request encoding.

use serde_json::{Value, json};

use crate::stream::{
    CompletionReplayItem, CompletionRequest, ToolCall, ToolDefinition, ToolResult, TurnItem,
};

use super::super::cache::prompt_cache_key;

pub(super) fn chat_body(
    model: &str,
    prompt_cache: bool,
    prompt_cache_retention: Option<&str>,
    request: &CompletionRequest<'_>,
) -> Value {
    let mut body = json!({
        "model": model,
        "stream": true,
        "store": false,
        "stream_options": { "include_usage": true },
        "messages": chat_messages(request),
        "tools": chat_tools(&request.tools),
    });
    if prompt_cache {
        body["prompt_cache_key"] = json!(prompt_cache_key(model, request));
        if let Some(retention) = prompt_cache_retention {
            body["prompt_cache_retention"] = json!(retention);
        }
    }
    body
}

pub(super) fn chat_messages(request: &CompletionRequest<'_>) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    let mut pending_text = String::new();
    let mut pending_calls: Vec<Value> = Vec::new();
    let mut pending_images: Vec<Value> = Vec::new();
    for item in request.replay_items() {
        match item {
            CompletionReplayItem::Prompt(prompt) => {
                flush_assistant_message(&mut messages, &mut pending_text, &mut pending_calls);
                flush_tool_images(&mut messages, &mut pending_images);
                messages.push(json!({ "role": "user", "content": prompt }));
            }
            CompletionReplayItem::Turn(item) => match item {
                TurnItem::AssistantText(text) => {
                    flush_tool_images(&mut messages, &mut pending_images);
                    pending_text.push_str(text);
                }
                TurnItem::Thinking(_) => flush_tool_images(&mut messages, &mut pending_images),
                TurnItem::ToolCall(call) => {
                    flush_tool_images(&mut messages, &mut pending_images);
                    pending_calls.push(chat_tool_call(call));
                }
                TurnItem::ToolResult(result) => {
                    flush_assistant_message(&mut messages, &mut pending_text, &mut pending_calls);
                    messages.push(chat_tool_result(result));
                    append_chat_tool_images(&mut pending_images, result);
                }
            },
        }
    }
    flush_assistant_message(&mut messages, &mut pending_text, &mut pending_calls);
    flush_tool_images(&mut messages, &mut pending_images);
    messages
}

fn flush_assistant_message(messages: &mut Vec<Value>, text: &mut String, calls: &mut Vec<Value>) {
    if text.is_empty() && calls.is_empty() {
        return;
    }

    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(std::mem::take(text)) },
    });
    if !calls.is_empty() {
        message["tool_calls"] = json!(std::mem::take(calls));
    }
    messages.push(message);
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
        "content": result.text_projection(),
    })
}

fn append_chat_tool_images(content: &mut Vec<Value>, result: &ToolResult) {
    let mut labeled = false;
    for image in result.content.images() {
        if !labeled {
            content.push(json!({
                "type": "text",
                "text": format!(
                    "Images returned by tool `{}` for call `{}`:",
                    result.name, result.call_id
                ),
            }));
            labeled = true;
        }
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", image.media_type(), image.data()),
            }
        }));
    }
}

fn flush_tool_images(messages: &mut Vec<Value>, content: &mut Vec<Value>) {
    if !content.is_empty() {
        messages.push(json!({ "role": "user", "content": std::mem::take(content) }));
    }
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

    use crate::{
        CompletionRequest, CompletionTurn, PromptTurn, ToolCall, ToolResult, ToolResultContent,
        ToolResultContentBlock, ToolResultImage, TurnItem,
    };

    use super::{chat_body, chat_messages};

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
    fn chat_body_includes_prompt_cache_key_by_default() {
        let request = CompletionRequest::new("hello");

        let body = chat_body("gpt-test", true, Some("24h"), &request);

        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| key.starts_with("waltr-"))
        );
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn chat_body_can_disable_prompt_cache_key() {
        let request = CompletionRequest::new("hello");

        let body = chat_body("gpt-test", false, Some("24h"), &request);

        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn chat_messages_replay_turns_as_alternating_assistant_and_tool_messages() {
        let mut request = CompletionRequest::new("follow up");
        request.prompt_turns = vec![
            PromptTurn {
                turn: 0,
                prompt: "hello".to_owned(),
            },
            PromptTurn {
                turn: 2,
                prompt: "follow up".to_owned(),
            },
        ];
        request.turns = vec![
            CompletionTurn {
                turn: 0,
                metadata: None,
                items: vec![
                    TurnItem::AssistantText("first answer".to_owned()),
                    TurnItem::ToolCall(ToolCall::new("call-0", "echo", json!({ "value": "a" }))),
                    TurnItem::ToolResult(ToolResult::new("call-0", "echo", "a")),
                ],
            },
            CompletionTurn {
                turn: 1,
                metadata: None,
                items: vec![
                    TurnItem::AssistantText("second answer".to_owned()),
                    TurnItem::ToolCall(ToolCall::new("call-1", "echo", json!({ "value": "b" }))),
                    TurnItem::ToolResult(ToolResult::new("call-1", "echo", "b")),
                ],
            },
        ];

        let messages = chat_messages(&request);

        assert_eq!(messages.len(), 6);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hello");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "first answer");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call-0");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-0");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["content"], "second answer");
        assert_eq!(messages[3]["tool_calls"][0]["id"], "call-1");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call-1");
        assert_eq!(messages[5]["role"], "user");
        assert_eq!(messages[5]["content"], "follow up");
    }

    #[test]
    fn chat_messages_emit_tool_results_before_labeled_image_messages() {
        let mut request = CompletionRequest::new("look");
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![
                TurnItem::ToolCall(ToolCall::new("call-image", "camera", json!({}))),
                TurnItem::ToolResult(image_result("call-image")),
            ],
        }];

        let messages = chat_messages(&request);

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-image");
        assert!(!messages[2]["content"].as_str().unwrap().contains("iVBOR"));
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"][0]["type"], "text");
        assert!(
            messages[3]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("call-image")
        );
        assert_eq!(messages[3]["content"][1]["type"], "image_url");
        assert_eq!(
            messages[3]["content"][1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn chat_messages_complete_all_tool_results_before_batched_images() {
        let mut request = CompletionRequest::new("look");
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![
                TurnItem::ToolCall(ToolCall::new("call-a", "camera", json!({}))),
                TurnItem::ToolCall(ToolCall::new("call-b", "camera", json!({}))),
                TurnItem::ToolResult(image_result("call-a")),
                TurnItem::ToolResult(image_result("call-b")),
            ],
        }];

        let messages = chat_messages(&request);

        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-a");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-b");
        assert_eq!(messages[4]["role"], "user");
        assert_eq!(messages[4]["content"].as_array().unwrap().len(), 4);
    }

    fn image_result(call_id: &str) -> ToolResult {
        ToolResult::from_content(
            call_id,
            "camera",
            ToolResultContent::from_blocks(vec![
                ToolResultContentBlock::Text {
                    text: "snapshot".to_owned(),
                },
                ToolResultContentBlock::Image(
                    ToolResultImage::new("iVBORw0KGgo=", "image/png").expect("valid image"),
                ),
            ])
            .expect("bounded content"),
            false,
        )
    }
}
