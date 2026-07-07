use std::sync::Arc;

use mcp::server::{ServerCapabilities, ServerHandler, ServerInfo};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct PingServer;

impl ServerHandler for PingServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-ping-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
    }
}

#[tokio::test]
async fn ping_returns_empty_result_object() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PingServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "ping-1",
            "method": "ping",
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "ping-1");
    assert_eq!(response.body["result"], json!({}));
    assert!(response.body.get("error").is_none());

    Ok(())
}

#[tokio::test]
async fn ping_preserves_numeric_request_id() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(PingServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": 23,
            "method": "ping",
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], 23);
    assert_eq!(response.body["result"], json!({}));
    assert!(response.body.get("error").is_none());

    Ok(())
}
