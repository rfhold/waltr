use std::sync::Arc;

use mcp::{
    protocol::{JsonRpcId, JsonRpcInbound, json_rpc_success, parse_json_rpc_message},
    server::ServerContext,
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::spec::support::SpecServer;

#[derive(Clone)]
struct MetaServer {
    notification_params: Arc<Mutex<Option<Value>>>,
}

impl Default for MetaServer {
    fn default() -> Self {
        Self {
            notification_params: Arc::new(Mutex::new(None)),
        }
    }
}

#[mcp::mcp_server(name = "mcp-spec-meta-server", version = "0")]
impl MetaServer {
    #[notification]
    async fn handle_notification(
        &self,
        notification: mcp::protocol::JsonRpcNotification,
        _context: ServerContext,
    ) {
        *self.notification_params.lock().await = notification.params;
    }
}

#[test]
fn result_meta_is_preserved_in_json_rpc_success_response() -> mcp::Result<()> {
    let response = json_rpc_success(
        JsonRpcId::String("meta-1".to_owned()),
        json!({
            "_meta": {
                "example.com/client": "opaque",
                "plain_name": 7,
            },
        }),
    );

    let parsed = parse_json_rpc_message(response)?;
    match parsed {
        JsonRpcInbound::Response(response) => {
            assert_eq!(
                response.result,
                Some(json!({
                    "_meta": {
                        "example.com/client": "opaque",
                        "plain_name": 7,
                    },
                }))
            );
        }
        other => panic!("expected response, got {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn notification_params_meta_is_preserved_for_receiver() -> mcp::Result<()> {
    let handler = Arc::new(MetaServer::default());
    let server = SpecServer::start(handler.clone()).await?;

    let response = reqwest::Client::new()
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {
                "_meta": {
                    "example.com/trace-id": "trace-1",
                    "plain_name": true,
                },
            },
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        *handler.notification_params.lock().await,
        Some(json!({
            "_meta": {
                "example.com/trace-id": "trace-1",
                "plain_name": true,
            },
        }))
    );

    Ok(())
}
