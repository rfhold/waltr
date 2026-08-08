use std::sync::Arc;

use mcp::{
    McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
    },
};
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, modern_params, sse_messages};

struct LoggingServer;

impl ServerHandler for LoggingServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-logging-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools().logging()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: vec![McpToolDefinition {
                    name: "log".to_owned(),
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
            context
                .log("debug", Some("mcp-spec"), json!({ "hidden": true }))
                .await
                .map_err(|error| mcp::server::ServerError::internal(error.to_string()))?;
            context
                .log("info", Some("mcp-spec"), json!({ "called": true }))
                .await
                .map_err(|error| mcp::server::ServerError::internal(error.to_string()))?;
            Ok(McpToolResult::text("logged"))
        })
    }
}

#[tokio::test]
async fn request_scoped_logging_emits_exact_fields_and_applies_threshold() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LoggingServer)).await?;
    let mut params = modern_params(json!({ "name": "log", "arguments": {} }));
    params["_meta"]["io.modelcontextprotocol/logLevel"] = json!("info");
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "log")
        .json(&json!({
            "jsonrpc": "2.0", "id": "logging", "method": "tools/call", "params": params
        }))
        .send()
        .await?;
    let messages = sse_messages(&response.text().await?)?;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["method"], "notifications/message");
    assert_eq!(
        messages[0]["params"],
        json!({ "level": "info", "logger": "mcp-spec", "data": { "called": true } })
    );
    assert_eq!(messages[1]["result"]["content"][0]["text"], "logged");
    Ok(())
}

#[tokio::test]
async fn invalid_request_log_level_fails_the_originating_request() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LoggingServer)).await?;
    let mut params = modern_params(json!({ "name": "log", "arguments": {} }));
    params["_meta"]["io.modelcontextprotocol/logLevel"] = Value::String("verbose".to_owned());
    let response = crate::spec::support::post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "bad-log-level", "method": "tools/call", "params": params
        }),
    )
    .await?;
    assert_eq!(response.body["error"]["code"], -32602);
    Ok(())
}
