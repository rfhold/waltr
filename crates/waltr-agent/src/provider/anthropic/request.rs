use serde_json::{Value, json};

use crate::{CompletionRequest, ThinkingBlockKind, ToolDefinition};

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
    if request.tool_calls.is_empty() {
        return messages;
    }

    let mut assistant_content = request
        .thinking_blocks
        .iter()
        .filter(|block| block.kind == ThinkingBlockKind::Anthropic)
        .map(|block| block.payload.clone())
        .collect::<Vec<_>>();
    assistant_content.extend(request.tool_calls.iter().map(|call| {
        json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.arguments,
        })
    }));
    messages.push(json!({
        "role": "assistant",
        "content": assistant_content
    }));
    messages.push(json!({
        "role": "user",
        "content": request
            .tool_results
            .iter()
            .map(|result| json!({
                "type": "tool_result",
                "tool_use_id": result.call_id,
                "content": result.content,
            }))
            .collect::<Vec<_>>()
    }));
    messages
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
    use crate::CompletionRequest;

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
}
