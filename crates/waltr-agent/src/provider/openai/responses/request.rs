//! `OpenAI` Responses request encoding.

use serde_json::{Value, json};

use crate::provider::openai::reasoning::{OpenAiReasoningConfig, openai_reasoning};
use crate::stream::{
    CompletionReplayItem, CompletionRequest, ThinkingBlockKind, ToolCall, ToolDefinition,
    ToolResult, ToolResultContentBlock, TurnItem,
};

use super::super::cache::prompt_cache_key;

pub(in crate::provider::openai) fn responses_body(
    model: &str,
    reasoning: Option<&OpenAiReasoningConfig>,
    prompt_cache: bool,
    prompt_cache_retention: Option<&str>,
    request: &CompletionRequest<'_>,
) -> Value {
    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "input": responses_input(request),
        "tools": responses_tools(&request.tools),
        // With store:false, reasoning items can only be replayed when they
        // carry encrypted content, so always ask for it: reasoning models emit
        // reasoning items even without an explicit reasoning config.
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(reasoning) = reasoning {
        body["reasoning"] = openai_reasoning(reasoning);
    }
    if let Some(system_prompt) = request.system_prompt {
        body["instructions"] = json!(system_prompt);
    }
    if prompt_cache {
        body["prompt_cache_key"] = json!(prompt_cache_key(model, request));
        if let Some(retention) = prompt_cache_retention {
            body["prompt_cache_retention"] = json!(retention);
        }
    }
    body
}

pub(super) fn responses_input(request: &CompletionRequest<'_>) -> Value {
    let mut input = Vec::new();

    for item in request.replay_items() {
        match item {
            CompletionReplayItem::Prompt(prompt) => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": prompt }]
            })),
            CompletionReplayItem::Turn(item) => match item {
                TurnItem::AssistantText(text) => input.push(responses_assistant_text(text)),
                TurnItem::Thinking(block)
                    if block.kind == ThinkingBlockKind::OpenAiResponses
                        && has_encrypted_content(&block.payload) =>
                {
                    input.push(block.payload.clone());
                }
                TurnItem::Thinking(_) => {}
                TurnItem::ToolCall(call) => input.push(responses_tool_call(call)),
                TurnItem::ToolResult(result) => input.push(responses_tool_result(result)),
            },
        }
    }
    Value::Array(input)
}

fn responses_assistant_text(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }]
    })
}

/// With store:false the API rejects reasoning items replayed by id alone, so a
/// payload without encrypted content cannot be sent back.
fn has_encrypted_content(payload: &Value) -> bool {
    payload
        .get("encrypted_content")
        .is_some_and(|content| !content.is_null())
}

fn responses_tool_call(call: &ToolCall) -> Value {
    json!({
        "type": "function_call",
        "call_id": call.id,
        "name": call.name,
        "arguments": call.arguments.to_string(),
    })
}

fn responses_tool_result(result: &ToolResult) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": result.call_id,
        "output": responses_tool_result_content(result),
    })
}

fn responses_tool_result_content(result: &ToolResult) -> Value {
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
                    json!({ "type": "input_text", "text": text })
                }
                ToolResultContentBlock::Image(image) => json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{}", image.media_type(), image.data()),
                }),
            })
            .collect(),
    )
}

pub(super) fn responses_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::CompletionRequest;

    use serde_json::json;

    use crate::{
        CompletionTurn, PromptTurn, ThinkingBlock, ThinkingBlockKind, ToolCall, ToolResult,
        ToolResultContent, ToolResultContentBlock, ToolResultImage, TurnItem,
    };

    use super::{responses_body, responses_input};
    use crate::provider::openai::{
        OpenAiReasoningConfig, OpenAiReasoningEffort, OpenAiReasoningSummary,
    };

    #[test]
    fn responses_body_uses_instructions_for_system_prompt() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let body = responses_body("gpt-test", None, true, None, &request);
        let input = &body["input"];

        assert_eq!(body["instructions"], "system text");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(input.as_array().expect("input array").len(), 1);
    }

    #[test]
    fn responses_body_includes_reasoning_config() {
        let request = CompletionRequest::new("hello");

        let body = responses_body(
            "gpt-test",
            Some(&OpenAiReasoningConfig {
                effort: OpenAiReasoningEffort::Low,
                summary: Some(OpenAiReasoningSummary::Auto),
            }),
            true,
            None,
            &request,
        );

        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn responses_body_requests_encrypted_reasoning_without_reasoning_config() {
        let request = CompletionRequest::new("hello");

        let body = responses_body("gpt-test", None, true, None, &request);

        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn responses_body_includes_prompt_cache_key_by_default() {
        let request = CompletionRequest::new("hello");

        let body = responses_body("gpt-test", None, true, Some("24h"), &request);

        assert!(
            body["prompt_cache_key"]
                .as_str()
                .is_some_and(|key| key.starts_with("waltr-"))
        );
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn responses_body_can_disable_prompt_cache_key() {
        let request = CompletionRequest::new("hello");

        let body = responses_body("gpt-test", None, false, Some("24h"), &request);

        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn responses_input_replays_assistant_text_as_output_message() {
        let mut request = CompletionRequest::new("hello");
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![TurnItem::AssistantText("prior answer".to_owned())],
        }];

        let input = responses_input(&request);

        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "prior answer");
        assert_eq!(input.as_array().expect("input array").len(), 2);
    }

    #[test]
    fn responses_input_skips_reasoning_payloads_without_encrypted_content() {
        let mut request = CompletionRequest::new("hello");
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![
                TurnItem::Thinking(ThinkingBlock {
                    kind: ThinkingBlockKind::OpenAiResponses,
                    text: String::new(),
                    payload: json!({ "type": "reasoning", "id": "rs_bare" }),
                }),
                TurnItem::ToolCall(ToolCall::new("call_1", "lookup", json!({}))),
                TurnItem::ToolResult(ToolResult::new("call_1", "lookup", "ok")),
            ],
        }];

        let input = responses_input(&request);

        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input.as_array().expect("input array").len(), 3);
    }

    #[test]
    fn responses_input_replays_turn_items_in_emission_order() {
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
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: String::new(),
                        payload: json!({
                            "type": "reasoning",
                            "id": "rs_1",
                            "encrypted_content": "abc",
                        }),
                    }),
                    TurnItem::ToolCall(ToolCall::new("call_1", "lookup", json!({}))),
                    TurnItem::ToolResult(ToolResult::new("call_1", "lookup", "ok")),
                ],
            },
            CompletionTurn {
                turn: 1,
                metadata: None,
                items: vec![
                    TurnItem::Thinking(ThinkingBlock {
                        kind: ThinkingBlockKind::Anthropic,
                        text: "foreign".to_owned(),
                        payload: json!({ "type": "thinking" }),
                    }),
                    TurnItem::Thinking(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: String::new(),
                        payload: json!({
                            "type": "reasoning",
                            "id": "rs_2",
                            "encrypted_content": "def",
                        }),
                    }),
                    TurnItem::ToolCall(ToolCall::new("call_2", "lookup", json!({}))),
                    TurnItem::ToolResult(ToolResult::new("call_2", "lookup", "ok")),
                ],
            },
        ];

        let input = responses_input(&request);

        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "hello");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["id"], "rs_1");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[4]["type"], "reasoning");
        assert_eq!(input[4]["id"], "rs_2");
        assert_eq!(input[5]["type"], "function_call");
        assert_eq!(input[5]["call_id"], "call_2");
        assert_eq!(input[6]["type"], "function_call_output");
        assert_eq!(input[6]["call_id"], "call_2");
        assert_eq!(input[7]["role"], "user");
        assert_eq!(input[7]["content"][0]["text"], "follow up");
        assert_eq!(input.as_array().expect("input array").len(), 8);
    }

    #[test]
    fn responses_input_encodes_durable_history_and_current_prompt_once_in_tool_order() {
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

        let input = responses_input(&request);
        let items = input.as_array().expect("input array");

        assert_eq!(items.len(), 6);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["text"], "seed prompt");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["text"], "seed answer");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "seed-call");
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "seed-call");
        assert_eq!(items[4]["content"][0]["text"], "current history");
        assert_eq!(items[5]["content"][0]["text"], "current prompt");
        assert_eq!(
            items
                .iter()
                .filter(|item| item["content"][0]["text"] == "seed prompt")
                .count(),
            1
        );
    }

    #[test]
    fn responses_input_encodes_image_tool_results_as_input_content() {
        let mut request = CompletionRequest::new("look");
        request.turns = vec![CompletionTurn {
            turn: 0,
            metadata: None,
            items: vec![
                TurnItem::ToolCall(ToolCall::new("call-image", "camera", json!({}))),
                TurnItem::ToolResult(image_result()),
            ],
        }];

        let input = responses_input(&request);
        let output = &input[2]["output"];

        assert_eq!(
            output[0],
            json!({ "type": "input_text", "text": "snapshot" })
        );
        assert_eq!(output[1]["type"], "input_image");
        assert_eq!(output[1]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
    }

    fn image_result() -> ToolResult {
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
            false,
        )
    }
}
