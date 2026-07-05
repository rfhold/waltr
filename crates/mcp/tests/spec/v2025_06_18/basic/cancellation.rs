use mcp::protocol::{JsonRpcId, cancellation_params};
use serde_json::json;

#[test]
fn cancellation_params_use_request_id_and_reason_shape() {
    assert_eq!(
        cancellation_params(
            &JsonRpcId::String("request-123".to_owned()),
            "User requested cancellation"
        ),
        json!({
            "requestId": "request-123",
            "reason": "User requested cancellation"
        })
    );
    assert_eq!(
        cancellation_params(&JsonRpcId::Number(42), "Request timed out"),
        json!({
            "requestId": 42,
            "reason": "Request timed out"
        })
    );
}
