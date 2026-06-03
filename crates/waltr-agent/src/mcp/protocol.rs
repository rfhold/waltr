use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{LlmError, Result};

pub(in crate::mcp) const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Deserialize)]
pub(in crate::mcp) struct JsonRpcMessage {
    pub(in crate::mcp) id: Option<u64>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

pub(in crate::mcp) fn json_rpc_request(id: u64, method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .expect("request serializes")
}

pub(in crate::mcp) fn json_rpc_notification(method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(JsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    })
    .expect("notification serializes")
}

pub(in crate::mcp) fn json_rpc_result(message: JsonRpcMessage) -> Result<Value> {
    if let Some(error) = message.error {
        return Err(protocol_error(format!(
            "JSON-RPC error {}: {}",
            error.code, error.message
        )));
    }
    Ok(message.result.unwrap_or(Value::Null))
}

pub(in crate::mcp) fn next_id() -> u64 {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(in crate::mcp) fn protocol_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "mcp",
        message: message.into(),
    }
}
