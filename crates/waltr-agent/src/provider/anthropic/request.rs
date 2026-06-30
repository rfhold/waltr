use serde_json::{Value, json};

use crate::{CompletionReplayItem, CompletionRequest, ThinkingBlockKind, ToolDefinition, TurnItem};

use super::thinking::{AnthropicThinkingConfig, anthropic_thinking};

pub(super) fn messages_request(
    model: &str,
    max_tokens: u32,
    thinking: Option<&AnthropicThinkingConfig>,
    prompt_cache: bool,
    request: &CompletionRequest<'_>,
) -> Value {
    let static_breakpoint =
        prompt_cache && (!request.tools.is_empty() || request.system_prompt.is_some());
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": anthropic_messages(request, prompt_cache, static_breakpoint),
        "tools": anthropic_tools(&request.tools, prompt_cache),
    });
    if let Some(system_prompt) = request.system_prompt {
        body["system"] = anthropic_system(system_prompt, prompt_cache && request.tools.is_empty());
    }
    if let Some(thinking) = thinking {
        body["thinking"] = anthropic_thinking(thinking);
    }
    body
}

fn anthropic_messages(
    request: &CompletionRequest<'_>,
    prompt_cache: bool,
    static_breakpoint: bool,
) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut assistant_content: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    for item in request.replay_items() {
        match item {
            CompletionReplayItem::Prompt(prompt) => {
                flush_assistant_content(&mut messages, &mut assistant_content);
                if tool_results.is_empty() {
                    messages.push(json!({ "role": "user", "content": prompt }));
                } else {
                    tool_results.push(json!({ "type": "text", "text": prompt }));
                    flush_tool_results(&mut messages, &mut tool_results);
                }
            }
            CompletionReplayItem::Turn(item) => match item {
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
            },
        }
    }
    flush_assistant_content(&mut messages, &mut assistant_content);
    flush_tool_results(&mut messages, &mut tool_results);
    if prompt_cache {
        add_rolling_cache_control(&mut messages, usize::from(static_breakpoint));
    }
    messages
}

fn anthropic_system(system_prompt: &str, prompt_cache: bool) -> Value {
    if prompt_cache {
        return json!([{ "type": "text", "text": system_prompt, "cache_control": cache_control() }]);
    }
    json!(system_prompt)
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

fn anthropic_tools(tools: &[ToolDefinition], prompt_cache: bool) -> Vec<Value> {
    let last_tool = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut definition = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            });
            if prompt_cache && index == last_tool {
                definition["cache_control"] = cache_control();
            }
            definition
        })
        .collect()
}

fn add_rolling_cache_control(messages: &mut [Value], static_breakpoints: usize) {
    let mut remaining = 4usize.saturating_sub(static_breakpoints);
    if remaining == 0 || messages.len() < 2 {
        return;
    }

    let prior_message_count = messages.len() - 1;
    for message in messages[..prior_message_count].iter_mut().rev() {
        if add_cache_control_to_message(message) {
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
    }
}

fn add_cache_control_to_message(message: &mut Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return false;
        }
        *content = json!([{ "type": "text", "text": text, "cache_control": cache_control() }]);
        return true;
    }
    let Some(blocks) = content.as_array_mut() else {
        return false;
    };
    for block in blocks.iter_mut().rev() {
        if is_cacheable_block(block) {
            block["cache_control"] = cache_control();
            return true;
        }
    }
    false
}

fn is_cacheable_block(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some("tool_use" | "tool_result") => true,
        _ => false,
    }
}

fn cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        CompletionRequest, CompletionTurn, PromptTurn, ThinkingBlock, ThinkingBlockKind, ToolCall,
        ToolResult, TurnItem,
    };

    use super::{AnthropicThinkingConfig, messages_request};
    use crate::provider::anthropic::AnthropicThinkingDisplay;

    #[test]
    fn request_includes_system_prompt_when_set() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = messages_request("claude-test", 256, None, true, &request);

        assert_eq!(body["system"][0]["text"], "system text");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn request_can_disable_prompt_cache() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = messages_request("claude-test", 256, None, false, &request);

        assert_eq!(body["system"], "system text");
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
            true,
            &request,
        );

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["thinking"]["display"], "summarized");
    }

    #[test]
    fn request_replays_turns_as_alternating_assistant_and_user_messages() {
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

        let body = messages_request("claude-test", 256, None, false, &request);
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
        assert_eq!(messages[4]["content"][1]["type"], "text");
        assert_eq!(messages[4]["content"][1]["text"], "follow up");
    }

    #[test]
    fn request_marks_tool_and_rolling_messages_for_cache() {
        let mut request = CompletionRequest::new("current");
        request.tools = vec![crate::ToolDefinition::new(
            "lookup",
            "lookup",
            json!({ "type": "object" }),
        )];
        request.prompt_turns = vec![
            PromptTurn {
                turn: 0,
                prompt: "first".to_owned(),
            },
            PromptTurn {
                turn: 1,
                prompt: "current".to_owned(),
            },
        ];
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![TurnItem::AssistantText("answer".to_owned())],
        }];

        let body = messages_request("claude-test", 256, None, true, &request);

        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        let cache_controlled_messages = body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter(|message| message["content"].to_string().contains("cache_control"))
            .count();
        assert_eq!(cache_controlled_messages, 2);
        assert!(
            body["messages"][2]["content"]
                .get("cache_control")
                .is_none()
        );
    }
}
