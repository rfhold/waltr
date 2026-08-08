use std::sync::Arc;

use mcp::server::{ServerCapabilities, ServerHandler, ServerInfo};
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, post_json, request_params};

#[derive(Debug)]
struct DiscoveryServer;

impl ServerHandler for DiscoveryServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("spec-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools().logging()
    }

    fn instructions(&self) -> Option<String> {
        Some("Use the advertised tools.".to_owned())
    }
}

#[tokio::test]
async fn discover_is_stateless_and_returns_modern_envelope() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(DiscoveryServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": request_params(&Value::Null),
        }),
    )
    .await?;
    assert_eq!(response.status, reqwest::StatusCode::OK);
    assert_eq!(response.body["result"]["resultType"], "complete");
    assert_eq!(
        response.body["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );
    assert_eq!(response.body["result"]["ttlMs"], 0);
    assert_eq!(response.body["result"]["cacheScope"], "private");
    assert_eq!(
        response.body["result"]["capabilities"]["tools"],
        json!({ "listChanged": false })
    );
    assert_eq!(
        response.body["result"]["capabilities"]["logging"],
        json!({})
    );
    assert_eq!(
        response.body["result"]["instructions"],
        "Use the advertised tools."
    );
    assert_eq!(
        response.body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "spec-server"
    );
    assert_eq!(
        response.body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"],
        "1.0.0"
    );
    assert!(response.headers.get("mcp-session-id").is_none());
    Ok(())
}

#[tokio::test]
async fn unsupported_version_uses_reserved_error() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(DiscoveryServer)).await?;
    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "server/discover",
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2025-11-25",
            "io.modelcontextprotocol/clientCapabilities": {}
        }}
    });
    let response = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-11-25")
        .header("mcp-method", "server/discover")
        .json(&body)
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await?;
    assert_eq!(body["error"]["code"], -32022);
    assert_eq!(body["error"]["data"]["supported"], json!(["2026-07-28"]));
    assert_eq!(body["error"]["data"]["requested"], "2025-11-25");
    Ok(())
}
