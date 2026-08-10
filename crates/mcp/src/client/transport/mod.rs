mod http;
mod sse;
mod stdio;

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    McpAuthorizationError, Result,
    protocol::{JsonRpcId, JsonRpcInbound},
};
use http::StreamableHttpTransport;
use stdio::StdioTransport;

use self::sse::{DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_MAX_SSE_LINE_BYTES};

/// Bounds applied while receiving MCP transport messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpTransportLimits {
    /// Maximum bytes accepted in one SSE line.
    pub max_sse_line_bytes: usize,
    /// Maximum bytes accepted in one JSON-RPC message or SSE event.
    pub max_message_bytes: usize,
}

impl McpTransportLimits {
    /// Create explicit positive transport limits.
    ///
    /// # Panics
    ///
    /// Panics when either limit is zero.
    #[must_use]
    pub const fn new(max_sse_line_bytes: usize, max_message_bytes: usize) -> Self {
        assert!(max_sse_line_bytes > 0, "SSE line limit must be positive");
        assert!(max_message_bytes > 0, "MCP message limit must be positive");
        Self {
            max_sse_line_bytes,
            max_message_bytes,
        }
    }
}

impl Default for McpTransportLimits {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SSE_LINE_BYTES, DEFAULT_MAX_MESSAGE_BYTES)
    }
}

/// Transport inbound channel sender.
pub type InboundSender = mpsc::UnboundedSender<TransportInbound>;

/// Inbound transport event.
#[derive(Debug)]
pub enum TransportInbound {
    /// Parsed JSON-RPC message.
    Message(JsonRpcInbound),
    /// Streamable HTTP authorization failure.
    AuthorizationFailed(McpAuthorizationError),
    /// Dynamic bearer provider failure.
    BearerProviderFailed(McpBearerProviderError),
    /// Recoverable or terminal transport-layer failure.
    TransportFailed(McpTransportError),
    /// One request stream ended before its terminal response.
    RequestFailed(JsonRpcId, String),
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

/// Failure returned by a dynamic Streamable HTTP bearer provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpBearerProviderError {
    /// The caller must obtain new authorization before requests can continue.
    Authorization,
    /// A current credential could not be obtained because its source is unavailable.
    Unavailable,
}

impl fmt::Display for McpBearerProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorization => formatter.write_str("MCP bearer authorization is required"),
            Self::Unavailable => formatter.write_str("MCP bearer provider is unavailable"),
        }
    }
}

impl std::error::Error for McpBearerProviderError {}

/// Asynchronous source of current Streamable HTTP bearer tokens.
pub trait McpBearerProvider: Send + Sync + 'static {
    /// Resolve the bearer token for one HTTP request.
    fn bearer_token(
        &self,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<String, McpBearerProviderError>> + Send + '_>,
    >;
}

impl<F, Fut> McpBearerProvider for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = std::result::Result<String, McpBearerProviderError>> + Send + 'static,
{
    fn bearer_token(
        &self,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<String, McpBearerProviderError>> + Send + '_>,
    > {
        Box::pin(self())
    }
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
        Self::start_with_limits(server_name, config, inbound, McpTransportLimits::default()).await
    }

    /// Start a transport from config with explicit receive limits.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured transport cannot start.
    pub async fn start_with_limits(
        server_name: String,
        config: McpTransportConfig,
        inbound: InboundSender,
        limits: McpTransportLimits,
    ) -> Result<Self> {
        Self::start_with_optional_bearer_provider(server_name, config, inbound, limits, None).await
    }

    /// Start a transport with a dynamic Streamable HTTP bearer provider.
    ///
    /// The provider supersedes the static token in Streamable HTTP configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured transport cannot start.
    pub async fn start_with_bearer_provider(
        server_name: String,
        config: McpTransportConfig,
        inbound: InboundSender,
        bearer_provider: Arc<dyn McpBearerProvider>,
    ) -> Result<Self> {
        Self::start_with_bearer_provider_and_limits(
            server_name,
            config,
            inbound,
            bearer_provider,
            McpTransportLimits::default(),
        )
        .await
    }

    /// Start a transport with a dynamic bearer provider and explicit receive limits.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured transport cannot start.
    pub async fn start_with_bearer_provider_and_limits(
        server_name: String,
        config: McpTransportConfig,
        inbound: InboundSender,
        bearer_provider: Arc<dyn McpBearerProvider>,
        limits: McpTransportLimits,
    ) -> Result<Self> {
        Self::start_with_optional_bearer_provider(
            server_name,
            config,
            inbound,
            limits,
            Some(bearer_provider),
        )
        .await
    }

    async fn start_with_optional_bearer_provider(
        server_name: String,
        config: McpTransportConfig,
        inbound: InboundSender,
        limits: McpTransportLimits,
        bearer_provider: Option<Arc<dyn McpBearerProvider>>,
    ) -> Result<Self> {
        match config {
            McpTransportConfig::Stdio { command, args } => Ok(Self::Stdio(Box::new(
                StdioTransport::start(
                    server_name,
                    command,
                    args,
                    inbound,
                    limits.max_message_bytes,
                )
                .await?,
            ))),
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token,
            } => Ok(Self::StreamableHttp(StreamableHttpTransport::new(
                endpoint,
                bearer_token,
                bearer_provider,
                inbound,
                limits,
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
        cancellation: Option<&CancellationToken>,
        http_headers: &[(String, String)],
    ) -> Result<()> {
        match self {
            Self::Stdio(transport) => tokio::select! {
                () = request_cancelled(cancellation) => Err(crate::Error::protocol("MCP request canceled")),
                result = transport.send(message) => result,
            },
            Self::StreamableHttp(transport) => {
                transport.send(message, cancellation, http_headers).await
            }
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
