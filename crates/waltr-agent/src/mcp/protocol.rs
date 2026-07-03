use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{LlmError, Result};

pub(in crate::mcp) const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub(in crate::mcp) enum JsonRpcId {
    Number(u64),
    String(String),
}

impl fmt::Display for JsonRpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(id) => write!(formatter, "{id}"),
            Self::String(id) => formatter.write_str(id),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(in crate::mcp) struct JsonRpcError {
    pub(in crate::mcp) code: i64,
    pub(in crate::mcp) message: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(in crate::mcp) data: Option<Value>,
}

#[derive(Debug, Clone)]
pub(in crate::mcp) enum JsonRpcInbound {
    Response(JsonRpcResponse),
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
}

#[derive(Debug, Clone)]
pub(in crate::mcp) struct JsonRpcResponse {
    pub(in crate::mcp) id: JsonRpcId,
    pub(in crate::mcp) result: Option<Value>,
    pub(in crate::mcp) error: Option<JsonRpcError>,
}

#[derive(Debug, Clone)]
pub(in crate::mcp) struct JsonRpcRequest {
    pub(in crate::mcp) id: JsonRpcId,
    pub(in crate::mcp) method: String,
    #[allow(dead_code)]
    pub(in crate::mcp) params: Option<Value>,
}

#[derive(Debug, Clone)]
pub(in crate::mcp) struct JsonRpcNotification {
    pub(in crate::mcp) method: String,
    pub(in crate::mcp) params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RawJsonRpcMessage {
    #[serde(default)]
    id: Option<JsonRpcId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct OutboundJsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: JsonRpcId,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct OutboundJsonRpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct OutboundJsonRpcResponse {
    jsonrpc: &'static str,
    id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<OutboundJsonRpcError>,
}

#[derive(Debug, Serialize)]
struct OutboundJsonRpcError {
    code: i64,
    message: String,
}

pub(in crate::mcp) fn json_rpc_request(
    id: JsonRpcId,
    method: &str,
    params: Option<Value>,
) -> Value {
    serde_json::to_value(OutboundJsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .expect("request serializes")
}

pub(in crate::mcp) fn json_rpc_notification(method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(OutboundJsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    })
    .expect("notification serializes")
}

pub(in crate::mcp) fn json_rpc_success(id: JsonRpcId, result: Value) -> Value {
    serde_json::to_value(OutboundJsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    })
    .expect("response serializes")
}

pub(in crate::mcp) fn json_rpc_method_not_found(id: JsonRpcId, method: &str) -> Value {
    serde_json::to_value(OutboundJsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(OutboundJsonRpcError {
            code: -32601,
            message: format!("method not found: {method}"),
        }),
    })
    .expect("response serializes")
}

pub(in crate::mcp) fn json_rpc_result(response: JsonRpcResponse) -> Result<Value> {
    if let Some(error) = response.error {
        return Err(protocol_error(format!(
            "JSON-RPC error {}: {}",
            error.code, error.message
        )));
    }
    Ok(response.result.unwrap_or(Value::Null))
}

pub(in crate::mcp) fn parse_json_rpc_message(value: Value) -> Result<JsonRpcInbound> {
    let raw: RawJsonRpcMessage = serde_json::from_value(value)?;
    if let Some(method) = raw.method {
        return Ok(match raw.id {
            Some(id) => JsonRpcInbound::Request(JsonRpcRequest {
                id,
                method,
                params: raw.params,
            }),
            None => JsonRpcInbound::Notification(JsonRpcNotification {
                method,
                params: raw.params,
            }),
        });
    }
    let Some(id) = raw.id else {
        return Err(protocol_error("MCP JSON-RPC message missing id or method"));
    };
    Ok(JsonRpcInbound::Response(JsonRpcResponse {
        id,
        result: raw.result,
        error: raw.error,
    }))
}

pub(in crate::mcp) fn parse_json_rpc_str(message: &str) -> Result<JsonRpcInbound> {
    parse_json_rpc_message(serde_json::from_str(message)?)
}

pub(in crate::mcp) fn next_id() -> JsonRpcId {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    JsonRpcId::Number(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

pub(in crate::mcp) fn cancellation_params(id: &JsonRpcId, reason: &str) -> Value {
    json!({
        "requestId": id,
        "reason": reason,
    })
}

pub(in crate::mcp) fn protocol_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "mcp",
        message: message.into(),
    }
}
