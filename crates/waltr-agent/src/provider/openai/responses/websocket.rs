//! `OpenAI` Responses websocket transport.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt, stream};
use http::HeaderValue;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};
use tracing::Instrument as _;

use crate::LlmError;
use crate::provider::openai::reasoning::{OpenAiReasoningConfig, openai_reasoning};
use crate::provider::openai::shared::RESPONSES_WEBSOCKET_NAME;
use crate::stream::{BoxCompletionStream, CompletionEvent, CompletionRequest, LlmStream};

use super::parser::ResponsesParser;
use super::request::{responses_input, responses_tools};

/// `OpenAI` Responses websocket streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiResponsesWebSocket {
    headers: Vec<(String, String)>,
    model: String,
    responses_url: String,
    reasoning: Option<OpenAiReasoningConfig>,
}

impl OpenAiResponsesWebSocket {
    /// Create an `OpenAI` Responses websocket provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            headers: Vec::new(),
            model: model.into(),
            responses_url: "wss://api.openai.com/v1/responses".to_owned(),
            reasoning: None,
        }
    }

    /// Set the exact Responses WebSocket endpoint URL used by this provider.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.responses_url = url.into();
        self
    }

    /// Add a request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Enable `OpenAI` Responses reasoning for websocket requests from this provider.
    #[must_use]
    pub const fn with_reasoning(mut self, config: OpenAiReasoningConfig) -> Self {
        self.reasoning = Some(config);
        self
    }
}

/// Build a TLS connector with an explicit crypto provider so the library never
/// depends on (or mutates) the process-level rustls default.
fn tls_connector() -> Result<Connector, LlmError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| LlmError::Protocol {
        provider: RESPONSES_WEBSOCKET_NAME,
        message: format!("failed to configure TLS: {error}"),
    })?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}

fn response_create_payload(
    model: &str,
    reasoning: Option<&OpenAiReasoningConfig>,
    request: &CompletionRequest<'_>,
) -> Value {
    let mut create = json!({
        "type": "response.create",
        "model": model,
        "store": false,
        "input": responses_input(request),
        "tools": responses_tools(&request.tools),
        // With store:false, reasoning items can only be replayed when they
        // carry encrypted content, so always ask for it: reasoning models emit
        // reasoning items even without an explicit reasoning config.
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(reasoning) = reasoning {
        create["reasoning"] = openai_reasoning(reasoning);
    }
    if let Some(system_prompt) = request.system_prompt {
        create["instructions"] = json!(system_prompt);
    }
    create
}

impl LlmStream for OpenAiResponsesWebSocket {
    fn name(&self) -> &'static str {
        RESPONSES_WEBSOCKET_NAME
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let request = request.for_provider_identity(
            RESPONSES_WEBSOCKET_NAME,
            &self.responses_url,
            &self.model,
        );
        Some(response_create_payload(
            &self.model,
            self.reasoning.as_ref(),
            &request,
        ))
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let request = request.for_provider_identity(
                RESPONSES_WEBSOCKET_NAME,
                &self.responses_url,
                &self.model,
            );
            let mut websocket_request = self.responses_url.as_str().into_client_request()?;
            for (name, value) in &self.headers {
                let value = HeaderValue::from_str(value).map_err(|error| LlmError::Protocol {
                    provider: RESPONSES_WEBSOCKET_NAME,
                    message: format!("invalid {name} header: {error}"),
                })?;
                let name =
                    http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                        LlmError::Protocol {
                            provider: RESPONSES_WEBSOCKET_NAME,
                            message: format!("invalid header name {name}: {error}"),
                        }
                    })?;
                websocket_request.headers_mut().insert(name, value);
            }
            let connect_span = tracing::info_span!(
                "websocket.client.connect",
                server_address = %self.responses_url,
                provider = RESPONSES_WEBSOCKET_NAME,
                model = %self.model,
                terminal_state = tracing::field::Empty,
            );
            let (mut websocket, _) = connect_async_tls_with_config(
                websocket_request,
                None,
                false,
                Some(tls_connector()?),
            )
            .instrument(connect_span.clone())
            .await?;
            connect_span.record("terminal_state", "connected");
            connect_span.in_scope(|| tracing::debug!("provider websocket connected"));

            let create = response_create_payload(&self.model, self.reasoning.as_ref(), &request);
            let send_span = tracing::info_span!(
                "websocket.client.send",
                provider = RESPONSES_WEBSOCKET_NAME,
                model = %self.model,
                message_kind = "response.create",
                terminal_state = tracing::field::Empty,
            );
            websocket
                .send(Message::Text(create.to_string().into()))
                .instrument(send_span.clone())
                .await?;
            send_span.record("terminal_state", "sent");
            let parser =
                ResponsesParser::new(RESPONSES_WEBSOCKET_NAME, &self.responses_url, &self.model);
            let stream_span = tracing::info_span!(
                "provider.stream_body",
                provider = RESPONSES_WEBSOCKET_NAME,
                model = %self.model,
                transport = "websocket",
                terminal_state = tracing::field::Empty,
                message_count = tracing::field::Empty,
                event_count = tracing::field::Empty,
            );

            Ok(Box::pin(stream::unfold(
                (
                    websocket,
                    parser,
                    Vec::<CompletionEvent>::new(),
                    false,
                    stream_span,
                    0_u64,
                    0_u64,
                ),
                |(
                    mut websocket,
                    mut parser,
                    mut pending,
                    finished,
                    stream_span,
                    mut message_count,
                    mut event_count,
                )| async move {
                    if finished {
                        return None;
                    }

                    loop {
                        if let Some(event) = pending.pop() {
                            event_count += 1;
                            if matches!(event, CompletionEvent::Finished) {
                                if let Err(error) = websocket.close(None).await {
                                    record_stream_error(
                                        &stream_span,
                                        &error,
                                        message_count,
                                        event_count,
                                    );
                                    return Some((
                                        Err(error.into()),
                                        (
                                            websocket,
                                            parser,
                                            pending,
                                            true,
                                            stream_span,
                                            message_count,
                                            event_count,
                                        ),
                                    ));
                                }
                                record_stream_end(
                                    &stream_span,
                                    "finished",
                                    message_count,
                                    event_count,
                                );
                                return Some((
                                    Ok(event),
                                    (
                                        websocket,
                                        parser,
                                        pending,
                                        true,
                                        stream_span,
                                        message_count,
                                        event_count,
                                    ),
                                ));
                            }
                            return Some((
                                Ok(event),
                                (
                                    websocket,
                                    parser,
                                    pending,
                                    false,
                                    stream_span,
                                    message_count,
                                    event_count,
                                ),
                            ));
                        }

                        let message = match websocket.next().await {
                            Some(result) => result,
                            None => {
                                record_stream_end(&stream_span, "eof", message_count, event_count);
                                return None;
                            }
                        };
                        let message = match message {
                            Ok(message) => {
                                message_count += 1;
                                message
                            }
                            Err(error) => {
                                record_stream_error(
                                    &stream_span,
                                    &error,
                                    message_count,
                                    event_count,
                                );
                                return Some((
                                    Err(error.into()),
                                    (
                                        websocket,
                                        parser,
                                        pending,
                                        true,
                                        stream_span,
                                        message_count,
                                        event_count,
                                    ),
                                ));
                            }
                        };

                        match message {
                            Message::Text(text) => match parser.parse_event(text.as_str()) {
                                Ok(events) => pending.extend(events.into_iter().rev()),
                                Err(error) => {
                                    record_stream_error(
                                        &stream_span,
                                        &error,
                                        message_count,
                                        event_count,
                                    );
                                    return Some((
                                        Err(error),
                                        (
                                            websocket,
                                            parser,
                                            pending,
                                            true,
                                            stream_span,
                                            message_count,
                                            event_count,
                                        ),
                                    ));
                                }
                            },
                            Message::Close(_) => {
                                record_stream_end(
                                    &stream_span,
                                    "closed",
                                    message_count,
                                    event_count,
                                );
                                return None;
                            }
                            Message::Ping(payload) => {
                                if let Err(error) = websocket.send(Message::Pong(payload)).await {
                                    record_stream_error(
                                        &stream_span,
                                        &error,
                                        message_count,
                                        event_count,
                                    );
                                    return Some((
                                        Err(error.into()),
                                        (
                                            websocket,
                                            parser,
                                            pending,
                                            true,
                                            stream_span,
                                            message_count,
                                            event_count,
                                        ),
                                    ));
                                }
                            }
                            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                        }
                    }
                },
            )) as BoxCompletionStream)
        })
    }
}

fn record_stream_end(
    stream_span: &tracing::Span,
    terminal_state: &'static str,
    message_count: u64,
    event_count: u64,
) {
    stream_span.record("terminal_state", terminal_state);
    stream_span.record("message_count", message_count);
    stream_span.record("event_count", event_count);
    stream_span.in_scope(|| {
        tracing::info!(
            message_count,
            event_count,
            terminal_state,
            "provider websocket stream ended"
        )
    });
}

fn record_stream_error(
    stream_span: &tracing::Span,
    error: &dyn std::fmt::Display,
    message_count: u64,
    event_count: u64,
) {
    stream_span.record("terminal_state", "error");
    stream_span.record("message_count", message_count);
    stream_span.record("event_count", event_count);
    stream_span.in_scope(
        || tracing::warn!(%error, message_count, event_count, "provider websocket stream failed"),
    );
}

#[cfg(test)]
mod tests {
    use crate::stream::CompletionRequest;

    use super::response_create_payload;

    #[test]
    fn response_create_payload_uses_instructions_for_system_prompt() {
        let mut request = CompletionRequest::new("hello");
        request.system_prompt = Some("system text");

        let payload = response_create_payload("gpt-test", None, &request);

        assert_eq!(payload["instructions"], "system text");
        assert_eq!(payload["input"][0]["role"], "user");
        assert_eq!(payload["input"].as_array().expect("input array").len(), 1);
    }
}
