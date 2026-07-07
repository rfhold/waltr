use std::sync::Arc;

use mcp::{
    McpIcon, McpToolTaskSupport,
    protocol::MCP_PROTOCOL_VERSION,
    server::{ServerCapabilities, ServerHandler, ServerInfo},
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct BaselineServer;

impl ServerHandler for BaselineServer {
    fn server_info(&self) -> ServerInfo {
        let mut icon = McpIcon::new("https://example.test/icon.svg");
        icon.mime_type = Some("image/svg+xml".to_owned());
        icon.sizes = Some("any".to_owned());
        icon.theme = Some("light".to_owned());
        ServerInfo::new("mcp-2025-11-25-baseline", "1.0.0")
            .with_description("2025-11-25 baseline server")
            .with_icons(vec![icon])
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
            .tools()
            .tasks(json!({ "list": {}, "cancel": {}, "requests": { "tools/call": {} } }))
    }
}

#[tokio::test]
async fn initialize_returns_2025_11_25_metadata_and_capabilities() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(BaselineServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "initialize-2025-11-25",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "spec-test", "version": "0" }
            }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["protocolVersion"],
        MCP_PROTOCOL_VERSION
    );
    assert_eq!(
        response.body["result"]["serverInfo"]["description"],
        "2025-11-25 baseline server"
    );
    assert_eq!(
        response.body["result"]["serverInfo"]["icons"][0]["src"],
        "https://example.test/icon.svg"
    );
    assert!(response.body["result"]["capabilities"]["tasks"].is_object());

    Ok(())
}

#[test]
fn feature_icons_and_tool_task_support_round_trip() -> mcp::Result<()> {
    let value = json!({
        "tools": [{
            "name": "long_running_tool",
            "icons": [{ "src": "https://example.test/tool.png", "mimeType": "image/png" }],
            "inputSchema": { "type": "object" },
            "execution": { "taskSupport": "optional" }
        }]
    });

    let page = mcp::McpToolList::parse(&value)?;

    assert_eq!(page.tools[0].icons[0].src, "https://example.test/tool.png");
    assert_eq!(
        page.tools[0].task_support,
        Some(McpToolTaskSupport::Optional)
    );
    assert_eq!(
        page.to_value()["tools"][0]["execution"]["taskSupport"],
        "optional"
    );
    assert_eq!(
        page.to_value()["tools"][0]["icons"][0]["mimeType"],
        "image/png"
    );

    Ok(())
}
