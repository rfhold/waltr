use std::{net::SocketAddr, sync::Arc, time::Duration};

use mcp::{
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    protocol::MCP_PROTOCOL_VERSION,
    server::{ServerCapabilities, ServerContext, ServerHandler, ServerInfo, StreamableHttpOptions},
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_util::sync::CancellationToken;

use crate::spec::support::{SpecServer, initialized_session};

struct TransportServer;

impl ServerHandler for TransportServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mcp-2025-11-25-transport", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().logging()
    }

    fn handle_notification(
        &self,
        notification: mcp::protocol::JsonRpcNotification,
        context: ServerContext,
    ) -> mcp::server::BoxFuture<()> {
        Box::pin(async move {
            if notification.method == "notifications/initialized" {
                context
                    .notify(
                        "notifications/message",
                        Some(json!({
                            "level": "info",
                            "data": "queued-for-get-polling"
                        })),
                    )
                    .await
                    .expect("queue notification");
            }
        })
    }
}

#[tokio::test]
async fn streamable_http_get_polls_queued_sse_messages_end_to_end() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = McpClient::connect(
        "polling-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(RecordingClient { tx }),
        McpClientOptions::new("polling-client", "0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await?;

    let received = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("client receives GET-polled notification")
        .expect("notification value");
    assert_eq!(received["data"], "queued-for-get-polling");

    client.shutdown().await;
    Ok(())
}

struct RecordingClient {
    tx: mpsc::UnboundedSender<Value>,
}

impl McpClientHandler for RecordingClient {
    fn log_message(&self, params: Value) {
        drop(self.tx.send(params));
    }
}

#[tokio::test]
async fn streamable_http_delete_terminates_session() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();

    let deleted = client
        .delete(server.endpoint())
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(deleted.status(), StatusCode::ACCEPTED);

    let get_after_delete = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(get_after_delete.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn streamable_http_origin_policy_rejects_invalid_origin() -> mcp::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let router = mcp::server::streamable_http_router_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default()
            .with_origin_policy(|origin| origin == "https://allowed.test"),
    );
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::debug!(%error, "mcp spec server stopped");
        }
    });
    let endpoint = format!("http://{address}/mcp");

    let response = reqwest::Client::new()
        .post(endpoint)
        .header("origin", "https://blocked.test")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "origin-rejected",
            "method": "ping",
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    task.abort();
    Ok(())
}
