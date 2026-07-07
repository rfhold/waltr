use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::Instrument as _;

use crate::{
    Error, Result,
    protocol::{MCP_PROTOCOL_VERSION, parse_json_rpc_message},
    trace::{request_id, request_method},
};

use super::{InboundSender, TransportInbound, sse::parse_sse_response};

#[derive(Debug, Default)]
struct HttpSessionState {
    session_id: Option<String>,
    last_event_id: Option<String>,
}

/// Streamable HTTP client transport.
pub struct StreamableHttpTransport {
    endpoint: String,
    client: reqwest::Client,
    state: Arc<Mutex<HttpSessionState>>,
    inbound: InboundSender,
    polling_started: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl std::fmt::Debug for StreamableHttpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamableHttpTransport")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl StreamableHttpTransport {
    /// Create a streamable HTTP transport.
    #[must_use]
    pub fn new(endpoint: String, inbound: InboundSender) -> Self {
        Self {
            endpoint,
            client: reqwest::Client::new(),
            state: Arc::new(Mutex::new(HttpSessionState::default())),
            inbound,
            polling_started: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Send one message over streamable HTTP.
    pub async fn send(&self, message: Value, initialized: bool) -> Result<()> {
        let state = self.state.lock().await;
        let session_id = state.session_id.clone();
        drop(state);
        let method = request_method(&message).unwrap_or("");
        let id = request_id(&message);

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
            rpc.system = "jsonrpc",
            rpc.jsonrpc.version = "2.0",
            rpc.method = %method,
            mcp.method = %method,
            mcp.request_id = id.as_deref().unwrap_or(""),
            mcp.transport = "streamable_http",
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
            return Err(Error::protocol(format!(
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
            if initialized && method == "notifications/initialized" {
                self.start_polling();
            }
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
        if initialized && method == "notifications/initialized" {
            self.start_polling();
        }
        Ok(())
    }

    fn start_polling(&self) {
        if self.polling_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let endpoint = self.endpoint.clone();
        let client = self.client.clone();
        let state = Arc::clone(&self.state);
        let inbound = self.inbound.clone();
        let shutdown = Arc::clone(&self.shutdown);
        tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                let state_snapshot = state.lock().await;
                let session_id = state_snapshot.session_id.clone();
                let last_event_id = state_snapshot.last_event_id.clone();
                drop(state_snapshot);
                let Some(session_id) = session_id else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };

                let mut request = client
                    .get(&endpoint)
                    .header("accept", "text/event-stream")
                    .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
                    .header("mcp-session-id", &session_id);
                if let Some(last_event_id) = &last_event_id {
                    request = request.header("last-event-id", last_event_id);
                }
                let response = match request.send().await {
                    Ok(response) => response,
                    Err(error) => {
                        drop(inbound.send(TransportInbound::Disconnected(format!(
                            "MCP HTTP polling failed: {error}"
                        ))));
                        break;
                    }
                };
                if response.status() == reqwest::StatusCode::NOT_FOUND {
                    state.lock().await.session_id = None;
                    drop(inbound.send(TransportInbound::Disconnected(
                        "MCP HTTP session expired".to_owned(),
                    )));
                    break;
                }
                if !response.status().is_success() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                match parse_sse_response(response).await {
                    Ok(events) => {
                        for event in events {
                            if let Some(id) = event.id {
                                state.lock().await.last_event_id = Some(id.clone());
                                drop(inbound.send(TransportInbound::SseEventId(id)));
                            }
                            drop(inbound.send(TransportInbound::Message(event.message)));
                        }
                    }
                    Err(error) => {
                        drop(inbound.send(TransportInbound::Disconnected(format!(
                            "MCP HTTP polling failed: {error}"
                        ))));
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    /// Stop background polling.
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
