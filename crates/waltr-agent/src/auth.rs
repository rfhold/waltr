//! Shared authentication primitives for providers and MCP servers.

use serde::{Deserialize, Serialize};

/// Runtime/config target that can require authentication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthTarget {
    /// Model provider credentials.
    Provider {
        /// Provider key from config.
        key: String,
    },
    /// Streamable HTTP MCP server credentials.
    McpServer {
        /// MCP server key from config.
        key: String,
    },
}

impl AuthTarget {
    /// Create a provider auth target.
    #[must_use]
    pub fn provider(key: impl Into<String>) -> Self {
        Self::Provider { key: key.into() }
    }

    /// Create an MCP server auth target.
    #[must_use]
    pub fn mcp_server(key: impl Into<String>) -> Self {
        Self::McpServer { key: key.into() }
    }

    /// Stable credential-store key for this target.
    #[must_use]
    pub fn credential_key(&self) -> String {
        match self {
            Self::Provider { key } => format!("provider:{key}"),
            Self::McpServer { key } => format!("mcp:{key}"),
        }
    }

    /// User/config key for display and lookup.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Provider { key } | Self::McpServer { key } => key,
        }
    }
}
