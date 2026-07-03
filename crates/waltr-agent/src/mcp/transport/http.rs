use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::Instrument as _;

use crate::Result;

use super::{InboundSender, TransportInbound, sse::parse_sse_response};
use crate::mcp::protocol::{MCP_PROTOCOL_VERSION, parse_json_rpc_message, protocol_error};

#[derive(Debug, Default)]
struct HttpSessionState {
    session_id: Option<String>,
    last_event_id: Option<String>,
}

pub(in crate::mcp) struct StreamableHttpTransport {
    endpoint: String,
    client: reqwest::Client,
    state: Arc<Mutex<HttpSessionState>>,
    inbound: InboundSender,
}

impl StreamableHttpTransport {
    pub(in crate::mcp) fn new(endpoint: String, inbound: InboundSender) -> Self {
        Self {
            endpoint,
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(HttpSessionState::default())),
            inbound,
        }
    }

    pub(in crate::mcp) async fn send(&self, message: Value, initialized: bool) -> Result<()> {
        let state = self.state.lock().await;
        let session_id = state.session_id.clone();
        drop(state);

        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&message);
        if initialized {
            builder = builder.header("mcp-protocol-version", MCP_PROTOCOL_VERSION);
        }
        if let Some(session_id) = &session_id {
            builder = builder.header("mcp-session-id", session_id);
        }
        let span = tracing::info_span!(
            "http.client.request",
            http_request_method = "POST",
            server_address = %self.endpoint,
            client = "mcp",
            mcp_session = session_id.is_some(),
            http_response_status_code = tracing::field::Empty,
        );
        let response = builder.send().instrument(span.clone()).await?;
        let status = response.status();
        span.record("http_response_status_code", status.as_u16());
        if status == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            self.state.lock().await.session_id = None;
            drop(self.inbound.send(TransportInbound::Disconnected(
                "MCP HTTP session expired".to_owned(),
            )));
            return Ok(());
        }
        if !status.is_success() {
            return Err(protocol_error(format!(
                "MCP HTTP request failed with status {status}"
            )));
        }
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            self.state.lock().await.session_id = Some(session_id.to_owned());
        }
        if status == reqwest::StatusCode::ACCEPTED || response.content_length() == Some(0) {
            return Ok(());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if content_type.contains("text/event-stream") {
            for event in parse_sse_response(response).await? {
                if let Some(id) = event.id {
                    self.state.lock().await.last_event_id = Some(id.clone());
                    drop(self.inbound.send(TransportInbound::SseEventId(id)));
                }
                drop(self.inbound.send(TransportInbound::Message(event.message)));
            }
        } else {
            let value = response.json::<Value>().await?;
            drop(
                self.inbound
                    .send(TransportInbound::Message(parse_json_rpc_message(value)?)),
            );
        }
        Ok(())
    }
}
