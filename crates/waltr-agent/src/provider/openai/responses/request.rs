//! `OpenAI` Responses request encoding.

use serde_json::{Value, json};

use crate::provider::openai::reasoning::{OpenAiReasoningConfig, openai_reasoning};
use crate::stream::{
    CompletionRequest, ThinkingBlockKind, ToolCall, ToolDefinition, ToolResult, TurnItem,
};

pub(super) fn responses_body(
    model: &str,
    reasoning: Option<&OpenAiReasoningConfig>,
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
    body
}

pub(super) fn responses_input(request: &CompletionRequest<'_>) -> Value {
    let mut input = Vec::new();
    if let Some(system_prompt) = request.system_prompt {
        input.push(json!({
            "type": "message",
            "role": "system",
            "content": [{ "type": "input_text", "text": system_prompt }]
        }));
    }
    input.push(json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": request.prompt }]
    }));

    for item in request.turns.iter().flat_map(|turn| turn.items.iter()) {
        match item {
            TurnItem::Thinking(block)
                if block.kind == ThinkingBlockKind::OpenAiResponses
                    && has_encrypted_content(&block.payload) =>
            {
                input.push(block.payload.clone());
            }
            TurnItem::Thinking(_) => {}
            TurnItem::ToolCall(call) => input.push(responses_tool_call(call)),
            TurnItem::ToolResult(result) => input.push(responses_tool_result(result)),
        }
    }
    Value::Array(input)
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
        "output": result.content,
    })
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

    use crate::{CompletionTurn, ThinkingBlock, ThinkingBlockKind, ToolCall, ToolResult, TurnItem};

    use super::{responses_body, responses_input};
    use crate::provider::openai::{
        OpenAiReasoningConfig, OpenAiReasoningEffort, OpenAiReasoningSummary,
    };

    #[test]
    fn responses_input_prepends_system_prompt() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let input = responses_input(&request);

        assert_eq!(input[0]["role"], "system");
        assert_eq!(input[0]["content"][0]["text"], "system text");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["text"], "hello");
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

        let body = responses_body("gpt-test", None, &request);

        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("reasoning").is_none());
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

        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input.as_array().expect("input array").len(), 3);
    }

    #[test]
    fn responses_input_replays_turn_items_in_emission_order() {
        let mut request = CompletionRequest::new("hello");
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
        assert_eq!(input.as_array().expect("input array").len(), 7);
    }
}
