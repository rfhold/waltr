pub use mcp::client::{McpBearerProvider, McpTransportConfig};
use mcp::{McpLogLevel, McpTracePropagation};

use std::time::Duration;
use std::{future::Future, pin::Pin, sync::Arc};

pub(crate) type AppAuthorityValidator =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Atomic credential snapshot for Apps-capable HTTP connections.
/// The first tuple value is a non-secret authorization incarnation, not a token hash.
/// Hosts must replace it on reauthorization/account replacement and retain it on refresh.
pub trait McpAppCredentialProvider: Send + Sync + 'static {
    /// Resolve identity and bearer token from the same credential snapshot.
    fn credential(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, String), mcp::client::McpBearerProviderError>>
                + Send
                + '_,
        >,
    >;
}

#[cfg(test)]
mod apps_tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn mcp_apps_authority_is_nonsecret_refresh_stable_and_pinned_before_dispatch() {
        let current = Arc::new(Mutex::new((
            "authority-a".to_owned(),
            "secret-a".to_owned(),
        )));
        let snapshot = current.clone();
        let provider = Arc::new(move || {
            let value = snapshot.lock().unwrap().clone();
            async move { Ok(value) }
        });
        let config = McpServerConfig::streamable_http_with_app_credentials(
            "server",
            "https://example.test/mcp",
            "authority-a".into(),
            provider.clone(),
        );
        let source = config.app_source().unwrap();
        assert!(!source.contains("secret"));
        let bearer = config.bearer_provider.as_ref().unwrap();
        assert_eq!(bearer.bearer_token().await.unwrap(), "secret-a");
        current.lock().unwrap().1 = "refreshed-secret-a".into();
        assert_eq!(bearer.bearer_token().await.unwrap(), "refreshed-secret-a");
        assert_eq!(config.app_source().unwrap(), source);
        *current.lock().unwrap() = ("authority-b".into(), "secret-b".into());
        assert_eq!(
            bearer.bearer_token().await.unwrap_err(),
            mcp::client::McpBearerProviderError::Authorization
        );
        assert!(
            config
                .app_authority
                .as_ref()
                .unwrap()
                .token()
                .await
                .is_err()
        );
        let replacement = McpServerConfig::streamable_http_with_app_credentials(
            "server",
            "https://example.test/mcp",
            "authority-b".into(),
            provider,
        );
        assert_ne!(replacement.app_source(), Some(source));
        assert_ne!(
            replacement.app_source(),
            McpServerConfig::streamable_http("server", "https://example.test/mcp").app_source()
        );
    }

    #[test]
    fn mcp_apps_unidentified_credentials_are_ineligible_not_anonymous() {
        let static_token = McpServerConfig::streamable_http_with_bearer_token(
            "server",
            "https://example.test/mcp",
            Some("secret".into()),
        );
        assert!(static_token.app_source().is_none());
        let provider = Arc::new(|| async { Ok("secret".to_owned()) });
        let custom = McpServerConfig::streamable_http_with_bearer_provider(
            "server",
            "https://example.test/mcp",
            provider,
        );
        assert!(custom.app_source().is_none());
        assert!(
            McpServerConfig::streamable_http("server", "https://example.test/mcp")
                .app_source()
                .is_some()
        );
        let empty = McpServerConfig::streamable_http_with_app_credentials(
            "server",
            "https://example.test/mcp",
            String::new(),
            Arc::new(|| async { Ok((String::new(), "secret".to_owned())) }),
        );
        assert!(empty.app_source().is_none());
    }
}

impl<F, Fut> McpAppCredentialProvider for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(String, String), mcp::client::McpBearerProviderError>>
        + Send
        + 'static,
{
    fn credential(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, String), mcp::client::McpBearerProviderError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(self())
    }
}

#[derive(Clone)]
pub(in crate::mcp) struct AppAuthority {
    pub id: String,
    pub provider: Arc<dyn McpAppCredentialProvider>,
}

impl AppAuthority {
    pub async fn token(&self) -> Result<String, mcp::client::McpBearerProviderError> {
        let (id, token) = self.provider.credential().await?;
        if id.is_empty() || id != self.id {
            return Err(mcp::client::McpBearerProviderError::Authorization);
        }
        Ok(token)
    }
}

pub(in crate::mcp) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP server configuration collection.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub(in crate::mcp) servers: Vec<McpServerConfig>,
    pub(in crate::mcp) timeout: Duration,
    pub(in crate::mcp) log_level: Option<McpLogLevel>,
    pub(in crate::mcp) trace_propagation: McpTracePropagation,
    pub(in crate::mcp) skills: bool,
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
            skills: false,
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

    /// Opt in to metadata discovery and verified on-demand MCP Skills.
    #[must_use]
    pub const fn with_skills(mut self) -> Self {
        self.skills = true;
        self
    }

    pub(crate) fn app_sources(&self) -> std::collections::BTreeMap<String, Option<String>> {
        self.servers
            .iter()
            .filter(|server| server.app_authority.is_some())
            .map(|server| (server.name.clone(), server.app_source()))
            .collect()
    }

    pub(in crate::mcp) fn into_parts(
        self,
    ) -> (
        Vec<McpServerConfig>,
        Duration,
        Option<McpLogLevel>,
        McpTracePropagation,
        bool,
    ) {
        (
            self.servers,
            self.timeout,
            self.log_level,
            self.trace_propagation,
            self.skills,
        )
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One configured MCP server.
#[derive(Clone)]
pub struct McpServerConfig {
    pub(in crate::mcp) name: String,
    pub(in crate::mcp) transport: McpTransportConfig,
    pub(in crate::mcp) bearer_provider: Option<std::sync::Arc<dyn McpBearerProvider>>,
    pub(in crate::mcp) app_authority: Option<AppAuthority>,
    pub(in crate::mcp) app_authority_validator: Option<AppAuthorityValidator>,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("bearer_provider", &self.bearer_provider.is_some())
            .finish()
    }
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
            bearer_provider: None,
            app_authority: None,
            app_authority_validator: None,
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
            bearer_provider: None,
            app_authority: None,
            app_authority_validator: None,
        }
    }

    /// Create a Streamable HTTP server using a token provider on every request.
    #[must_use]
    pub fn streamable_http_with_bearer_provider(
        name: impl Into<String>,
        endpoint: impl Into<String>,
        bearer_provider: std::sync::Arc<dyn McpBearerProvider>,
    ) -> Self {
        Self {
            name: name.into(),
            transport: McpTransportConfig::StreamableHttp {
                endpoint: endpoint.into(),
                bearer_token: None,
            },
            bearer_provider: Some(bearer_provider),
            app_authority: None,
            app_authority_validator: None,
        }
    }

    /// Create an Apps-capable credentialed connection pinned to one authorization identity.
    /// Every HTTP request fails closed if the provider returns another identity.
    #[must_use]
    pub fn streamable_http_with_app_credentials(
        name: impl Into<String>,
        endpoint: impl Into<String>,
        authority_id: String,
        provider: Arc<dyn McpAppCredentialProvider>,
    ) -> Self {
        let authority = AppAuthority {
            id: authority_id,
            provider,
        };
        let pinned = authority.clone();
        let bearer = Arc::new(move || {
            let pinned = pinned.clone();
            async move { pinned.token().await }
        });
        let mut config = Self::streamable_http_with_bearer_provider(name, endpoint, bearer);
        config.app_authority = Some(authority);
        config
    }

    pub(crate) fn with_app_authority_validator(mut self, validator: AppAuthorityValidator) -> Self {
        self.app_authority_validator = Some(validator);
        self
    }

    pub(in crate::mcp) fn app_source(&self) -> Option<String> {
        use sha2::{Digest as _, Sha256};
        let identity = match &self.transport {
            McpTransportConfig::Stdio { command, args } => format!("stdio:{command:?}:{args:?}"),
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token,
            } => {
                if let Some(authority) = &self.app_authority {
                    if authority.id.is_empty() {
                        return None;
                    }
                    format!("http:{endpoint:?}:authorized:{}", authority.id)
                } else if bearer_token.is_some() || self.bearer_provider.is_some() {
                    return None;
                } else {
                    format!("http:{endpoint:?}:anonymous")
                }
            }
        };
        Some(
            Sha256::digest(identity)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }
}
