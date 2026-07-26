use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ModelApi, ModelCost};

/// First-class runtime settings stored on a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRuntimeSettings {
    /// Provider settings snapshot.
    pub provider: SessionProviderRuntimeSettings,
    /// Enabled MCP server snapshots.
    #[serde(default)]
    pub mcp_servers: Vec<SessionMcpServerRuntimeSettings>,
    /// Whether MCP requests propagate the current trace context.
    #[serde(default)]
    pub mcp_trace_propagation: bool,
}

/// Provider settings needed to build a runtime without consulting server config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionProviderRuntimeSettings {
    /// User/config provider key.
    pub key: String,
    /// Provider API surface.
    pub api: ModelApi,
    /// Endpoint URL override.
    pub url: String,
    /// Model id.
    pub model_id: String,
    /// Model cost snapshot.
    pub model_cost: Option<ModelCost>,
    /// Provider header templates, excluding empty values after runtime resolution.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Optional OAuth refresh definition for this provider.
    pub oauth: Option<SessionOAuthProviderSettings>,
    /// Optional OpenAI reasoning effort.
    pub reasoning_effort: Option<String>,
}

/// MCP server settings snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMcpServerRuntimeSettings {
    /// MCP server key.
    pub key: String,
    /// Server kind.
    pub kind: SessionMcpServerRuntimeKind,
    /// Stdio command.
    pub command: String,
    /// Streamable HTTP/SSE URL.
    pub url: String,
}

/// MCP server kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMcpServerRuntimeKind {
    /// Stdio transport.
    Stdio,
    /// HTTP transport.
    StreamableHttp,
}

/// OAuth provider settings needed for token refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOAuthProviderSettings {
    /// Token URL.
    pub token_url: String,
    /// OAuth client id.
    pub client_id: String,
    /// Redirect URI.
    pub redirect_uri: String,
    /// Token request style.
    pub token_request_style: SessionOAuthTokenRequestStyle,
    /// Provider-specific token field mappings.
    #[serde(default)]
    pub token_fields: Vec<SessionOAuthTokenField>,
}

/// OAuth token request body format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionOAuthTokenRequestStyle {
    /// JSON body.
    Json,
    /// Form-url-encoded body.
    FormUrlencoded,
}

/// OAuth token response field mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOAuthTokenField {
    /// Source response field.
    pub source: String,
    /// Destination credential field.
    pub target: SessionOAuthTokenFieldTarget,
}

/// OAuth token response destination field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionOAuthTokenFieldTarget {
    /// Access token.
    Access,
    /// Refresh token.
    Refresh,
    /// Expiration timestamp in milliseconds.
    ExpiresAtMs,
    /// Expiration duration in seconds.
    ExpiresInSeconds,
    /// Provider-specific metadata.
    Metadata,
}

mod credentials;

pub use credentials::{
    FileCredentialStore, McpOAuthCredentialStore, McpOAuthEndpoints, McpOAuthRegistration,
    McpOAuthRegistrationMethod, McpOAuthState, McpOAuthStoreFuture, McpOAuthToken, McpRefreshClaim,
    McpRefreshClaimResult, OAuthCredential, StoredCredential,
};

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn runtime_settings_default_mcp_trace_propagation_is_false() {
        let settings: SessionRuntimeSettings = serde_json::from_value(json!({
            "provider": {
                "key": "test",
                "api": "OpenAiResponses",
                "url": "",
                "model_id": "test-model",
                "model_cost": null,
                "headers": {},
                "oauth": null,
                "reasoning_effort": null
            },
            "mcp_servers": []
        }))
        .expect("settings deserialize");

        assert!(!settings.mcp_trace_propagation);
    }
}
