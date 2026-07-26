#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use mcp::{
    McpToolCall, McpToolDefinition, McpToolResult,
    server::{ServerContext, ServerError, ServerResult},
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct ToolsServer;

#[mcp::mcp_server(
    name = "mcp-spec-tools-server",
    version = "0",
    tools_next_cursor = "next-tool-page"
)]
impl ToolsServer {
    #[tool(name = "get_weather_data", definition = weather_tool_definition())]
    fn get_weather_data(call: McpToolCall, _context: ServerContext) -> ServerResult<McpToolResult> {
        if call.name != "get_weather_data" {
            return Err(ServerError::invalid_params(format!(
                "Unknown tool: {}",
                call.name
            )));
        }
        Ok(McpToolResult::new(json!({
            "content": [{
                "type": "text",
                "text": "{\"temperature\":22.5,\"conditions\":\"Partly cloudy\"}"
            }],
            "structuredContent": {
                "temperature": 22.5,
                "conditions": "Partly cloudy"
            },
            "isError": false
        })))
    }
}

fn weather_tool_definition() -> McpToolDefinition {
    McpToolDefinition {
        name: "get_weather_data".to_owned(),
        title: Some("Weather Data Retriever".to_owned()),
        description: Some("Get current weather data for a location".to_owned()),
        icons: Vec::new(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "City name" }
            },
            "required": ["location"]
        }),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "temperature": { "type": "number" },
                "conditions": { "type": "string" }
            },
            "required": ["temperature", "conditions"]
        })),
        task_support: None,
        annotations: None,
    }
}

#[tokio::test]
async fn initialize_declares_tools_capability() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "initialize-tools",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "spec-test", "version": "0" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    Ok(())
}

#[tokio::test]
async fn tools_list_returns_tool_metadata_schemas_and_next_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "tools-list",
            "method": "tools/list",
            "params": { "cursor": "first-page" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "tools-list");
    assert_eq!(
        response.body["result"]["tools"][0]["name"],
        "get_weather_data"
    );
    assert_eq!(
        response.body["result"]["tools"][0]["title"],
        "Weather Data Retriever"
    );
    assert_eq!(
        response.body["result"]["tools"][0]["inputSchema"]["required"],
        json!(["location"])
    );
    assert_eq!(
        response.body["result"]["tools"][0]["outputSchema"]["required"],
        json!(["temperature", "conditions"])
    );
    assert_eq!(response.body["result"]["nextCursor"], "next-tool-page");
    assert!(response.body.get("error").is_none());
    Ok(())
}

#[tokio::test]
async fn tools_call_returns_content_structured_content_and_error_flag() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "tools-call",
            "method": "tools/call",
            "params": {
                "name": "get_weather_data",
                "arguments": { "location": "New York" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["result"]["content"][0]["type"], "text");
    assert_eq!(
        response.body["result"]["structuredContent"],
        json!({ "temperature": 22.5, "conditions": "Partly cloudy" })
    );
    assert_eq!(response.body["result"]["isError"], false);
    assert!(response.body.get("error").is_none());
    Ok(())
}

#[tokio::test]
async fn tools_call_without_name_returns_invalid_params_error() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "tools-call-invalid",
            "method": "tools/call",
            "params": { "arguments": { "location": "New York" } }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());
    Ok(())
}
