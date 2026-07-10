pub use mcp::client::McpTransportConfig;
use mcp::{McpLogLevel, McpTracePropagation};

use std::time::Duration;

pub(in crate::mcp) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP server configuration collection.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub(in crate::mcp) servers: Vec<McpServerConfig>,
    pub(in crate::mcp) timeout: Duration,
    pub(in crate::mcp) log_level: Option<McpLogLevel>,
    pub(in crate::mcp) trace_propagation: McpTracePropagation,
}

impl McpConfig {
    /// Create an empty MCP configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            servers: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            log_level: None,
            trace_propagation: McpTracePropagation::Disabled,
        }
    }

    /// Add one MCP server.
    #[must_use]
    pub fn with_server(mut self, server: McpServerConfig) -> Self {
        self.servers.push(server);
        self
    }

    /// Configure per-request timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Configure the minimum MCP protocol log level requested from servers that support logging.
    #[must_use]
    pub const fn with_log_level(mut self, level: McpLogLevel) -> Self {
        self.log_level = Some(level);
        self
    }

    /// Configure MCP request trace propagation.
    #[must_use]
    pub const fn with_trace_propagation(mut self, trace_propagation: McpTracePropagation) -> Self {
        self.trace_propagation = trace_propagation;
        self
    }

    pub(in crate::mcp) fn into_parts(
        self,
    ) -> (
        Vec<McpServerConfig>,
        Duration,
        Option<McpLogLevel>,
        McpTracePropagation,
    ) {
        (
            self.servers,
            self.timeout,
            self.log_level,
            self.trace_propagation,
        )
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One configured MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub(in crate::mcp) name: String,
    pub(in crate::mcp) transport: McpTransportConfig,
}

impl McpServerConfig {
    /// Create a stdio MCP server configuration.
    #[must_use]
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            transport: McpTransportConfig::Stdio {
                command: command.into(),
                args: args.into_iter().map(Into::into).collect(),
            },
        }
    }

    /// Create a Streamable HTTP MCP server configuration.
    #[must_use]
    pub fn streamable_http(name: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self::streamable_http_with_bearer_token(name, endpoint, None)
    }

    /// Create a Streamable HTTP MCP server configuration with an optional bearer token.
    #[must_use]
    pub fn streamable_http_with_bearer_token(
        name: impl Into<String>,
        endpoint: impl Into<String>,
        bearer_token: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            transport: McpTransportConfig::StreamableHttp {
                endpoint: endpoint.into(),
                bearer_token,
            },
        }
    }
}
