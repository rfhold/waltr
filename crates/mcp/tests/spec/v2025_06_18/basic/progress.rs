use std::sync::Arc;

use mcp::{
    McpToolCall, McpToolResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::spec::support::{SpecServer, initialized_session};

struct ProgressServer;

#[derive(Default)]
struct ContextCaptureServer {
    progress_token: Arc<Mutex<Option<Value>>>,
    traceparent: Arc<Mutex<Option<Value>>>,
}

impl ServerHandler for ProgressServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-progress-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn call_tool(
        &self,
        call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async move {
            let token = call
                .progress_token
                .ok_or_else(|| ServerError::invalid_params("missing progress token"))?;
            context
                .progress(token, 0.5, Some(1.0), Some("Halfway done".to_owned()))
                .await
                .map_err(|error| ServerError::internal(error.to_string()))?;
            Ok(McpToolResult::text("progress complete"))
        })
    }
}

impl ServerHandler for ContextCaptureServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-context-capture-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        let progress_token = self.progress_token.clone();
        let traceparent = self.traceparent.clone();
        Box::pin(async move {
            *progress_token.lock().await = context.progress_token();
            *traceparent.lock().await = context.meta_value("traceparent").cloned();
            Ok(McpToolResult::text("captured"))
        })
    }
}

#[tokio::test]
async fn progress_notification_reuses_request_progress_token() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ProgressServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "progress-request",
            "method": "tools/call",
            "params": {
                "name": "progress-tool",
                "arguments": {},
                "_meta": { "progressToken": 20250618 }
            }
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let messages = sse_data_messages(&response.text().await?);

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["jsonrpc"], "2.0");
    assert_eq!(messages[0]["method"], "notifications/progress");
    assert_eq!(messages[0]["params"]["progressToken"], 20250618);
    assert_eq!(messages[0]["params"]["progress"], 0.5);
    assert_eq!(messages[0]["params"]["total"], 1.0);
    assert_eq!(messages[0]["params"]["message"], "Halfway done");

    assert_eq!(messages[1]["jsonrpc"], "2.0");
    assert_eq!(messages[1]["id"], "progress-request");
    assert_eq!(
        messages[1]["result"],
        json!({ "content": [{ "type": "text", "text": "progress complete" }] })
    );

    Ok(())
}

#[tokio::test]
async fn server_context_progress_token_comes_from_request_meta() -> mcp::Result<()> {
    let handler = Arc::new(ContextCaptureServer::default());
    let server = SpecServer::start(handler.clone()).await?;
    let session_id = initialized_session(&server.endpoint()).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "context-progress-token-request",
            "method": "tools/call",
            "params": {
                "name": "capture-context",
                "arguments": {},
                "_meta": { "progressToken": "token-1" }
            }
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(*handler.progress_token.lock().await, Some(json!("token-1")));

    Ok(())
}

#[tokio::test]
async fn server_context_exposes_request_meta_values() -> mcp::Result<()> {
    let handler = Arc::new(ContextCaptureServer::default());
    let server = SpecServer::start(handler.clone()).await?;
    let session_id = initialized_session(&server.endpoint()).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "context-meta-request",
            "method": "tools/call",
            "params": {
                "name": "capture-context",
                "arguments": {},
                "_meta": {
                    "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                }
            }
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        *handler.traceparent.lock().await,
        Some(json!(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ))
    );

    Ok(())
}

fn sse_data_messages(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|event| {
            event.lines().find_map(|line| {
                line.strip_prefix("data: ")
                    .map(|data| serde_json::from_str(data).expect("SSE data is JSON"))
            })
        })
        .collect()
}
