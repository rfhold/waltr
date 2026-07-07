use std::sync::{Arc, Mutex};

use mcp::server::{
    BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::spec::support::{SpecServer, initialized_session, post_json};

struct LoggingServer {
    seen_levels: Arc<Mutex<Vec<String>>>,
}

impl LoggingServer {
    fn new(seen_levels: Arc<Mutex<Vec<String>>>) -> Self {
        Self { seen_levels }
    }
}

impl ServerHandler for LoggingServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-logging-server", "0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().logging()
    }

    fn set_log_level(&self, level: String, context: ServerContext) -> BoxFuture<ServerResult<()>> {
        let seen_levels = Arc::clone(&self.seen_levels);
        Box::pin(async move {
            seen_levels.lock().expect("levels lock").push(level);
            context
                .log(
                    "error",
                    Some("database"),
                    json!({ "error": "Connection failed", "details": { "port": 5432 } }),
                )
                .await
                .map_err(|error| mcp::server::ServerError::internal(error.to_string()))?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn initialize_advertises_logging_capability() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LoggingServer::new(Arc::new(Mutex::new(
        Vec::new(),
    )))))
    .await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "init-logging",
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
        response.body["result"]["capabilities"]["logging"],
        json!({})
    );

    Ok(())
}

#[tokio::test]
async fn logging_set_level_returns_empty_result_and_can_emit_message_notification()
-> mcp::Result<()> {
    let seen_levels = Arc::new(Mutex::new(Vec::new()));
    let server = SpecServer::start(Arc::new(LoggingServer::new(Arc::clone(&seen_levels)))).await?;
    let session_id = initialized_session(&server.endpoint()).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "set-log-level",
            "method": "logging/setLevel",
            "params": { "level": "info" }
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

    let body = response.text().await?;
    let messages = sse_json_messages(&body);
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0],
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": "error",
                "logger": "database",
                "data": { "error": "Connection failed", "details": { "port": 5432 } }
            }
        })
    );
    assert_eq!(messages[1]["jsonrpc"], "2.0");
    assert_eq!(messages[1]["id"], "set-log-level");
    assert_eq!(messages[1]["result"], json!({}));
    assert_eq!(
        seen_levels.lock().expect("levels lock").as_slice(),
        &["info".to_owned()]
    );

    Ok(())
}

#[tokio::test]
async fn logging_set_level_requires_string_level() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LoggingServer::new(Arc::new(Mutex::new(
        Vec::new(),
    )))))
    .await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "bad-log-level",
            "method": "logging/setLevel",
            "params": { "level": 1 }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());

    Ok(())
}

#[tokio::test]
async fn logging_set_level_rejects_unknown_level_before_handler() -> mcp::Result<()> {
    let seen_levels = Arc::new(Mutex::new(Vec::new()));
    let server = SpecServer::start(Arc::new(LoggingServer::new(Arc::clone(&seen_levels)))).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "unknown-log-level",
            "method": "logging/setLevel",
            "params": { "level": "verbose" }
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["error"]["code"], -32602);
    assert!(response.body.get("result").is_none());
    assert!(seen_levels.lock().expect("levels lock").is_empty());

    Ok(())
}

fn sse_json_messages(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| serde_json::from_str(line).expect("SSE data is JSON"))
        .collect()
}
