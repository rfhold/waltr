use mcp::protocol::{
    JsonRpcId, JsonRpcInbound, JsonRpcResponse, cancellation_params, json_rpc_error,
    json_rpc_notification, json_rpc_request, json_rpc_result, json_rpc_success,
    parse_json_rpc_message,
};
use serde_json::json;

#[test]
fn json_rpc_builders_emit_spec_shapes() {
    assert_eq!(
        json_rpc_request(JsonRpcId::Number(-7), "tools/list", Some(json!({}))),
        json!({ "jsonrpc": "2.0", "id": -7, "method": "tools/list", "params": {} })
    );
    assert_eq!(
        json_rpc_notification("notifications/cancelled", Some(json!({ "requestId": "x" }))),
        json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": "x" } })
    );
    assert_eq!(
        json_rpc_success(
            JsonRpcId::String("x".to_owned()),
            json!({ "resultType": "complete" })
        ),
        json!({ "jsonrpc": "2.0", "id": "x", "result": { "resultType": "complete" } })
    );
    assert_eq!(
        json_rpc_error(JsonRpcId::Number(1), -32602, "bad params"),
        json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "bad params" } })
    );
}

#[test]
fn parser_accepts_signed_and_string_ids_and_classifies_notifications() {
    assert!(matches!(
        parse_json_rpc_message(json!({ "jsonrpc": "2.0", "id": -1, "method": "server/discover", "params": {} })).unwrap(),
        JsonRpcInbound::Request(request) if request.id == JsonRpcId::Number(-1)
    ));
    assert!(matches!(
        parse_json_rpc_message(json!({ "jsonrpc": "2.0", "id": "request", "result": {} })).unwrap(),
        JsonRpcInbound::Response(response) if response.id == JsonRpcId::String("request".to_owned())
    ));
    assert!(matches!(
        parse_json_rpc_message(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" }))
            .unwrap(),
        JsonRpcInbound::Notification(_)
    ));
}

#[test]
fn parser_rejects_null_fractional_and_ambiguous_messages() {
    for value in [
        json!({ "jsonrpc": "2.0", "id": null, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 1.5, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 1, "result": {}, "error": { "code": -1, "message": "x" } }),
        json!({ "jsonrpc": "1.0", "id": 1, "method": "tools/list" }),
    ] {
        assert!(parse_json_rpc_message(value).is_err());
    }
}

#[test]
fn result_requires_an_object_and_valid_result_type_shape() {
    let response = |result| JsonRpcResponse {
        id: JsonRpcId::Number(1),
        result: Some(result),
        error: None,
    };
    assert!(json_rpc_result(response(json!(null))).is_err());
    assert!(json_rpc_result(response(json!([]))).is_err());
    assert!(json_rpc_result(response(json!({ "resultType": 7 }))).is_err());
    assert_eq!(
        json_rpc_result(response(json!({ "value": 1 }))).unwrap(),
        json!({ "value": 1 })
    );
}

#[test]
fn cancellation_uses_request_id_and_reason() {
    assert_eq!(
        cancellation_params(&JsonRpcId::String("work".to_owned()), "stopped"),
        json!({ "requestId": "work", "reason": "stopped" })
    );
}
