use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::Result;

use super::super::protocol::{
    JsonRpcMessage, MCP_PROTOCOL_VERSION, json_rpc_result, protocol_error,
};
use super::sse::parse_sse_response;

pub(in crate::mcp) struct StreamableHttpTransport {
    endpoint: String,
    client: reqwest::Client,
    session_id: Mutex<Option<String>>,
}

impl StreamableHttpTransport {
    pub(in crate::mcp) fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: reqwest::Client::new(),
            session_id: Mutex::new(None),
        }
    }

    pub(in crate::mcp) async fn request(&self, request: Value, id: Option<u64>) -> Result<Value> {
        let session_id = self.session_id.lock().await.clone();
        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .json(&request);
        if let Some(session_id) = &session_id {
            builder = builder
                .header("mcp-session-id", session_id)
                .header("mcp-protocol-version", MCP_PROTOCOL_VERSION);
        }
        let response = builder.send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            *self.session_id.lock().await = None;
            return Err(protocol_error("MCP HTTP session expired"));
        }
        if !response.status().is_success() {
            return Err(protocol_error(format!(
                "MCP HTTP request failed with status {}",
                response.status()
            )));
        }
        if let Some(session_id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session_id.to_owned());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if content_type.contains("text/event-stream") {
            parse_sse_response(response, id).await
        } else if id.is_none() {
            Ok(json!(null))
        } else {
            json_rpc_result(response.json::<JsonRpcMessage>().await?)
        }
    }
}
