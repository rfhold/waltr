use futures_util::StreamExt;

use crate::{
    Result,
    protocol::{JsonRpcInbound, parse_json_rpc_str},
};

/// One SSE JSON-RPC event.
#[derive(Debug)]
pub struct SseJsonRpcEvent {
    /// Optional SSE event id.
    pub id: Option<String>,
    /// Parsed JSON-RPC message.
    pub message: JsonRpcInbound,
}

/// Parse a finite streamable HTTP SSE response.
pub async fn parse_sse_response(response: reqwest::Response) -> Result<Vec<SseJsonRpcEvent>> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut messages = Vec::new();
    loop {
        let chunk = stream.next().await;
        let Some(chunk) = chunk else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(index) = buffer.find("\n\n") {
            let event = buffer[..index].to_owned();
            buffer = buffer[index + 2..].to_owned();
            let event_id = event
                .lines()
                .find_map(|line| line.strip_prefix("id:"))
                .map(str::trim_start)
                .map(ToOwned::to_owned);
            let data = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            messages.push(SseJsonRpcEvent {
                id: event_id,
                message: parse_json_rpc_str(&data)?,
            });
        }
    }
    Ok(messages)
}
