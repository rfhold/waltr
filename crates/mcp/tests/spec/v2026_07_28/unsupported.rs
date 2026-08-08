use std::{sync::Arc, time::Duration};

use axum::{Json, Router, routing::post};
use mcp::{
    McpRoot, McpToolCall,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    protocol::MCP_PROTOCOL_VERSION,
    server::{ServerCapabilities, ServerHandler, ServerInfo},
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::spec::support::{SpecServer, post_json, request_params};

struct UnsupportedServer;

impl ServerHandler for UnsupportedServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("unsupported-spec", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
    }
}

struct RootsOnlyClient;

impl McpClientHandler for RootsOnlyClient {
    fn roots(&self) -> Vec<McpRoot> {
        vec![McpRoot::new("file:///workspace", None)]
    }
}

#[tokio::test]
async fn discovery_omits_every_unsupported_capability() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(UnsupportedServer)).await?;
    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0", "id": "unsupported-capabilities",
            "method": "server/discover", "params": request_params(&Value::Null)
        }),
    )
    .await?;
    let capabilities = response.body["result"]["capabilities"]
        .as_object()
        .expect("capabilities object");
    for capability in ["sampling", "elicitation", "tasks", "resourceSubscriptions"] {
        assert!(
            !capabilities.contains_key(capability),
            "unexpected {capability}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_methods_return_method_not_found() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(UnsupportedServer)).await?;
    for method in [
        "sampling/createMessage",
        "elicitation/create",
        "tasks/list",
        "tasks/get",
        "tasks/cancel",
        "resources/subscribe",
        "resources/unsubscribe",
        "notifications/resources/updated",
    ] {
        let response = post_json(
            &server.endpoint(),
            json!({
                "jsonrpc": "2.0", "id": method, "method": method,
                "params": request_params(&Value::Null)
            }),
        )
        .await?;
        assert_eq!(response.status, reqwest::StatusCode::NOT_FOUND, "{method}");
        assert_eq!(response.body["error"]["code"], -32601, "{method}");
    }
    Ok(())
}

#[tokio::test]
async fn client_rejects_non_roots_mrtr_input_requests() -> mcp::Result<()> {
    for method in ["sampling/createMessage", "elicitation/create"] {
        let app = Router::new()
            .route("/mcp", post(non_roots_mrtr_server))
            .with_state(method.to_owned());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/mcp", listener.local_addr()?);
        tokio::spawn(async move {
            drop(axum::serve(listener, app).await);
        });
        let cancellation = CancellationToken::new();
        let client = McpClient::connect(
            "unsupported-mrtr-spec",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            Arc::new(RootsOnlyClient),
            McpClientOptions::new("spec-client", "1", Duration::from_secs(2)),
            cancellation.clone(),
        )
        .await?;
        let error = client
            .call_tool(
                McpToolCall::new("unsupported-input", json!({})),
                cancellation,
            )
            .await
            .expect_err("non-roots MRTR must fail");
        assert!(
            error.to_string().contains("unsupported MCP input request"),
            "unexpected error for {method}: {error}"
        );
        client.shutdown().await?;
    }
    Ok(())
}

async fn non_roots_mrtr_server(
    axum::extract::State(method): axum::extract::State<String>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let result = match request["method"].as_str().expect("method") {
        "server/discover" => json!({
            "resultType": "complete", "supportedVersions": [MCP_PROTOCOL_VERSION],
            "capabilities": { "tools": {} }, "ttlMs": 0, "cacheScope": "private"
        }),
        "tools/call" => json!({
            "resultType": "input_required",
            "inputRequests": { "unsupported": { "method": method } },
            "requestState": "unsupported-state"
        }),
        method => panic!("unexpected method {method}"),
    };
    Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
}
