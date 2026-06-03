//! Shared `OpenAI` provider helpers.

use std::collections::VecDeque;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};

use crate::stream::{BoxCompletionStream, CompletionEvent, ToolCall};
use crate::{LlmError, Result};

pub(super) const CHAT_COMPLETIONS_NAME: &str = "openai_chat_completions";
pub(super) const RESPONSES_NAME: &str = "openai_responses";
pub(super) const RESPONSES_WEBSOCKET_NAME: &str = "openai_responses_websocket";

pub(super) async fn stream_openai_http(
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

pub(super) fn api_error(provider: &'static str, value: &Value) -> LlmError {
    LlmError::Api {
        provider,
        message: value["message"]
            .as_str()
            .map_or_else(|| value.to_string(), ToOwned::to_owned),
    }
}
