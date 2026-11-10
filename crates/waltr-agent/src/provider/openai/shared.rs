//! Shared `OpenAI` provider helpers.

use std::collections::VecDeque;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tracing::Instrument as _;

use crate::stream::{BoxCompletionStream, CompletionEvent, ToolCall};
use crate::{LlmError, Result};

#[path = "diagnostics.rs"]
mod diagnostics;
use diagnostics::{Diagnostics, Stage};

pub(super) const CHAT_COMPLETIONS_NAME: &str = "openai_chat_completions";
pub(super) const RESPONSES_NAME: &str = "openai_responses";
pub(super) const RESPONSES_WEBSOCKET_NAME: &str = "openai_responses_websocket";

pub(super) const fn chargeable_input_tokens(
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
) -> Option<u64> {
    match (input_tokens, cached_tokens) {
        (Some(input), Some(cached)) => Some(input.saturating_sub(cached)),
        (input, None) => input,
        (None, Some(_)) => None,
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "keeps ordinary and bounded transient serialization on the same HTTP transport"
)]
pub(super) async fn stream_openai_http(
    provider: &'static str,
    url: &str,
    body: Value,
    transient: bool,
    headers: impl IntoIterator<Item = (String, String)>,
    parse_event: impl FnMut(&str) -> Result<Vec<CompletionEvent>> + Send + 'static,
) -> Result<BoxCompletionStream> {
    let mut diagnostics = Diagnostics::new(body["tools"].as_array().map_or(0, Vec::len));
    // Ordinary bodies match the journaled projection. Invocation-local bodies
    // have metadata-only audits and cannot be reconstructed after restart.
    let builder = crate::outbound_http::client().post(url);
    let mut request_builder = if transient {
        builder
            .header("content-type", "application/json")
            .body(crate::transient_input::encode_body(&body)?)
    } else {
        builder.json(&body)
    };
    drop(body);
    for (name, value) in headers {
        request_builder = request_builder.header(name.as_str(), value);
    }
    diagnostics.stage(Stage::Serialized);
    let span = tracing::info_span!(
        "http.client.request",
        http_request_method = "POST",
        server_address = %url,
        provider,
        http_response_status_code = tracing::field::Empty,
    );
    diagnostics.stage(Stage::Send);
    let response = request_builder
        .send()
        .instrument(span.clone())
        .await
        .inspect_err(|error| diagnostics.http_error(error))?;

    let status = response.status();
    diagnostics.headers(status.as_u16());
    span.record("http_response_status_code", status.as_u16());
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), "provider http request failed");
        let headers = response_headers(response.headers());
        diagnostics.stage(Stage::ErrorBodyStart);
        let text = response
            .text()
            .await
            .inspect_err(|error| diagnostics.http_error(error))?;
        diagnostics.stage(Stage::ErrorBodyEnd);
        diagnostics.stage(Stage::Failed);
        return Err(LlmError::Api {
            provider,
            message: text.clone(),
            status: Some(status.as_u16()),
            headers,
            code: openai_error_code_from_text(&text),
            body: Some(text),
        });
    }
    span.in_scope(|| tracing::debug!(status = status.as_u16(), "provider http stream opened"));
    diagnostics.stage(Stage::Open);
    let stream_span = tracing::info_span!(
        "provider.stream_body",
        provider,
        terminal_state = tracing::field::Empty,
        chunk_count = tracing::field::Empty,
        event_count = tracing::field::Empty,
    );

    Ok(Box::pin(stream::unfold(
        (
            response.bytes_stream(),
            SseBuffer::default(),
            VecDeque::<String>::new(),
            VecDeque::<CompletionEvent>::new(),
            parse_event,
            false,
            stream_span,
            0_u64,
            0_u64,
            diagnostics,
        ),
        move |(
            mut bytes,
            mut sse,
            mut pending_sse,
            mut pending_events,
            mut parse_event,
            finished,
            stream_span,
            mut chunk_count,
            mut event_count,
            mut diagnostics,
        )| async move {
            if finished {
                return None;
            }

            loop {
                if let Some(event) = pending_events.pop_front() {
                    diagnostics.event();
                    match event {
                        CompletionEvent::Finished => {
                            diagnostics.stage(Stage::Finished);
                            event_count += 1;
                            stream_span.record("terminal_state", "finished");
                            stream_span.record("chunk_count", chunk_count);
                            stream_span.record("event_count", event_count);
                            stream_span.in_scope(|| {
                                tracing::info!(
                                    chunk_count,
                                    event_count,
                                    "provider stream completed"
                                );
                            });
                            return Some((
                                Ok(CompletionEvent::Finished),
                                (
                                    bytes,
                                    sse,
                                    pending_sse,
                                    pending_events,
                                    parse_event,
                                    true,
                                    stream_span,
                                    chunk_count,
                                    event_count,
                                    diagnostics,
                                ),
                            ));
                        }
                        event => {
                            event_count += 1;
                            return Some((
                                Ok(event),
                                (
                                    bytes,
                                    sse,
                                    pending_sse,
                                    pending_events,
                                    parse_event,
                                    false,
                                    stream_span,
                                    chunk_count,
                                    event_count,
                                    diagnostics,
                                ),
                            ));
                        }
                    }
                }

                if let Some(event) = pending_sse.pop_front() {
                    diagnostics.sse();
                    match parse_event(&event) {
                        Ok(events) => {
                            diagnostics.parsed();
                            pending_events.extend(events);
                        }
                        Err(error) => {
                            diagnostics.parse_error();
                            stream_span.record("terminal_state", "error");
                            stream_span.record("chunk_count", chunk_count);
                            stream_span.record("event_count", event_count);
                            stream_span.in_scope(|| tracing::warn!(%error, chunk_count, event_count, "provider stream parse failed"));
                            return Some((
                                Err(error),
                                (
                                    bytes,
                                    sse,
                                    pending_sse,
                                    pending_events,
                                    parse_event,
                                    true,
                                    stream_span,
                                    chunk_count,
                                    event_count,
                                    diagnostics,
                                ),
                            ));
                        }
                    }
                    continue;
                }

                match bytes.next().await {
                    Some(Ok(chunk)) => {
                        diagnostics.chunk();
                        chunk_count += 1;
                        pending_sse.extend(sse.push(chunk.as_ref()));
                    }
                    Some(Err(error)) => {
                        diagnostics.http_error(&error);
                        stream_span.record("terminal_state", "error");
                        stream_span.record("chunk_count", chunk_count);
                        stream_span.record("event_count", event_count);
                        stream_span.in_scope(|| tracing::warn!(%error, chunk_count, event_count, "provider stream read failed"));
                        return Some((
                            Err(error.into()),
                            (
                                bytes,
                                sse,
                                pending_sse,
                                pending_events,
                                parse_event,
                                true,
                                stream_span,
                                chunk_count,
                                event_count,
                                diagnostics,
                            ),
                        ));
                    }
                    None => {
                        diagnostics.stage(Stage::Eof);
                        stream_span.record("terminal_state", "eof");
                        stream_span.record("chunk_count", chunk_count);
                        stream_span.record("event_count", event_count);
                        stream_span.in_scope(|| {
                            tracing::info!(chunk_count, event_count, "provider stream ended");
                        });
                        return None;
                    }
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

// Transient parser state only; crash recovery retries the turn from the
// journaled request record, so partial tool calls are never persisted.
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
        status: None,
        headers: Vec::new(),
        code: value["code"]
            .as_str()
            .or_else(|| value["type"].as_str())
            .map(str::to_owned),
        body: Some(value.to_string()),
    }
}

fn response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn openai_error_code_from_text(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text).ok().and_then(|value| {
        let error = value.get("error").unwrap_or(&value);
        error["code"]
            .as_str()
            .or_else(|| error["type"].as_str())
            .map(str::to_owned)
    })
}
