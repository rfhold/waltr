mod http;
mod sse;
mod stdio;

use std::fmt;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::{Result, protocol::JsonRpcInbound};
use http::StreamableHttpTransport;
use stdio::StdioTransport;

/// Transport inbound channel sender.
pub type InboundSender = mpsc::UnboundedSender<TransportInbound>;

/// Inbound transport event.
#[derive(Debug)]
pub enum TransportInbound {
    /// Parsed JSON-RPC message.
    Message(JsonRpcInbound),
    /// Streamable HTTP SSE event id.
    SseEventId(String),
    /// Transport disconnection.
    Disconnected(String),
}

/// MCP client transport configuration.
#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    /// Stdio subprocess transport.
    Stdio {
        /// Command to spawn.
        command: String,
        /// Command arguments.
        args: Vec<String>,
    },
    /// Streamable HTTP endpoint.
    StreamableHttp {
        /// Endpoint URL.
        endpoint: String,
    },
}

impl McpTransportConfig {
    /// Return the transport kind name.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::StreamableHttp { .. } => "streamable_http",
        }
    }
}

/// MCP client transport.
pub enum McpTransport {
    /// Stdio subprocess transport.
    Stdio(Box<StdioTransport>),
    /// Streamable HTTP transport.
    StreamableHttp(StreamableHttpTransport),
}

impl fmt::Debug for McpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Stdio(_) => "Stdio",
            Self::StreamableHttp(_) => "StreamableHttp",
        };
        formatter.debug_tuple("McpTransport").field(&kind).finish()
    }
}

impl McpTransport {
    /// Start a transport from config.
    pub async fn start(
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

    /// Send one JSON-RPC message.
    pub async fn send(&self, message: Value, initialized: bool) -> Result<()> {
        match self {
            Self::Stdio(transport) => transport.send(message).await,
            Self::StreamableHttp(transport) => transport.send(message, initialized).await,
        }
    }

    /// Return the transport kind name.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Stdio(_) => "stdio",
            Self::StreamableHttp(_) => "streamable_http",
        }
    }

    /// Shutdown the transport.
    pub async fn shutdown(&self) {
        if let Self::Stdio(transport) = self {
            transport.shutdown().await;
        }
    }
}

pub use sse::{SseJsonRpcEvent, parse_sse_response};
