use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::Result;

use super::super::protocol::{JsonRpcMessage, json_rpc_result, protocol_error};

pub(in crate::mcp) async fn parse_sse_response(
    response: reqwest::Response,
    id: Option<u64>,
) -> Result<Value> {
    let Some(id) = id else {
        return Ok(json!(null));
    };
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(index) = buffer.find("\n\n") {
            let event = buffer[..index].to_owned();
            buffer = buffer[index + 2..].to_owned();
            let data = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let message: JsonRpcMessage = serde_json::from_str(&data)?;
            if message.id == Some(id) {
                return json_rpc_result(message);
            }
        }
    }
    Err(protocol_error(format!(
        "MCP SSE response ended before JSON-RPC id {id}"
    )))
}
