mod http;
mod sse;
mod stdio;

use std::fmt;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{McpAuthorizationError, Result, protocol::JsonRpcInbound};
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
    /// Streamable HTTP authorization failure.
    AuthorizationFailed(McpAuthorizationError),
    /// Recoverable or terminal transport-layer failure.
    TransportFailed(McpTransportError),
    /// Transport disconnection.
    Disconnected(String),
}

/// Classification for an HTTP transport failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpTransportErrorKind {
    /// HTTP request could not be completed.
    Network,
    /// HTTP response status rejected the transport operation.
    HttpStatus,
    /// SSE response body failed while streaming.
    SseBody,
    /// SSE framing or JSON-RPC decoding failed.
    SseDecode,
}

/// Failure surfaced by a transport background task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpTransportError {
    /// Stable failure class.
    pub kind: McpTransportErrorKind,
    /// Human-readable detail.
    pub message: String,
    /// Whether the transport will retry automatically.
    pub retrying: bool,
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
        /// Optional bearer token used on every HTTP request.
        bearer_token: Option<String>,
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
// Boxing this public enum's HTTP variant would break the transport API.
#[allow(clippy::large_enum_variant)]
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
    ///
    /// # Errors
    ///
    /// Returns an error when the configured transport cannot start.
    pub async fn start(
        server_name: String,
        config: McpTransportConfig,
        inbound: InboundSender,
    ) -> Result<Self> {
        match config {
            McpTransportConfig::Stdio { command, args } => Ok(Self::Stdio(Box::new(
                StdioTransport::start(server_name, command, args, inbound).await?,
            ))),
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token,
            } => Ok(Self::StreamableHttp(StreamableHttpTransport::new(
                endpoint,
                bearer_token,
                inbound,
            ))),
        }
    }

    /// Send one JSON-RPC message.
    ///
    /// # Errors
    ///
    /// Returns an error when sending fails or the request is canceled.
    pub async fn send(
        &self,
        message: Value,
        initialized: bool,
        cancellation: Option<&CancellationToken>,
    ) -> Result<()> {
        match self {
            Self::Stdio(transport) => tokio::select! {
                () = request_cancelled(cancellation) => Err(crate::Error::protocol("MCP request canceled")),
                result = transport.send(message) => result,
            },
            Self::StreamableHttp(transport) => {
                transport.send(message, initialized, cancellation).await
            }
        }
    }

    /// Promote transport session state after initialize result validation.
    ///
    /// # Errors
    ///
    /// Returns an error when the initialized state cannot be committed.
    pub async fn mark_initialized(&self, protocol_version: &str) -> Result<()> {
        match self {
            Self::Stdio(_) => Ok(()),
            Self::StreamableHttp(transport) => transport.mark_initialized(protocol_version).await,
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
    ///
    /// # Errors
    ///
    /// Returns an error when graceful transport shutdown fails.
    pub async fn shutdown(&self) -> Result<()> {
        match self {
            Self::Stdio(transport) => transport.shutdown().await,
            Self::StreamableHttp(transport) => transport.shutdown().await,
        }
    }
}

async fn request_cancelled(cancellation: Option<&CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending().await,
    }
}

pub use sse::{SseDecoder, SseFrame, SseJsonRpcEvent, parse_sse_response};
