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

pub async fn post_json(endpoint: &str, body: Value) -> mcp::Result<JsonResponse> {
    let client = reqwest::Client::new();
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| mcp::Error::protocol("test request omitted method"))?;
    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .header("mcp-method", method)
        .json(&body);
    if let Some(name) = request_name(method, &body) {
        request = request.header("mcp-name", name);
    }
    let response = request.send().await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.bytes().await?;
    let body = if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
    {
        let text =
            std::str::from_utf8(&bytes).map_err(|error| mcp::Error::protocol(error.to_string()))?;
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .next_back()
            .ok_or_else(|| mcp::Error::protocol("SSE response omitted data"))?
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok(JsonResponse {
        status,
        headers,
        body,
    })
}

pub fn request_params(value: &Value) -> Value {
    modern_params(json!({ "value": value.clone() }))
}

pub fn modern_params(mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {
            "name": "mcp-spec-test-client",
            "version": "1.0.0"
        }
    });
    params
}

pub fn sse_messages(body: &str) -> mcp::Result<Vec<Value>> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).map_err(Into::into))
        .collect()
}

fn request_name<'a>(method: &str, body: &'a Value) -> Option<&'a str> {
    let params = body.get("params")?;
    match method {
        "tools/call" | "prompts/get" => params.get("name")?.as_str(),
        "resources/read" => params.get("uri")?.as_str(),
        _ => None,
    }
}
