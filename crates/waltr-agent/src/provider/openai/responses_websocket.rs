//! `OpenAI` Responses websocket streaming provider.

use futures_util::{SinkExt, StreamExt, stream};
use http::HeaderValue;
use serde_json::json;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::LlmError;
use crate::stream::{BoxCompletionStream, CompletionEvent, CompletionRequest, LlmStream};

use super::{
    OpenAiReasoningConfig, RESPONSES_WEBSOCKET_NAME, ResponsesParser, openai_reasoning,
    responses_input, responses_tools,
};

/// `OpenAI` Responses websocket streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiResponsesWebSocket {
    api_key: String,
    model: String,
    responses_url: String,
    reasoning: Option<OpenAiReasoningConfig>,
}

impl OpenAiResponsesWebSocket {
    /// Create an `OpenAI` Responses websocket provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
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

    /// Enable `OpenAI` Responses reasoning for websocket requests from this provider.
    #[must_use]
    pub const fn with_reasoning(mut self, config: OpenAiReasoningConfig) -> Self {
        self.reasoning = Some(config);
        self
    }
}

impl LlmStream for OpenAiResponsesWebSocket {
    fn name(&self) -> &'static str {
        RESPONSES_WEBSOCKET_NAME
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut websocket_request = self.responses_url.as_str().into_client_request()?;
            if !self.api_key.is_empty() {
                let authorization = HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                    .map_err(|error| LlmError::Protocol {
                        provider: RESPONSES_WEBSOCKET_NAME,
                        message: format!("invalid authorization header: {error}"),
                    })?;
                websocket_request
                    .headers_mut()
                    .insert("Authorization", authorization);
            }
            let (mut websocket, _) = connect_async(websocket_request).await?;
            let mut create = json!({
                "type": "response.create",
                "model": self.model,
                "store": false,
                "input": responses_input(&request),
                "tools": responses_tools(&request.tools),
            });
            if let Some(reasoning) = &self.reasoning {
                create["reasoning"] = openai_reasoning(reasoning);
                create["include"] = json!(["reasoning.encrypted_content"]);
            }
            websocket
                .send(Message::Text(create.to_string().into()))
                .await?;
            let parser = ResponsesParser::default();

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
