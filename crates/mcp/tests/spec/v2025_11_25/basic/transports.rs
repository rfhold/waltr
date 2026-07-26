use std::sync::Arc;

use mcp::protocol::MCP_PROTOCOL_VERSION;
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, initialized_session, post_json};

struct TransportServer;

#[mcp::mcp_server(name = "mcp-spec-transport-server", version = "0")]
impl TransportServer {}

#[tokio::test]
async fn streamable_http_accepts_client_notifications_with_accepted_status() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/example",
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.bytes().await?.len(), 0);

    Ok(())
}

#[tokio::test]
async fn streamable_http_accepts_client_responses_with_accepted_status() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "server-request-1",
            "result": {},
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.bytes().await?.len(), 0);

    Ok(())
}

#[tokio::test]
async fn streamable_http_post_requires_a_known_session_after_initialize() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": "session-required",
        "method": "ping",
    });

    let missing = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await?;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let unknown = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", "unknown-session")
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&body)
        .send()
        .await?;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn streamable_http_request_returns_json_content_type_for_single_response() -> mcp::Result<()>
{
    let server = SpecServer::start(Arc::new(TransportServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "transport-ping-1",
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
    assert_eq!(response.body["id"], "transport-ping-1");
    assert_eq!(response.body["result"], json!({}));

    Ok(())
}

#[tokio::test]
async fn streamable_http_post_requires_json_and_sse_accept_header() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "transport-accept-1",
            "method": "ping",
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);

    Ok(())
}

#[tokio::test]
async fn streamable_http_rejects_unsupported_protocol_version_header() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2024-11-05")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "transport-version-1",
            "method": "ping",
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn streamable_http_get_without_session_is_bad_request() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;

    let response = reqwest::Client::new()
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    Ok(())
}
