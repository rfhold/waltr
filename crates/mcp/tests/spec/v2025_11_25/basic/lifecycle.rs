use std::{sync::Arc, time::Duration};

use mcp::{
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    protocol::MCP_PROTOCOL_VERSION,
    server::{ServerCapabilities, ServerHandler, ServerInfo},
};
use reqwest::StatusCode;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::spec::support::{SpecServer, post_json};

struct LifecycleServer {
    notifications: Arc<Mutex<Vec<String>>>,
    instructions: Option<String>,
}

impl Default for LifecycleServer {
    fn default() -> Self {
        Self {
            notifications: Arc::new(Mutex::new(Vec::new())),
            instructions: None,
        }
    }
}

impl LifecycleServer {
    fn with_instructions(instructions: impl Into<String>) -> Self {
        Self {
            notifications: Arc::new(Mutex::new(Vec::new())),
            instructions: Some(instructions.into()),
        }
    }
}

impl ServerHandler for LifecycleServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-spec-lifecycle-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
            .tools()
            .prompts()
            .resources()
            .logging()
    }

    fn instructions(&self) -> Option<String> {
        self.instructions.clone()
    }

    fn handle_notification(
        &self,
        notification: mcp::protocol::JsonRpcNotification,
        _context: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<()> {
        let method = notification.method;
        let notifications = self.notifications.clone();
        Box::pin(async move {
            notifications.lock().await.push(method);
        })
    }
}

#[derive(Default)]
struct LifecycleClient;

impl McpClientHandler for LifecycleClient {}

#[tokio::test]
async fn initialize_returns_protocol_capabilities_and_server_info() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LifecycleServer::default())).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "roots": { "listChanged": true },
                    "sampling": {},
                    "elicitation": {},
                },
                "clientInfo": {
                    "name": "mcp-spec-client",
                    "title": "MCP Spec Client",
                    "version": "1.0.0",
                },
            },
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], 1);
    assert_eq!(
        response.body["result"]["protocolVersion"],
        MCP_PROTOCOL_VERSION
    );
    assert_eq!(
        response.body["result"]["serverInfo"],
        json!({ "name": "mcp-spec-lifecycle-server", "version": "1.0.0" })
    );
    assert_eq!(
        response.body["result"]["capabilities"],
        json!({
            "logging": {},
            "prompts": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
            "tools": { "listChanged": false },
        })
    );
    assert!(response.body["result"].get("instructions").is_none());
    assert!(response.body.get("error").is_none());

    Ok(())
}

#[tokio::test]
async fn initialize_returns_server_instructions_when_available() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LifecycleServer::with_instructions(
        "Use this server for lifecycle-marker requests.",
    )))
    .await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "initialize-with-instructions",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "mcp-spec-client", "version": "1.0.0" },
            },
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.body["result"]["instructions"],
        "Use this server for lifecycle-marker requests."
    );

    Ok(())
}

#[tokio::test]
async fn client_exposes_server_instructions() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LifecycleServer::with_instructions(
        "Prefer lifecycle-marker tools.",
    )))
    .await?;

    let client = McpClient::connect(
        "mcp-spec-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(LifecycleClient),
        McpClientOptions::new("mcp-spec-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await?;

    assert_eq!(
        client.instructions(),
        Some("Prefer lifecycle-marker tools.")
    );

    client.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn feature_requests_before_initialization_are_rejected() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LifecycleServer::default())).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "pre-init-tools",
            "method": "tools/list",
        }))
        .send()
        .await?;
    let status = response.status();
    let body = response.json::<serde_json::Value>().await?;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "pre-init-tools");
    assert_eq!(body["error"]["code"], -32600);
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("not completed initialization")
    );

    Ok(())
}

#[tokio::test]
async fn repeated_initialize_on_same_session_is_rejected() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(LifecycleServer::default())).await?;
    let client = reqwest::Client::new();

    let first = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "initialize-1",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "mcp-spec-client", "version": "1.0.0" },
            },
        }))
        .send()
        .await?;
    let session_id = first
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("session id")
        .to_owned();
    drop(first.json::<serde_json::Value>().await?);

    let second = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "initialize-2",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "mcp-spec-client", "version": "1.0.0" },
            },
        }))
        .send()
        .await?;

    assert_eq!(second.status(), StatusCode::OK);
    let body = second.json::<serde_json::Value>().await?;
    assert_eq!(body["id"], "initialize-2");
    assert_eq!(body["error"]["code"], -32600);

    Ok(())
}

#[tokio::test]
async fn initialized_notification_is_sent_after_client_initializes() -> mcp::Result<()> {
    let handler = Arc::new(LifecycleServer::default());
    let server = SpecServer::start(handler.clone()).await?;

    let client = McpClient::connect(
        "mcp-spec-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(LifecycleClient),
        McpClientOptions::new("mcp-spec-client", "1.0.0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await?;

    let notifications = handler.notifications.lock().await;
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0], "notifications/initialized");

    client.shutdown().await;
    Ok(())
}
