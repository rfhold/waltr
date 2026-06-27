//! `OpenAI` Responses websocket transport.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt, stream};
use http::HeaderValue;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::LlmError;
use crate::provider::openai::reasoning::{OpenAiReasoningConfig, openai_reasoning};
use crate::provider::openai::shared::RESPONSES_WEBSOCKET_NAME;
use crate::stream::{BoxCompletionStream, CompletionEvent, CompletionRequest, LlmStream};

use super::parser::ResponsesParser;
use super::request::{responses_input, responses_tools};

/// `OpenAI` Responses websocket streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiResponsesWebSocket {
    headers: Vec<(&'static str, String)>,
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

    /// Set the API base URL used by this provider.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        let base_url = base_url.as_ref().trim_end_matches('/');
        let websocket_base_url = base_url
            .strip_prefix("https://")
            .map_or_else(|| base_url.to_owned(), |rest| format!("wss://{rest}"));
        self.responses_url = format!("{websocket_base_url}/v1/responses");
        self
    }

    /// Add a static request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
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
                websocket_request.headers_mut().insert(*name, value);
            }
            let (mut websocket, _) = connect_async_tls_with_config(
                websocket_request,
                None,
                false,
                Some(tls_connector()?),
            )
            .await?;
            let create = response_create_payload(&self.model, self.reasoning.as_ref(), &request);
            websocket
                .send(Message::Text(create.to_string().into()))
                .await?;
            let parser =
                ResponsesParser::new(RESPONSES_WEBSOCKET_NAME, &self.responses_url, &self.model);

            Ok(Box::pin(stream::unfold(
                (websocket, parser, Vec::<CompletionEvent>::new(), false),
                |(mut websocket, mut parser, mut pending, finished)| async move {
                    if finished {
                        return None;
                    }

                    loop {
                        if let Some(event) = pending.pop() {
                            if matches!(event, CompletionEvent::Finished) {
                                if let Err(error) = websocket.close(None).await {
                                    return Some((
                                        Err(error.into()),
                                        (websocket, parser, pending, true),
                                    ));
                                }
                                return Some((Ok(event), (websocket, parser, pending, true)));
                            }
                            return Some((Ok(event), (websocket, parser, pending, false)));
                        }

                        let message = match websocket.next().await? {
                            Ok(message) => message,
                            Err(error) => {
                                return Some((
                                    Err(error.into()),
                                    (websocket, parser, pending, true),
                                ));
                            }
                        };

                        match message {
                            Message::Text(text) => match parser.parse_event(text.as_str()) {
                                Ok(events) => pending.extend(events.into_iter().rev()),
                                Err(error) => {
                                    return Some((Err(error), (websocket, parser, pending, true)));
                                }
                            },
                            Message::Close(_) => return None,
                            Message::Ping(payload) => {
                                if let Err(error) = websocket.send(Message::Pong(payload)).await {
                                    return Some((
                                        Err(error.into()),
                                        (websocket, parser, pending, true),
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
