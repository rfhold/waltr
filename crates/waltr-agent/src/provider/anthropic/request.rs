use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{
    CompletionReplayItem, CompletionRequest, ThinkingBlockKind, ToolDefinition, ToolResult,
    ToolResultContentBlock, TurnItem,
};

use super::thinking::{AnthropicThinkingConfig, anthropic_thinking};

const BILLING_SALT: &str = "59cf53e54c78";

#[derive(Clone, Copy)]
pub(super) struct MessagesRequestOptions<'a> {
    prompt_cache: bool,
    oauth_billing_version: Option<&'a str>,
}

impl<'a> MessagesRequestOptions<'a> {
    pub(super) const fn new(prompt_cache: bool, oauth_billing_version: Option<&'a str>) -> Self {
        Self {
            prompt_cache,
            oauth_billing_version,
        }
    }
}

pub(super) fn messages_request(
    model: &str,
    max_tokens: u32,
    thinking: Option<&AnthropicThinkingConfig>,
    options: MessagesRequestOptions<'_>,
    request: &CompletionRequest<'_>,
) -> Value {
    let MessagesRequestOptions {
        prompt_cache,
        oauth_billing_version,
    } = options;
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
    if let Some(version) = oauth_billing_version {
        prepend_oauth_billing_marker(&mut body, version);
    }
    body
}

fn prepend_oauth_billing_marker(body: &mut Value, version: &str) {
    let marker = billing_marker(first_user_text(&body["messages"]), version);
    let marker = json!({ "type": "text", "text": marker });
    match body.get_mut("system") {
        None => body["system"] = Value::Array(vec![marker]),
        Some(Value::String(system)) => {
            let system = std::mem::take(system);
            body["system"] = Value::Array(vec![marker, json!({ "type": "text", "text": system })]);
        }
        Some(Value::Array(system)) => system.insert(0, marker),
        Some(_) => unreachable!("Anthropic request builder emits only supported system shapes"),
    }
}

fn first_user_text(messages: &Value) -> &str {
    for message in messages.as_array().into_iter().flatten() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        match message.get("content") {
            Some(Value::String(text)) => return text,
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                    {
                        return text;
                    }
                }
            }
            _ => {}
        }
    }
    ""
}

fn billing_marker(first_user_message: &str, version: &str) -> String {
    let code_units = first_user_message.encode_utf16().collect::<Vec<_>>();
    let sampled = [4, 7, 20]
        .into_iter()
        .map(|index| {
            code_units
                .get(index)
                .map_or_else(|| "30".to_owned(), |unit| format!("{unit:x}"))
        })
        .collect::<String>();
    let digest = Sha256::digest(format!("{BILLING_SALT}{sampled}{version}"));
    let hash = format!("{:02x}{:02x}", digest[0], digest[1]);
    format!(
        "x-anthropic-billing-header: cc_version={version}.{}; cc_entrypoint=cli; cch=00000;",
        &hash[..3]
    )
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
                    let mut tool_result = json!({
                        "type": "tool_result",
                        "tool_use_id": result.call_id,
                        "content": anthropic_tool_result_content(result),
                    });
                    if result.is_error {
                        tool_result["is_error"] = json!(true);
                    }
                    tool_results.push(tool_result);
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

fn anthropic_tool_result_content(result: &ToolResult) -> Value {
    if let [ToolResultContentBlock::Text { text }] = result.content.blocks() {
        return json!(text);
    }
    Value::Array(
        result
            .content
            .blocks()
            .iter()
            .map(|block| match block {
                ToolResultContentBlock::Text { text } => {
                    json!({ "type": "text", "text": text })
                }
                ToolResultContentBlock::Image(image) => json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": image.media_type(),
                        "data": image.data(),
                    }
                }),
            })
            .collect(),
    )
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
        ToolResult, ToolResultContent, ToolResultContentBlock, ToolResultImage, TurnItem,
    };

    use super::{
        AnthropicThinkingConfig, MessagesRequestOptions, billing_marker, first_user_text,
        messages_request,
    };
    use crate::provider::anthropic::AnthropicThinkingDisplay;

    #[test]
    fn request_includes_system_prompt_when_set() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(true, None),
            &request,
        );

        assert_eq!(body["system"][0]["text"], "system text");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn request_can_disable_prompt_cache() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, None),
            &request,
        );

        assert_eq!(body["system"], "system text");
    }

    #[test]
    fn non_oauth_request_does_not_add_a_system_marker() {
        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(true, None),
            &CompletionRequest::new("hello"),
        );

        assert!(body.get("system").is_none());
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn oauth_request_prepends_billing_marker_to_each_system_shape() {
        let absent = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, Some("2.1.76")),
            &CompletionRequest::new("Hello from Claude Code"),
        );
        assert_eq!(
            absent["system"],
            json!([{
                "type": "text",
                "text": "x-anthropic-billing-header: cc_version=2.1.76.ad6; cc_entrypoint=cli; cch=00000;"
            }])
        );

        let mut with_string = CompletionRequest::new("Hello from Claude Code");
        with_string.system_prompt = Some("instructions");
        let string = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, Some("2.1.76")),
            &with_string,
        );
        assert_eq!(string["system"][0], absent["system"][0]);
        assert_eq!(
            string["system"][1],
            json!({ "type": "text", "text": "instructions" })
        );

        let cached = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(true, Some("2.1.76")),
            &with_string,
        );
        assert_eq!(cached["system"][0], absent["system"][0]);
        assert_eq!(cached["system"][1]["text"], "instructions");
        assert_eq!(cached["system"][1]["cache_control"]["type"], "ephemeral");

        let alternate_version = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, Some("9.8.7")),
            &CompletionRequest::new("Hello from Claude Code"),
        );
        assert_eq!(
            alternate_version["system"][0]["text"],
            "x-anthropic-billing-header: cc_version=9.8.7.069; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[test]
    fn oauth_billing_marker_samples_utf16_and_first_nonempty_user_text_block() {
        let messages = json!([
            { "role": "assistant", "content": "ignored" },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "" },
                    { "type": "image", "source": {} },
                    { "type": "text", "text": "abcd😀xyZ0123456789uvw" }
                ]
            },
            { "role": "user", "content": "later" }
        ]);

        assert_eq!(first_user_text(&messages), "abcd😀xyZ0123456789uvw");
        assert_eq!(
            billing_marker(first_user_text(&messages), "2.1.76"),
            "x-anthropic-billing-header: cc_version=2.1.76.af9; cc_entrypoint=cli; cch=00000;"
        );
        assert!(billing_marker("", "2.1.76").contains("cc_version=2.1.76.62d;"));
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
            MessagesRequestOptions::new(true, None),
            &request,
        );

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["thinking"]["display"], "summarized");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
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

        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, None),
            &request,
        );
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
    fn request_encodes_durable_history_and_current_prompt_once_in_tool_order() {
        let mut request = CompletionRequest::new("current prompt");
        request.prompt_turns = vec![
            PromptTurn {
                turn: 0,
                prompt: "seed prompt".to_owned(),
            },
            PromptTurn {
                turn: 2,
                prompt: "current prompt".to_owned(),
            },
        ];
        request.turns = vec![
            CompletionTurn {
                turn: 0,
                metadata: None,
                items: vec![
                    TurnItem::AssistantText("seed answer".to_owned()),
                    TurnItem::ToolCall(ToolCall::new("seed-call", "lookup", json!({}))),
                    TurnItem::ToolResult(ToolResult::new("seed-call", "lookup", "seed result")),
                ],
            },
            CompletionTurn {
                turn: 1,
                metadata: None,
                items: vec![TurnItem::AssistantText("current history".to_owned())],
            },
        ];

        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, None),
            &request,
        );
        let messages = body["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 5);
        assert_eq!(
            messages[0],
            json!({ "role": "user", "content": "seed prompt" })
        );
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["text"], "seed answer");
        assert_eq!(messages[1]["content"][1]["id"], "seed-call");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "seed-call");
        assert_eq!(messages[3]["content"][0]["text"], "current history");
        assert_eq!(
            messages[4],
            json!({ "role": "user", "content": "current prompt" })
        );
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

        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(true, None),
            &request,
        );

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

    #[test]
    fn request_encodes_image_tool_results_as_native_anthropic_blocks() {
        let mut request = CompletionRequest::new("look");
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![
                TurnItem::ToolCall(ToolCall::new("call-image", "camera", json!({}))),
                TurnItem::ToolResult(image_result(true)),
            ],
        }];

        let body = messages_request(
            "claude-test",
            256,
            None,
            MessagesRequestOptions::new(false, None),
            &request,
        );
        let tool_result = &body["messages"][2]["content"][0];

        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["is_error"], true);
        assert_eq!(tool_result["content"][0]["type"], "text");
        assert_eq!(tool_result["content"][1]["type"], "image");
        assert_eq!(tool_result["content"][1]["source"]["type"], "base64");
        assert_eq!(
            tool_result["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(tool_result["content"][1]["source"]["data"], "iVBORw0KGgo=");
    }

    fn image_result(is_error: bool) -> ToolResult {
        ToolResult::from_content(
            "call-image",
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
            is_error,
        )
    }
}
