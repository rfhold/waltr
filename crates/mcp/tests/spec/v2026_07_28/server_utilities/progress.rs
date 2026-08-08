use std::sync::Arc;

use mcp::{
    McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, modern_params, sse_messages};

struct ProgressServer;

impl ServerHandler for ProgressServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-progress-server", "0")
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
                    name: "progress".to_owned(),
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
        Box::pin(async move {
            if context.progress_token() != Some(json!("job-7"))
                || context.meta_value("traceparent") != Some(&json!("00-trace-parent"))
                || context.meta_value("example.test/opaque") != Some(&json!({ "tenant": 7 }))
            {
                return Err(ServerError::internal("request metadata was not preserved"));
            }
            context
                .progress(json!("job-7"), 1.0, Some(3.0), Some("started".to_owned()))
                .await
                .map_err(|error| ServerError::internal(error.to_string()))?;
            assert!(
                context
                    .progress(json!("other"), 2.0, None, None)
                    .await
                    .is_err()
            );
            assert!(
                context
                    .progress(json!("job-7"), 1.0, None, None)
                    .await
                    .is_err()
            );
            assert!(
                context
                    .progress(json!("job-7"), f64::NAN, None, None)
                    .await
                    .is_err()
            );
            Ok(McpToolResult::text("complete"))
        })
    }
}

#[tokio::test]
async fn progress_uses_the_active_token_and_preserves_request_metadata() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ProgressServer)).await?;
    let mut params = modern_params(json!({ "name": "progress", "arguments": {} }));
    params["_meta"]["progressToken"] = json!("job-7");
    params["_meta"]["traceparent"] = json!("00-trace-parent");
    params["_meta"]["example.test/opaque"] = json!({ "tenant": 7 });
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "progress")
        .json(&json!({
            "jsonrpc": "2.0", "id": "progress-request", "method": "tools/call",
            "params": params
        }))
        .send()
        .await?;
    let messages = sse_messages(&response.text().await?)?;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["method"], "notifications/progress");
    assert_eq!(
        messages[0]["params"],
        json!({
            "progressToken": "job-7", "progress": 1.0, "total": 3.0,
            "message": "started"
        })
    );
    assert_eq!(messages[1]["result"]["content"][0]["text"], "complete");
    Ok(())
}

#[tokio::test]
async fn invalid_progress_tokens_are_rejected_before_dispatch() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ProgressServer)).await?;
    let mut params = modern_params(json!({ "name": "progress", "arguments": {} }));
    params["_meta"]["progressToken"] = Value::Bool(true);
    let response = crate::spec::support::post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "bad-progress", "method": "tools/call",
            "params": params
        }),
    )
    .await?;
    assert_eq!(response.body["error"]["code"], -32602);
    Ok(())
}
