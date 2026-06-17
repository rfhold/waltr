use serde_json::{Value, json};

use crate::{CompletionRequest, ThinkingBlockKind, ToolDefinition, TurnItem};

use super::thinking::{AnthropicThinkingConfig, anthropic_thinking};

pub(super) fn messages_request(
    model: &str,
    max_tokens: u32,
    thinking: Option<&AnthropicThinkingConfig>,
    request: &CompletionRequest<'_>,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": anthropic_messages(request),
        "tools": anthropic_tools(&request.tools),
    });
    if let Some(system_prompt) = request.system_prompt {
        body["system"] = json!(system_prompt);
    }
    if let Some(thinking) = thinking {
        body["thinking"] = anthropic_thinking(thinking);
    }
    body
}

fn anthropic_messages(request: &CompletionRequest<'_>) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "user", "content": request.prompt })];
    let mut assistant_content: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    for item in request.turns.iter().flat_map(|turn| turn.items.iter()) {
        match item {
            TurnItem::AssistantText(text) => {
                flush_tool_results(&mut messages, &mut tool_results);
                assistant_content.push(json!({ "type": "text", "text": text }));
            }
            TurnItem::Thinking(block) if block.kind == ThinkingBlockKind::Anthropic => {
                flush_tool_results(&mut messages, &mut tool_results);
                assistant_content.push(block.payload.clone());
            }
            TurnItem::Thinking(_) => {}
            TurnItem::ToolCall(call) => {
                flush_tool_results(&mut messages, &mut tool_results);
                assistant_content.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            TurnItem::ToolResult(result) => {
                flush_assistant_content(&mut messages, &mut assistant_content);
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": result.content,
                }));
            }
        }
    }
    flush_assistant_content(&mut messages, &mut assistant_content);
    flush_tool_results(&mut messages, &mut tool_results);
    messages
}

fn flush_assistant_content(messages: &mut Vec<Value>, content: &mut Vec<Value>) {
    if !content.is_empty() {
        messages.push(json!({ "role": "assistant", "content": std::mem::take(content) }));
    }
}

fn flush_tool_results(messages: &mut Vec<Value>, results: &mut Vec<Value>) {
    if !results.is_empty() {
        messages.push(json!({ "role": "user", "content": std::mem::take(results) }));
    }
}

fn anthropic_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        CompletionRequest, CompletionTurn, ThinkingBlock, ThinkingBlockKind, ToolCall, ToolResult,
        TurnItem,
    };

    use super::{AnthropicThinkingConfig, messages_request};
    use crate::provider::anthropic::AnthropicThinkingDisplay;

    #[test]
    fn request_includes_system_prompt_when_set() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = messages_request("claude-test", 256, None, &request);

        assert_eq!(body["system"], "system text");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn request_includes_thinking_when_configured() {
        let request = CompletionRequest::new("hello");

        let body = messages_request(
            "claude-test",
            256,
            Some(&AnthropicThinkingConfig::Enabled {
                budget_tokens: 1024,
                display: Some(AnthropicThinkingDisplay::Summarized),
            }),
            &request,
        );

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["thinking"]["display"], "summarized");
    }

    #[test]
    fn request_replays_turns_as_alternating_assistant_and_user_messages() {
        let mut request = CompletionRequest::new("hello");
        request.turns = vec![
            CompletionTurn {
                turn: 0,
                metadata: None,
                items: vec![
                    TurnItem::Thinking(ThinkingBlock {
                        kind: ThinkingBlockKind::Anthropic,
                        text: "first thought".to_owned(),
                        payload: json!({
                            "type": "thinking",
                            "thinking": "first thought",
                            "signature": "sig-0",
                        }),
                    }),
                    TurnItem::AssistantText("first answer".to_owned()),
                    TurnItem::ToolCall(ToolCall::new("call-0", "echo", json!({ "value": "a" }))),
                    TurnItem::ToolResult(ToolResult::new("call-0", "echo", "a")),
                ],
            },
            CompletionTurn {
                turn: 1,
                metadata: None,
                items: vec![
                    TurnItem::Thinking(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: "foreign".to_owned(),
                        payload: json!({ "type": "reasoning", "id": "rs-1" }),
                    }),
                    TurnItem::Thinking(ThinkingBlock {
                        kind: ThinkingBlockKind::Anthropic,
                        text: "second thought".to_owned(),
                        payload: json!({
                            "type": "thinking",
                            "thinking": "second thought",
                            "signature": "sig-1",
                        }),
                    }),
                    TurnItem::AssistantText("second answer".to_owned()),
                    TurnItem::ToolCall(ToolCall::new("call-1", "echo", json!({ "value": "b" }))),
                    TurnItem::ToolResult(ToolResult::new("call-1", "echo", "b")),
                ],
            },
        ];

        let body = messages_request("claude-test", 256, None, &request);
        let messages = body["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hello");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(messages[1]["content"][0]["signature"], "sig-0");
        assert_eq!(messages[1]["content"][1]["type"], "text");
        assert_eq!(messages[1]["content"][1]["text"], "first answer");
        assert_eq!(messages[1]["content"][2]["type"], "tool_use");
        assert_eq!(messages[1]["content"][2]["id"], "call-0");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call-0");
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["content"][0]["signature"], "sig-1");
        assert_eq!(messages[3]["content"][1]["text"], "second answer");
        assert_eq!(messages[3]["content"][2]["id"], "call-1");
        assert_eq!(messages[4]["role"], "user");
        assert_eq!(messages[4]["content"][0]["tool_use_id"], "call-1");
    }
}
