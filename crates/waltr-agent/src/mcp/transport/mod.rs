mod http;
mod sse;
mod stdio;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::Result;

use super::{config::McpTransportConfig, protocol::JsonRpcInbound};
use http::StreamableHttpTransport;
use stdio::StdioTransport;

pub(in crate::mcp) type InboundSender = mpsc::UnboundedSender<TransportInbound>;

#[derive(Debug)]
pub(in crate::mcp) enum TransportInbound {
    Message(JsonRpcInbound),
    SseEventId(String),
    Disconnected(String),
}

pub(in crate::mcp) enum McpTransport {
    Stdio(Box<StdioTransport>),
    StreamableHttp(StreamableHttpTransport),
}

impl McpTransport {
    pub(in crate::mcp) async fn start(
        server_name: String,
        config: McpTransportConfig,
        inbound: InboundSender,
    ) -> Result<Self> {
        match config {
            McpTransportConfig::Stdio { command, args } => Ok(Self::Stdio(Box::new(
                StdioTransport::start(server_name, command, args, inbound).await?,
            ))),
            McpTransportConfig::StreamableHttp { endpoint } => Ok(Self::StreamableHttp(
                StreamableHttpTransport::new(endpoint, inbound),
            )),
        }
    }

    pub(in crate::mcp) async fn send(&self, message: Value, initialized: bool) -> Result<()> {
        match self {
            Self::Stdio(transport) => transport.send(message).await,
            Self::StreamableHttp(transport) => transport.send(message, initialized).await,
        }
    }

    pub(in crate::mcp) async fn shutdown(&self) {
        if let Self::Stdio(transport) = self {
            transport.shutdown().await;
        }
    }
}
