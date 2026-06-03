use std::{sync::Arc, time::Duration};

use crate::{AgentTool, Result};

use super::runtime::McpRuntime;

pub(in crate::mcp) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP server configuration collection.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub(in crate::mcp) servers: Vec<McpServerConfig>,
    pub(in crate::mcp) timeout: Duration,
}

impl McpConfig {
    /// Create an empty MCP configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            servers: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
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

    /// Convert this MCP configuration into host tools.
    ///
    /// # Errors
    ///
    /// Returns an error if any configured MCP server cannot be initialized or listed.
    pub async fn into_agent_tools(self) -> Result<Vec<AgentTool>> {
        let runtime = Arc::new(McpRuntime::new(self.servers, self.timeout));
        runtime.direct_agent_tools().await
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
