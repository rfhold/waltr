use std::{net::SocketAddr, sync::Arc};

use mcp::protocol::MCP_PROTOCOL_VERSION;
use mcp::server::{ServerHandler, StreamableHttpOptions};
use reqwest::{StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

pub struct SpecServer {
    address: SocketAddr,
    _task: JoinHandle<()>,
}

impl SpecServer {
    pub async fn start(handler: Arc<dyn ServerHandler>) -> mcp::Result<Self> {
        Self::start_with_options(handler, StreamableHttpOptions::default()).await
    }

    pub async fn start_with_options(
        handler: Arc<dyn ServerHandler>,
        options: StreamableHttpOptions,
    ) -> mcp::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = mcp::server::streamable_http_router_with_options(handler, options);
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::debug!(%error, "mcp spec server stopped");
            }
        });
        Ok(Self {
            address,
            _task: task,
        })
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}/mcp", self.address)
    }
}

pub struct JsonResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

pub async fn initialized_session(endpoint: &str) -> mcp::Result<String> {
    let client = reqwest::Client::new();
    let initialize = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "spec-initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "mcp-spec-test-client",
                    "version": "1.0.0",
                },
            },
        }))
        .send()
        .await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| mcp::Error::protocol("missing MCP session id"))?
        .to_owned();
    drop(initialize.json::<Value>().await?);
    drop(
        client
            .post(endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }))
            .send()
            .await?
            .bytes()
            .await?,
    );
    Ok(session_id)
}

pub async fn post_json(endpoint: &str, body: Value) -> mcp::Result<JsonResponse> {
    let client = reqwest::Client::new();
    let session_id = if should_initialize_before_post(&body) {
        Some(initialized_session(endpoint).await?)
    } else {
        None
    };

    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(session_id) = &session_id {
        request = request
            .header("mcp-session-id", session_id)
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION);
    }
    let response = request.send().await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.json::<Value>().await?;
    Ok(JsonResponse {
        status,
        headers,
        body,
    })
}

fn should_initialize_before_post(body: &Value) -> bool {
    body.get("method").and_then(Value::as_str) != Some("initialize")
}
