use std::time::Duration;

use serde_json::{Value, json};
use tokio::{sync::Mutex, time};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

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

    pub(in crate::mcp) async fn request(
        &self,
        request: Value,
        id: Option<u64>,
        timeout: Duration,
        cancellation_token: CancellationToken,
    ) -> Result<Value> {
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
        let session_id = self.session_id.lock().await.clone();
        if cancellation_token.is_cancelled() {
            return Err(protocol_error("MCP request canceled"));
        }
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
        let span = tracing::info_span!(
            "http.client.request",
            http_request_method = "POST",
            server_address = %self.endpoint,
            client = "mcp",
            mcp_session = session_id.is_some(),
            http_response_status_code = tracing::field::Empty,
        );
        let response = tokio::select! {
            biased;

            () = cancellation_token.cancelled() => {
                return Err(protocol_error("MCP request canceled"));
            }
            () = time::sleep(timeout) => {
                return Err(protocol_error("MCP request timed out"));
            }
            response = builder.send().instrument(span.clone()) => response?,
        };
        let status = response.status();
        span.record("http_response_status_code", status.as_u16());
        if status == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            *self.session_id.lock().await = None;
            return Err(protocol_error("MCP HTTP session expired"));
        }
        if !status.is_success() {
            span.in_scope(|| tracing::warn!(status = status.as_u16(), "mcp http request failed"));
            return Err(protocol_error(format!(
                "MCP HTTP request failed with status {}",
                status
            )));
        }
        span.in_scope(|| tracing::debug!(status = status.as_u16(), "mcp http response received"));
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
            parse_sse_response(response, id, timeout, cancellation_token).await
        } else if id.is_none() {
            Ok(json!(null))
        } else {
            let message = tokio::select! {
                biased;

                () = cancellation_token.cancelled() => {
                    return Err(protocol_error("MCP request canceled"));
                }
                () = time::sleep(timeout) => {
                    return Err(protocol_error("MCP request timed out"));
                }
                message = response.json::<JsonRpcMessage>() => message?,
            };
            json_rpc_result(message)
        }
    }
}
