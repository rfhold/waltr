mod http;
mod sse;
mod stdio;

use serde_json::Value;

use crate::Result;

use super::config::McpTransportConfig;
use http::StreamableHttpTransport;
use stdio::StdioTransport;

pub(in crate::mcp) enum McpTransport {
    Stdio(Box<StdioTransport>),
    StreamableHttp(StreamableHttpTransport),
}

impl McpTransport {
    pub(in crate::mcp) fn new(config: McpTransportConfig) -> Self {
        match config {
            McpTransportConfig::Stdio { command, args } => {
                Self::Stdio(Box::new(StdioTransport::new(command, args)))
            }
            McpTransportConfig::StreamableHttp { endpoint } => {
                Self::StreamableHttp(StreamableHttpTransport::new(endpoint))
            }
        }
    }

    pub(in crate::mcp) async fn request(&self, request: Value, id: Option<u64>) -> Result<Value> {
        match self {
            Self::Stdio(transport) => transport.request(request, id).await,
            Self::StreamableHttp(transport) => transport.request(request, id).await,
        }
    }
}
