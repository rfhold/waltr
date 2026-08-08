use std::sync::Arc;

use futures_util::StreamExt;
use mcp::{
    McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
    },
};
use serde_json::json;
use tokio::sync::Notify;

use crate::spec::support::{SpecServer, modern_params};

struct StreamingServer {
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
}

struct NotifyOnDrop(Arc<Notify>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl ServerHandler for StreamingServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-request-sse-spec-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: vec![McpToolDefinition {
                    name: "stream".to_owned(),
                    title: None,
                    description: None,
                    icons: Vec::new(),
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    annotations: None,
                    meta: None,
                }],
                next_cursor: None,
            })
        })
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        let started = Arc::clone(&self.started);
        let cancelled = Arc::clone(&self.cancelled);
        Box::pin(async move {
            let _notify_on_drop = NotifyOnDrop(cancelled);
            context
                .progress(json!(1), 1.0, None, Some("ready".to_owned()))
                .await
                .expect("progress notification");
            started.notify_one();
            context.cancelled().await;
            Ok(McpToolResult::text("cancelled"))
        })
    }
}

#[tokio::test]
async fn request_sse_delivers_incrementally_and_disconnect_cancels_handler() -> mcp::Result<()> {
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let server = SpecServer::start(Arc::new(StreamingServer {
        started: Arc::clone(&started),
        cancelled: Arc::clone(&cancelled),
    }))
    .await?;
    let mut params = modern_params(json!({ "name": "stream", "arguments": {} }));
    params["_meta"]["progressToken"] = json!(1);
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": "stream", "method": "tools/call", "params": params
        }))
        .send()
        .await?;
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "text/event-stream"
    );
    let mut stream = response.bytes_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("progress arrives before handler completion")
        .expect("SSE frame")?;
    assert!(String::from_utf8_lossy(&first).contains("notifications/progress"));
    started.notified().await;
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(1), cancelled.notified())
        .await
        .expect("disconnect cancels originating handler");
    Ok(())
}
