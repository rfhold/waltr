//! Shared authentication primitives for providers and MCP servers.

use serde::{Deserialize, Serialize};

/// Sanitized authorization context returned by an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorizationContext {
    /// Configured protected MCP endpoint.
    pub endpoint: String,
    /// HTTP status returned by the protected endpoint.
    pub http_status: u16,
    /// Protected resource metadata URL from the bearer challenge.
    pub resource_metadata_url: Option<String>,
    /// Required scope string from the bearer challenge.
    pub scope: Option<String>,
    /// Bearer error code.
    pub error: Option<String>,
    /// Sanitized bearer error description.
    pub error_description: Option<String>,
    /// Bearer error documentation URL.
    pub error_uri: Option<String>,
    /// Authentication realm.
    pub realm: Option<String>,
}

/// Machine-readable outbound authentication failure reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationFailureReason {
    /// A usable credential is absent or expired.
    CredentialRequired,
    /// The upstream rejected a credential.
    CredentialRejected,
    /// An authorization-code or refresh exchange was rejected.
    TokenExchangeFailed,
    /// The authentication service could not be reached.
    NetworkFailed,
    /// The authentication service returned an invalid response.
    ResponseInvalid,
    /// Required OAuth registration or endpoint configuration is absent.
    ConfigurationRequired,
}

/// Durable, target-scoped outbound authentication failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticationFailure {
    /// Configured provider or MCP server that failed authentication.
    pub target: AuthTarget,
    /// Machine-readable reason for the failure.
    pub reason: AuthenticationFailureReason,
    /// Sanitized MCP challenge context, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_authorization: Option<Box<McpAuthorizationContext>>,
}

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
