use std::{
    fmt,
    sync::atomic::{AtomicI64, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Error, Result};

/// MCP protocol version used by this crate.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// Reserved request metadata key carrying the active protocol revision.
pub const MCP_PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
/// Reserved request metadata key carrying per-request client capabilities.
pub const MCP_CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
/// Reserved request metadata key carrying client implementation information.
pub const MCP_CLIENT_INFO_META_KEY: &str = "io.modelcontextprotocol/clientInfo";
/// Reserved request metadata key carrying the requested log level.
pub const MCP_LOG_LEVEL_META_KEY: &str = "io.modelcontextprotocol/logLevel";
/// Reserved result metadata key carrying server implementation information.
pub const MCP_SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";
/// Reserved notification metadata key carrying a subscription identifier.
pub const MCP_SUBSCRIPTION_ID_META_KEY: &str = "io.modelcontextprotocol/subscriptionId";

/// HTTP/body metadata mismatch error code.
pub const HEADER_MISMATCH: i64 = -32020;
/// Missing required client capability error code.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
/// Unsupported protocol revision error code.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// JSON-RPC request identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric identifier.
    Number(i64),
    /// String identifier.
    String(String),
    /// Null identifier used for errors without a usable request id.
    Null,
}

impl fmt::Display for JsonRpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(id) => write!(formatter, "{id}"),
            Self::String(id) => formatter.write_str(id),
            Self::Null => formatter.write_str("null"),
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    /// Error code.
    pub code: i64,
    /// Error message.
    pub message: String,
    /// Optional error data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Parsed inbound JSON-RPC message.
#[derive(Debug, Clone)]
pub enum JsonRpcInbound {
    /// Response to a prior request.
    Response(JsonRpcResponse),
    /// Server/client request.
    Request(JsonRpcRequest),
    /// Notification.
    Notification(JsonRpcNotification),
}

/// JSON-RPC response.
#[derive(Debug, Clone)]
pub struct JsonRpcResponse {
    /// Request id.
    pub id: JsonRpcId,
    /// Success result.
    pub result: Option<Value>,
    /// Error result.
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC request.
#[derive(Debug, Clone)]
pub struct JsonRpcRequest {
    /// Request id.
    pub id: JsonRpcId,
    /// Method name.
    pub method: String,
    /// Optional params.
    pub params: Option<Value>,
}

/// JSON-RPC notification.
#[derive(Debug, Clone)]
pub struct JsonRpcNotification {
    /// Method name.
    pub method: String,
    /// Optional params.
    pub params: Option<Value>,
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
    error: Option<JsonRpcError>,
}

/// Build a JSON-RPC request value.
///
/// # Panics
///
/// Panics only if the internal JSON-RPC request representation cannot serialize.
#[must_use]
pub fn json_rpc_request(id: JsonRpcId, method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(OutboundJsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .expect("request serializes")
}

/// Build a JSON-RPC notification value.
///
/// # Panics
///
/// Panics only if the internal JSON-RPC notification representation cannot serialize.
#[must_use]
pub fn json_rpc_notification(method: &str, params: Option<Value>) -> Value {
    serde_json::to_value(OutboundJsonRpcNotification {
        jsonrpc: "2.0",
        method,
        params,
    })
    .expect("notification serializes")
}

/// Build a JSON-RPC success response value.
///
/// # Panics
///
/// Panics only if the internal JSON-RPC response representation cannot serialize.
#[must_use]
pub fn json_rpc_success(id: JsonRpcId, result: Value) -> Value {
    serde_json::to_value(OutboundJsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    })
    .expect("response serializes")
}

/// Build a JSON-RPC error response value.
///
/// # Panics
///
/// Panics only if the internal JSON-RPC response representation cannot serialize.
#[must_use]
pub fn json_rpc_error(id: JsonRpcId, code: i64, message: impl Into<String>) -> Value {
    json_rpc_error_with_data(id, code, message, None)
}

/// Build a JSON-RPC error response value with optional structured data.
///
/// # Panics
///
/// Panics only if the internal JSON-RPC response representation cannot serialize.
#[must_use]
pub fn json_rpc_error_with_data(
    id: JsonRpcId,
    code: i64,
    message: impl Into<String>,
    data: Option<Value>,
) -> Value {
    serde_json::to_value(OutboundJsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data,
        }),
    })
    .expect("response serializes")
}

/// Build a JSON-RPC method-not-found response value.
#[must_use]
pub fn json_rpc_method_not_found(id: JsonRpcId, method: &str) -> Value {
    json_rpc_error(id, -32601, format!("method not found: {method}"))
}

/// Extract the result value or convert JSON-RPC error into protocol error.
///
/// # Errors
///
/// Returns an error when the response contains a JSON-RPC error object.
pub fn json_rpc_result(response: JsonRpcResponse) -> Result<Value> {
    if let Some(error) = response.error {
        return Err(Error::protocol(format!(
            "JSON-RPC error {}: {}",
            error.code, error.message
        )));
    }
    let result = response.result.unwrap_or(Value::Null);
    if !result.is_object() {
        return Err(Error::protocol("MCP result must be an object"));
    }
    if result
        .get("resultType")
        .is_some_and(|result_type| !result_type.is_string())
    {
        return Err(Error::protocol("MCP resultType must be a string"));
    }
    // The active revision requires senders to include resultType, but also requires
    // receivers to treat omission as a complete result.
    Ok(result)
}

/// Parse a JSON-RPC message from a value.
///
/// # Errors
///
/// Returns an error when the value is not a valid JSON-RPC 2.0 message.
// Retain ownership in the public parser API so callers can hand off decoded values.
#[allow(clippy::needless_pass_by_value)]
pub fn parse_json_rpc_message(value: Value) -> Result<JsonRpcInbound> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::protocol("MCP JSON-RPC message must be an object"))?;
    match object.get("jsonrpc") {
        Some(Value::String(version)) if version == "2.0" => {}
        _ => return Err(Error::protocol("MCP JSON-RPC message must use jsonrpc 2.0")),
    }

    let has_id = object.contains_key("id");
    let has_method = object.contains_key("method");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");

    if has_method {
        if has_result || has_error {
            return Err(Error::protocol(
                "MCP JSON-RPC request/notification must not include result or error",
            ));
        }
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::protocol("MCP JSON-RPC method must be a string"))?
            .to_owned();
        let params = object.get("params").cloned();
        if has_id {
            return Ok(JsonRpcInbound::Request(JsonRpcRequest {
                id: parse_json_rpc_id(object.get("id"))?,
                method,
                params,
            }));
        }
        return Ok(JsonRpcInbound::Notification(JsonRpcNotification {
            method,
            params,
        }));
    }

    if !has_id {
        return Err(Error::protocol("MCP JSON-RPC response missing id"));
    }
    if has_result == has_error {
        return Err(Error::protocol(
            "MCP JSON-RPC response must include exactly one of result or error",
        ));
    }
    Ok(JsonRpcInbound::Response(JsonRpcResponse {
        id: parse_json_rpc_id(object.get("id"))?,
        result: object.get("result").cloned(),
        error: object
            .get("error")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?,
    }))
}

fn parse_json_rpc_id(value: Option<&Value>) -> Result<JsonRpcId> {
    let value = value.ok_or_else(|| Error::protocol("MCP JSON-RPC id is missing"))?;
    if value.is_null() {
        return Err(Error::protocol("MCP JSON-RPC id must not be null"));
    }
    serde_json::from_value(value.clone()).map_err(Into::into)
}

/// Parse a JSON-RPC message from a string.
///
/// # Errors
///
/// Returns an error when the string is not valid JSON or JSON-RPC 2.0.
pub fn parse_json_rpc_str(message: &str) -> Result<JsonRpcInbound> {
    parse_json_rpc_message(serde_json::from_str(message)?)
}

/// Return the next process-global JSON-RPC id.
#[must_use]
pub fn next_id() -> JsonRpcId {
    static NEXT_ID: AtomicI64 = AtomicI64::new(1);
    JsonRpcId::Number(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// Build MCP cancellation notification params.
#[must_use]
pub fn cancellation_params(id: &JsonRpcId, reason: &str) -> Value {
    json!({ "requestId": id, "reason": reason })
}
