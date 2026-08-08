use std::sync::Arc;

use mcp::server::{
    BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult,
};
use serde_json::{Value, json};

use super::super::support::{SpecServer, modern_params};

struct SubscriptionServer;

impl ServerHandler for SubscriptionServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("subscription-spec", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        let mut capabilities = ServerCapabilities::new();
        capabilities.tools = Some(json!({ "listChanged": true }));
        capabilities.resources = Some(json!({ "subscribe": false, "listChanged": false }));
        capabilities
    }

    fn listen(&self, filter: Value, context: ServerContext) -> BoxFuture<ServerResult<()>> {
        Box::pin(async move {
            assert_eq!(filter, json!({ "toolsListChanged": true }));
            context
                .notify("notifications/tools/list_changed", Some(json!({})))
                .await
                .map_err(|error| mcp::server::ServerError::internal(error.to_string()))?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn acknowledgment_is_first_and_contains_only_supported_filters() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(SubscriptionServer)).await?;
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "subscriptions/listen")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "subscription-1",
            "method": "subscriptions/listen",
            "params": modern_params(json!({
                "notifications": {
                    "toolsListChanged": true,
                    "resourcesListChanged": true,
                    "resourceSubscriptions": [{ "uri": "file:///unsupported" }]
                }
            }))
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await?;
    let messages = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0]["method"],
        "notifications/subscriptions/acknowledged"
    );
    assert_eq!(
        messages[0]["params"]["notifications"],
        json!({ "toolsListChanged": true })
    );
    assert_eq!(messages[1]["method"], "notifications/tools/list_changed");
    assert_eq!(messages[2]["result"]["resultType"], "complete");
    for message in &messages {
        let meta = message
            .pointer(if message.get("method").is_some() {
                "/params/_meta"
            } else {
                "/result/_meta"
            })
            .expect("subscription metadata");
        assert_eq!(
            meta["io.modelcontextprotocol/subscriptionId"],
            "subscription-1"
        );
    }
    Ok(())
}

#[tokio::test]
async fn listen_requires_notifications_filter() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(SubscriptionServer)).await?;
    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "subscriptions/listen")
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "subscriptions/listen",
            "params": modern_params(json!({}))
        }))
        .send()
        .await?;
    let body = response.text().await?;
    let message: Value = serde_json::from_str(
        body.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("SSE data"),
    )?;
    assert_eq!(message["error"]["code"], -32602);
    Ok(())
}
