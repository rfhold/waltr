//! `OpenAI` Responses request encoding.

use serde_json::{Value, json};

use crate::provider::openai::reasoning::{OpenAiReasoningConfig, openai_reasoning};
use crate::stream::{CompletionRequest, ThinkingBlockKind, ToolCall, ToolDefinition, ToolResult};

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
    });
    if let Some(reasoning) = reasoning {
        body["reasoning"] = openai_reasoning(reasoning);
        body["include"] = json!(["reasoning.encrypted_content"]);
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

    input.extend(request.tool_calls.iter().map(responses_tool_call));
    input.extend(
        request
            .thinking_blocks
            .iter()
            .filter(|block| block.kind == ThinkingBlockKind::OpenAiResponses)
            .map(|block| block.payload.clone()),
    );
    input.extend(request.tool_results.iter().map(responses_tool_result));
    Value::Array(input)
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

    use crate::{ThinkingBlock, ThinkingBlockKind, ToolCall, ToolResult};

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
    fn responses_input_replays_reasoning_before_tool_outputs() {
        let mut request = CompletionRequest::new("hello");
        request
            .tool_calls
            .push(ToolCall::new("call_1", "lookup", json!({})));
        request.thinking_blocks.push(ThinkingBlock {
            kind: ThinkingBlockKind::OpenAiResponses,
            text: String::new(),
            payload: json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "abc" }),
        });
        request
            .tool_results
            .push(ToolResult::new("call_1", "lookup", "ok"));

        let input = responses_input(&request);

        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "reasoning");
        assert_eq!(input[3]["type"], "function_call_output");
    }
}
