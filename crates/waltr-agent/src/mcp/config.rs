use serde_json::json;

use super::log::McpLogLevel;

use std::time::Duration;

pub(in crate::mcp) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP server configuration collection.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub(in crate::mcp) servers: Vec<McpServerConfig>,
    pub(in crate::mcp) timeout: Duration,
    pub(in crate::mcp) log_level: Option<McpLogLevel>,
}

impl McpConfig {
    /// Create an empty MCP configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            servers: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            log_level: None,
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

    pub(in crate::mcp) fn into_parts(
        self,
    ) -> (Vec<McpServerConfig>, Duration, Option<McpLogLevel>) {
        (self.servers, self.timeout, self.log_level)
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
        Self {
            name: name.into(),
            transport: McpTransportConfig::StreamableHttp {
                endpoint: endpoint.into(),
            },
        }
    }
}

/// MCP transport configuration.
#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    /// MCP stdio subprocess transport.
    Stdio {
        /// Command to spawn.
        command: String,
        /// Command arguments.
        args: Vec<String>,
    },
    /// MCP Streamable HTTP endpoint.
    StreamableHttp {
        /// MCP endpoint URL.
        endpoint: String,
    },
}

impl McpTransportConfig {
    pub(in crate::mcp) const fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::StreamableHttp { .. } => "streamable_http",
        }
    }
}

pub(in crate::mcp) fn set_log_level_params(level: McpLogLevel) -> serde_json::Value {
    json!({ "level": level.as_str() })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{McpLogLevel, set_log_level_params};

    #[test]
    fn serializes_set_log_level_params() {
        assert_eq!(
            set_log_level_params(McpLogLevel::Warning),
            json!({ "level": "warning" })
        );
    }
}
