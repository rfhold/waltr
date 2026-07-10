use std::sync::Arc;

use mcp::protocol::{
    JsonRpcId, JsonRpcInbound, json_rpc_error, json_rpc_notification, json_rpc_request,
    json_rpc_success, parse_json_rpc_message,
};
use reqwest::StatusCode;
use serde_json::json;

use crate::spec::support::{SpecServer, post_json};

struct JsonRpcServer;

#[mcp::mcp_server(name = "mcp-spec-json-rpc-server", version = "0")]
impl JsonRpcServer {}

#[test]
fn request_notification_and_response_builders_emit_spec_shapes() {
    assert_eq!(
        json_rpc_request(
            JsonRpcId::String("request-1".to_owned()),
            "example/request",
            Some(json!({ "value": true })),
        ),
        json!({
            "jsonrpc": "2.0",
            "id": "request-1",
            "method": "example/request",
            "params": { "value": true },
        })
    );
    assert_eq!(
        json_rpc_notification("example/notification", None),
        json!({
            "jsonrpc": "2.0",
            "method": "example/notification",
        })
    );
    assert_eq!(
        json_rpc_success(JsonRpcId::Number(7), json!({ "ok": true })),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": { "ok": true },
        })
    );
    assert_eq!(
        json_rpc_error(
            JsonRpcId::String("bad".to_owned()),
            -32602,
            "invalid params"
        ),
        json!({
            "jsonrpc": "2.0",
            "id": "bad",
            "error": {
                "code": -32602,
                "message": "invalid params",
            },
        })
    );
}

#[test]
fn parser_classifies_json_rpc_message_kinds() -> mcp::Result<()> {
    let request = parse_json_rpc_message(json!({
        "jsonrpc": "2.0",
        "id": "request-1",
        "method": "example/request",
        "params": { "value": true },
    }))?;
    match request {
        JsonRpcInbound::Request(request) => {
            assert_eq!(request.id, JsonRpcId::String("request-1".to_owned()));
            assert_eq!(request.method, "example/request");
            assert_eq!(request.params, Some(json!({ "value": true })));
        }
        other => panic!("expected request, got {other:?}"),
    }

    let notification = parse_json_rpc_message(json!({
        "jsonrpc": "2.0",
        "method": "example/notification",
    }))?;
    match notification {
        JsonRpcInbound::Notification(notification) => {
            assert_eq!(notification.method, "example/notification");
            assert!(notification.params.is_none());
        }
        other => panic!("expected notification, got {other:?}"),
    }

    let response = parse_json_rpc_message(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "result": {},
    }))?;
    match response {
        JsonRpcInbound::Response(response) => {
            assert_eq!(response.id, JsonRpcId::Number(7));
            assert_eq!(response.result, Some(json!({})));
            assert!(response.error.is_none());
        }
        other => panic!("expected response, got {other:?}"),
    }

    Ok(())
}

#[test]
fn parser_rejects_malformed_json_rpc_messages() {
    let cases = [
        json!({
            "id": "missing-version",
            "method": "example/request",
        }),
        json!({
            "jsonrpc": "1.0",
            "id": "wrong-version",
            "method": "example/request",
        }),
        json!({
            "jsonrpc": "2.0",
            "id": null,
            "method": "example/request",
        }),
        json!({
            "jsonrpc": "2.0",
            "method": 7,
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "example/request",
            "result": {},
        }),
        json!({
            "jsonrpc": "2.0",
            "id": "ambiguous-response",
            "result": {},
            "error": {
                "code": -32603,
                "message": "internal error",
            },
        }),
        json!({
            "jsonrpc": "2.0",
            "id": "empty-response",
        }),
    ];

    for message in cases {
        assert!(
            parse_json_rpc_message(message.clone()).is_err(),
            "message should be rejected: {message}"
        );
    }
}

#[tokio::test]
async fn http_error_response_uses_json_rpc_error_shape() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(JsonRpcServer)).await?;

    let response = post_json(
        &server.endpoint(),
        json!({
            "jsonrpc": "2.0",
            "id": "unknown-1",
            "method": "example/unknown",
        }),
    )
    .await?;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body["jsonrpc"], "2.0");
    assert_eq!(response.body["id"], "unknown-1");
    assert_eq!(response.body["error"]["code"], -32601);
    assert!(response.body["error"]["message"].as_str().is_some());
    assert!(response.body.get("result").is_none());

    Ok(())
}
