//! `OpenAI` streaming providers.

use std::collections::VecDeque;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::stream::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, ThinkingBlock, ThinkingBlockKind,
    ThinkingDelta, ToolCall, ToolDefinition, ToolResult,
};
use crate::{LlmError, Result};

mod chat;
mod reasoning;
mod responses;
mod responses_websocket;
mod shared;

pub use chat::OpenAiChatCompletions;
pub use responses::OpenAiResponses;
pub use responses_websocket::OpenAiResponsesWebSocket;

/// `OpenAI` Responses reasoning configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiReasoningConfig {
    /// Reasoning effort.
    pub effort: OpenAiReasoningEffort,
    /// Optional reasoning summary mode.
    pub summary: Option<OpenAiReasoningSummary>,
}

/// `OpenAI` Responses reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiReasoningEffort {
    /// Disable reasoning.
    None,
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    XHigh,
}

/// `OpenAI` Responses reasoning summary mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiReasoningSummary {
    /// Let `OpenAI` automatically summarize reasoning when supported.
    Auto,
}

const CHAT_COMPLETIONS_NAME: &str = "openai_chat_completions";
const RESPONSES_NAME: &str = "openai_responses";
const RESPONSES_WEBSOCKET_NAME: &str = "openai_responses_websocket";

async fn stream_openai_http(
    provider: &'static str,
    url: &str,
    api_key: &str,
    body: Value,
    parse_event: impl FnMut(&str) -> Result<Vec<CompletionEvent>> + Send + 'static,
) -> Result<BoxCompletionStream> {
    let mut request_builder = reqwest::Client::new().post(url).json(&body);
    if !api_key.is_empty() {
        request_builder = request_builder.bearer_auth(api_key);
    }
    let response = request_builder.send().await?;

    if !response.status().is_success() {
        return Err(LlmError::Api {
            provider,
            message: response.text().await?,
        });
    }

    Ok(Box::pin(stream::unfold(
        (
            response.bytes_stream(),
            SseBuffer::default(),
            VecDeque::<String>::new(),
            VecDeque::<CompletionEvent>::new(),
            parse_event,
            false,
        ),
        move |(
            mut bytes,
            mut sse,
            mut pending_sse,
            mut pending_events,
            mut parse_event,
            finished,
        )| async move {
            if finished {
                return None;
            }

            loop {
                if let Some(event) = pending_events.pop_front() {
                    match event {
                        CompletionEvent::Finished => {
                            return Some((
                                Ok(CompletionEvent::Finished),
                                (bytes, sse, pending_sse, pending_events, parse_event, true),
                            ));
                        }
                        event => {
                            return Some((
                                Ok(event),
                                (bytes, sse, pending_sse, pending_events, parse_event, false),
                            ));
                        }
                    }
                }

                if let Some(event) = pending_sse.pop_front() {
                    match parse_event(&event) {
                        Ok(events) => pending_events.extend(events),
                        Err(error) => {
                            return Some((
                                Err(error),
                                (bytes, sse, pending_sse, pending_events, parse_event, true),
                            ));
                        }
                    }
                    continue;
                }

                match bytes.next().await {
                    Some(Ok(chunk)) => pending_sse.extend(sse.push(chunk.as_ref())),
                    Some(Err(error)) => {
                        return Some((
                            Err(error.into()),
                            (bytes, sse, pending_sse, pending_events, parse_event, true),
                        ));
                    }
                    None => return None,
                }
            }
        },
    )) as BoxCompletionStream)
}

#[derive(Debug, Default)]
struct SseBuffer {
    line: Vec<u8>,
    data: Vec<String>,
}

impl SseBuffer {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();

        for byte in chunk {
            if *byte == b'\n' {
                if self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                self.process_line(&mut events);
            } else {
                self.line.push(*byte);
            }
        }

        events
    }

    fn process_line(&mut self, events: &mut Vec<String>) {
        if self.line.is_empty() {
            if !self.data.is_empty() {
                events.push(self.data.join("\n"));
                self.data.clear();
            }
            return;
        }

        if let Some(data) = self.line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            self.data.push(String::from_utf8_lossy(data).into_owned());
        }

        self.line.clear();
    }
}

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

pub(super) fn openai_reasoning(config: &OpenAiReasoningConfig) -> Value {
    let mut reasoning = json!({
        "effort": match config.effort {
            OpenAiReasoningEffort::None => "none",
            OpenAiReasoningEffort::Minimal => "minimal",
            OpenAiReasoningEffort::Low => "low",
            OpenAiReasoningEffort::Medium => "medium",
            OpenAiReasoningEffort::High => "high",
            OpenAiReasoningEffort::XHigh => "xhigh",
        }
    });
    if let Some(summary) = config.summary {
        reasoning["summary"] = json!(match summary {
            OpenAiReasoningSummary::Auto => "auto",
        });
    }
    reasoning
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

#[derive(Debug, Default)]
pub(super) struct ResponsesParser {
    tool_call: Option<PartialToolCall>,
    reasoning_item: Option<Value>,
}

impl ResponsesParser {
    pub(super) fn parse_event(&mut self, data: &str) -> Result<Vec<CompletionEvent>> {
        let value: Value = serde_json::from_str(data)?;
        parse_responses_event_value(&value, &mut self.tool_call, &mut self.reasoning_item)
    }
}

fn parse_responses_event_value(
    value: &Value,
    tool_call: &mut Option<PartialToolCall>,
    reasoning_item: &mut Option<Value>,
) -> Result<Vec<CompletionEvent>> {
    match value["type"].as_str() {
        Some("response.output_text.delta") => Ok(value["delta"]
            .as_str()
            .map(|delta| vec![CompletionEvent::Delta(delta.to_owned())])
            .unwrap_or_default()),
        Some("response.output_item.added") => {
            let item = &value["item"];
            if item["type"].as_str() == Some("function_call") {
                let call = partial_tool_call_from_response_item(item);
                *tool_call = Some(call);
            } else if item["type"].as_str() == Some("reasoning") {
                *reasoning_item = Some(item.clone());
            }
            Ok(Vec::new())
        }
        Some("response.output_item.done") => {
            let item = &value["item"];
            if item["type"].as_str() == Some("reasoning") {
                return Ok(vec![CompletionEvent::ThinkingBlock(openai_thinking_block(
                    item,
                ))]);
            }
            Ok(Vec::new())
        }
        Some("response.reasoning_summary_text.delta") => Ok(value["delta"]
            .as_str()
            .map(|text| {
                vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default()),
        Some("response.reasoning_summary_text.done") => Ok(value["text"]
            .as_str()
            .map(|text| {
                if let Some(item) = reasoning_item.as_mut() {
                    item["summary"] = json!([{ "type": "summary_text", "text": text }]);
                }
                vec![CompletionEvent::ThinkingDelta(ThinkingDelta {
                    text: text.to_owned(),
                })]
            })
            .unwrap_or_default()),
        Some("response.function_call_arguments.delta") => {
            let delta = value["delta"].as_str().unwrap_or_default();
            if let Some(call) = tool_call.as_mut() {
                call.arguments.push_str(delta);
            }
            Ok(Vec::new())
        }
        Some("response.function_call_arguments.done") => {
            if let Some(arguments) = value["arguments"].as_str()
                && let Some(call) = tool_call.as_mut()
            {
                arguments.clone_into(&mut call.arguments);
            }
            Ok(tool_call
                .take()
                .map(PartialToolCall::into_event)
                .transpose()?
                .map(|event| vec![event])
                .unwrap_or_default())
        }
        Some("response.completed") => Ok(vec![CompletionEvent::Finished]),
        Some("error") => Err(api_error(
            RESPONSES_NAME,
            value.get("error").unwrap_or(value),
        )),
        _ => Ok(Vec::new()),
    }
}

fn openai_thinking_block(item: &Value) -> ThinkingBlock {
    ThinkingBlock {
        kind: ThinkingBlockKind::OpenAiResponses,
        text: openai_reasoning_text(item),
        payload: item.clone(),
    }
}

fn openai_reasoning_text(item: &Value) -> String {
    item["summary"]
        .as_array()
        .map(|summary| {
            summary
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub(super) struct PartialToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

impl PartialToolCall {
    pub(super) fn into_event(self) -> Result<CompletionEvent> {
        let arguments = if self.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments)?
        };
        Ok(CompletionEvent::ToolCall(ToolCall::new(
            self.id, self.name, arguments,
        )))
    }
}

fn partial_tool_call_from_response_item(item: &Value) -> PartialToolCall {
    PartialToolCall {
        id: item["call_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .unwrap_or_default()
            .to_owned(),
        name: item["name"].as_str().unwrap_or_default().to_owned(),
        arguments: item["arguments"].as_str().unwrap_or_default().to_owned(),
    }
}

fn api_error(provider: &'static str, value: &Value) -> LlmError {
    LlmError::Api {
        provider,
        message: value["message"]
            .as_str()
            .map_or_else(|| value.to_string(), ToOwned::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use crate::CompletionRequest;

    use serde_json::json;

    use crate::{ThinkingBlock, ThinkingBlockKind, ToolCall, ToolResult};

    use super::{
        OpenAiReasoningConfig, OpenAiReasoningEffort, OpenAiReasoningSummary, responses_body,
        responses_input,
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
