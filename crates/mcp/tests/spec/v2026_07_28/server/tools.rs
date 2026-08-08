use std::sync::Arc;

use mcp::{
    McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::json;

use crate::spec::support::{SpecServer, modern_params, post_json};

struct ToolsServer;

impl ServerHandler for ToolsServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-tools-server", "0")
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
                tools: vec![weather_tool_definition(), header_tool_definition()],
                next_cursor: Some("next-tool-page".to_owned()),
            })
        })
    }

    fn call_tool(
        &self,
        call: McpToolCall,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async move {
            if call.name != "get_weather_data" {
                return Err(ServerError::invalid_params(format!(
                    "unknown tool: {}",
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
        })
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
        annotations: Some(json!({ "readOnlyHint": true })),
        meta: Some(json!({ "example.test/tool": "opaque" })),
    }
}

fn header_tool_definition() -> McpToolDefinition {
    McpToolDefinition {
        name: "header_weather".to_owned(),
        title: None,
        description: None,
        icons: Vec::new(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "x-mcp-header": "Location" }
            }
        }),
        output_schema: None,
        annotations: None,
        meta: None,
    }
}

#[tokio::test]
async fn tools_list_returns_metadata_schemas_annotations_and_cursor() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "tools-list", "method": "tools/list",
            "params": modern_params(json!({ "cursor": "first-page" }))
        }),
    )
    .await?;

    assert_eq!(response.body["result"]["resultType"], "complete");
    let tool = &response.body["result"]["tools"][0];
    assert_eq!(tool["name"], "get_weather_data");
    assert_eq!(tool["title"], "Weather Data Retriever");
    assert_eq!(
        tool["description"],
        "Get current weather data for a location"
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["location"]));
    assert_eq!(
        tool["outputSchema"]["required"],
        json!(["temperature", "conditions"])
    );
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert_eq!(tool["_meta"]["example.test/tool"], "opaque");
    assert_eq!(response.body["result"]["nextCursor"], "next-tool-page");
    Ok(())
}

#[tokio::test]
async fn tools_call_returns_content_structured_content_and_error_flag() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "tools-call", "method": "tools/call",
            "params": modern_params(json!({
                "name": "get_weather_data", "arguments": { "location": "New York" }
            }))
        }),
    )
    .await?;

    assert_eq!(response.body["result"]["content"][0]["type"], "text");
    assert_eq!(
        response.body["result"]["structuredContent"],
        json!({ "temperature": 22.5, "conditions": "Partly cloudy" })
    );
    assert_eq!(response.body["result"]["isError"], false);
    Ok(())
}

#[tokio::test]
async fn tools_call_without_name_returns_invalid_params() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "tools-call-invalid", "method": "tools/call",
            "params": modern_params(json!({ "arguments": { "location": "New York" } }))
        }),
    )
    .await?;
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());
    Ok(())
}

#[tokio::test]
async fn tools_call_rejects_mirrored_name_and_parameter_mismatches() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(ToolsServer)).await?;
    let body = json!({
        "jsonrpc": "2.0", "id": "tools-call-headers", "method": "tools/call",
        "params": modern_params(json!({
            "name": "header_weather", "arguments": { "location": "New York" }
        }))
    });
    let client = reqwest::Client::new();
    let request = || {
        client
            .post(server.endpoint())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
            .header("mcp-method", "tools/call")
    };
    let wrong_name = request()
        .header("mcp-name", "another_tool")
        .json(&body)
        .send()
        .await?;
    assert_eq!(wrong_name.status(), reqwest::StatusCode::BAD_REQUEST);
    let wrong_name: serde_json::Value = wrong_name.json().await?;
    assert_eq!(wrong_name["error"]["code"], -32020);
    assert_eq!(wrong_name["error"]["data"]["header"], "mcp-name");

    let wrong_parameter = request()
        .header("mcp-name", "header_weather")
        .header("mcp-param-location", "Boston")
        .json(&body)
        .send()
        .await?;
    assert_eq!(wrong_parameter.status(), reqwest::StatusCode::BAD_REQUEST);
    let wrong_parameter: serde_json::Value = wrong_parameter.json().await?;
    assert_eq!(wrong_parameter["error"]["code"], -32020);
    assert_eq!(
        wrong_parameter["error"]["data"]["header"],
        "mcp-param-Location"
    );
    Ok(())
}
