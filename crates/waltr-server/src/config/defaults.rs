//! Server-owned default configuration.

use std::{collections::HashMap, net::SocketAddr};

use crate::{DEFAULT_LISTEN_ADDR, catalog::CatalogLoader};
use waltr::grpc::waltr::config::v1::{
    Configuration, OAuthProvider, OauthJwtMetadata, OauthTokenField, OauthTokenRequestStyle,
    OauthUrlParam, Provider, ProviderType,
};

use super::ConfigError;

/// Catalog-backed builder for fresh server configuration.
#[derive(Clone, Debug)]
pub struct DefaultConfigBuilder {
    catalog_loader: CatalogLoader,
    mcp_url: Result<String, String>,
}

impl DefaultConfigBuilder {
    /// Build defaults from a shared catalog loader and deployment environment.
    #[must_use]
    pub fn from_env(catalog_loader: CatalogLoader) -> Self {
        Self {
            catalog_loader,
            mcp_url: default_mcp_url(),
        }
    }

    /// Build defaults from a shared catalog loader and explicit self-MCP URL.
    #[must_use]
    pub fn new(catalog_loader: CatalogLoader, mcp_url: impl Into<String>) -> Self {
        Self {
            catalog_loader,
            mcp_url: Ok(mcp_url.into()),
        }
    }

    /// Construct a complete fresh configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the self-MCP URL or provider catalog is invalid or unavailable.
    pub async fn build(&self) -> Result<Configuration, ConfigError> {
        let mcp_url = self
            .mcp_url
            .as_deref()
            .map_err(|message| ConfigError::invalid(message.to_owned()))?;
        self.catalog_loader
            .build_default_configuration(mcp_url)
            .await
    }

    pub(crate) fn catalog_loader(&self) -> CatalogLoader {
        self.catalog_loader.clone()
    }
}

pub(crate) fn provider_templates() -> Configuration {
    Configuration {
        providers: HashMap::from([
            (
                "anthropic".to_owned(),
                provider(
                    "anthropic",
                    ProviderType::AnthropicMessages,
                    "https://api.anthropic.com/v1/messages",
                    false,
                    HashMap::from([(
                        "x-api-key".to_owned(),
                        "${env:ANTHROPIC_API_KEY}".to_owned(),
                    )]),
                    None,
                ),
            ),
            (
                "claude-code".to_owned(),
                provider(
                    "claude-code",
                    ProviderType::AnthropicMessages,
                    "https://api.anthropic.com/v1/messages?beta=true",
                    true,
                    HashMap::from([
                        (
                            "anthropic-beta".to_owned(),
                            "claude-code-20250219,oauth-2025-04-20".to_owned(),
                        ),
                        ("user-agent".to_owned(), "claude-cli/2.1.75".to_owned()),
                        ("x-app".to_owned(), "cli".to_owned()),
                    ]),
                    Some(claude_code_oauth_settings()),
                ),
            ),
            (
                "openai".to_owned(),
                provider(
                    "openai",
                    ProviderType::OpenaiChatCompletions,
                    "https://api.openai.com/v1/chat/completions",
                    false,
                    HashMap::from([(
                        "Authorization".to_owned(),
                        concat!("Bearer ", "${env:OPENAI_API_KEY}").to_owned(),
                    )]),
                    None,
                ),
            ),
            (
                "codex".to_owned(),
                provider(
                    "codex",
                    ProviderType::OpenaiResponses,
                    "https://chatgpt.com/backend-api/codex/responses",
                    true,
                    HashMap::from([
                        ("accept".to_owned(), "text/event-stream".to_owned()),
                        (
                            "OpenAI-Beta".to_owned(),
                            "responses=experimental".to_owned(),
                        ),
                        ("originator".to_owned(), "waltr".to_owned()),
                    ]),
                    Some(openai_codex_oauth_settings()),
                ),
            ),
        ]),
        ..Configuration::default()
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the helper mirrors the provider fixture fields"
)]
fn provider(
    name: &str,
    provider_type: ProviderType,
    url: &str,
    oauth_compatible: bool,
    headers: HashMap<String, String>,
    oauth_settings: Option<OAuthProvider>,
) -> Provider {
    Provider {
        r#type: provider_type as i32,
        name: name.to_owned(),
        url: url.to_owned(),
        oauth_compatible,
        headers,
        oauth_settings,
        models: HashMap::new(),
    }
}

fn default_mcp_url() -> Result<String, String> {
    mcp_url_from_values(
        std::env::var("WALTR_PUBLIC_URL").ok().as_deref(),
        std::env::var("WALTR_SERVER_ADDR").ok().as_deref(),
    )
}

fn mcp_url_from_values(
    public_url: Option<&str>,
    server_addr: Option<&str>,
) -> Result<String, String> {
    if let Some(public_url) = public_url {
        return append_mcp_path(public_url);
    }

    let address = server_addr
        .unwrap_or(DEFAULT_LISTEN_ADDR)
        .parse::<SocketAddr>()
        .or_else(|_| DEFAULT_LISTEN_ADDR.parse::<SocketAddr>())
        .expect("DEFAULT_LISTEN_ADDR must be a socket address");
    Ok(format!("http://127.0.0.1:{}/mcp", address.port()))
}

fn append_mcp_path(value: &str) -> Result<String, String> {
    let mut url = url::Url::parse(value)
        .map_err(|_| "WALTR_PUBLIC_URL must be an absolute HTTP(S) URL".to_owned())?;
    let loopback_http = url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        };
    if !(url.scheme() == "https" || loopback_http)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "WALTR_PUBLIC_URL must use HTTPS (except loopback HTTP) without userinfo, query, or fragment"
                .to_owned(),
        );
    }
    let path = format!("{}/mcp", url.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

pub(crate) fn claude_code_oauth_settings() -> OAuthProvider {
    OAuthProvider {
        name: "Claude Code".to_owned(),
        authorize_url: "https://claude.ai/oauth/authorize".to_owned(),
        token_url: "https://platform.claude.com/v1/oauth/token".to_owned(),
        client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_owned(),
        redirect_uri: "http://localhost:53692/callback".to_owned(),
        scopes: vec![
            "org:create_api_key".to_owned(),
            "user:profile".to_owned(),
            "user:inference".to_owned(),
            "user:sessions:claude_code".to_owned(),
            "user:mcp_servers".to_owned(),
            "user:file_upload".to_owned(),
        ],
        token_request_style: OauthTokenRequestStyle::Json as i32,
        auth_url_params: vec![OauthUrlParam {
            key: "code".to_owned(),
            value: "true".to_owned(),
        }],
        jwt_metadata: Vec::new(),
        send_state_in_token_request: true,
    }
}

pub(crate) fn openai_codex_oauth_settings() -> OAuthProvider {
    OAuthProvider {
        name: "OpenAI Codex".to_owned(),
        authorize_url: "https://auth.openai.com/oauth/authorize".to_owned(),
        token_url: "https://auth.openai.com/oauth/token".to_owned(),
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_owned(),
        redirect_uri: "http://localhost:1455/auth/callback".to_owned(),
        scopes: vec![
            "openid".to_owned(),
            "profile".to_owned(),
            "email".to_owned(),
            "offline_access".to_owned(),
            "api.connectors.read".to_owned(),
            "api.connectors.invoke".to_owned(),
        ],
        token_request_style: OauthTokenRequestStyle::FormUrlencoded as i32,
        auth_url_params: vec![
            OauthUrlParam {
                key: "id_token_add_organizations".to_owned(),
                value: "true".to_owned(),
            },
            OauthUrlParam {
                key: "codex_cli_simplified_flow".to_owned(),
                value: "true".to_owned(),
            },
            OauthUrlParam {
                key: "originator".to_owned(),
                value: "waltr".to_owned(),
            },
        ],
        jwt_metadata: vec![OauthJwtMetadata {
            token: OauthTokenField::Id as i32,
            metadata_key: "account_id".to_owned(),
            claim_names: vec![
                "https://api.openai.com/auth.chatgpt_account_id".to_owned(),
                "chatgpt_account_id".to_owned(),
            ],
            required: true,
        }],
        send_state_in_token_request: false,
    }
}

#[cfg(test)]
mod tests {
    use waltr::grpc::waltr::config::v1::ProviderType;

    use super::{append_mcp_path, mcp_url_from_values, provider_templates};

    #[test]
    fn defaults_split_api_key_and_oauth_providers() {
        let config = provider_templates();

        let anthropic = &config.providers["anthropic"];
        assert_eq!(anthropic.r#type, ProviderType::AnthropicMessages as i32);
        assert!(!anthropic.oauth_compatible);
        assert!(anthropic.oauth_settings.is_none());
        assert_eq!(anthropic.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(anthropic.headers["x-api-key"], "${env:ANTHROPIC_API_KEY}");

        let claude_code = &config.providers["claude-code"];
        assert_eq!(claude_code.r#type, ProviderType::AnthropicMessages as i32);
        assert!(claude_code.oauth_compatible);
        assert!(claude_code.oauth_settings.is_some());
        assert_eq!(
            claude_code.url,
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(
            claude_code.headers["anthropic-beta"],
            "claude-code-20250219,oauth-2025-04-20"
        );
        assert_eq!(claude_code.headers["user-agent"], "claude-cli/2.1.75");
        assert_eq!(claude_code.headers["x-app"], "cli");

        let openai = &config.providers["openai"];
        assert!(!openai.oauth_compatible);
        assert!(openai.oauth_settings.is_none());
        assert_eq!(
            openai.headers["Authorization"],
            concat!("Bearer ", "${env:OPENAI_API_KEY}")
        );

        let codex = &config.providers["codex"];
        assert_eq!(codex.r#type, ProviderType::OpenaiResponses as i32);
        assert!(codex.oauth_compatible);
        assert!(codex.oauth_settings.is_some());
        assert!(!config.providers.contains_key("openai-codex"));
    }

    #[test]
    fn public_url_appends_mcp_without_discarding_a_path_prefix() {
        assert_eq!(
            append_mcp_path("https://waltr.example/prefix/").as_deref(),
            Ok("https://waltr.example/prefix/mcp")
        );
        assert!(append_mcp_path("http://127.0.0.1:14333").is_ok());
        for invalid in [
            "not a url",
            "http://waltr.example",
            "https://user:password@waltr.example",
            "https://waltr.example?query=forbidden",
            "https://waltr.example#fragment-forbidden",
        ] {
            assert!(append_mcp_path(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn mcp_url_uses_public_url_or_loopback_listen_port() {
        assert_eq!(
            mcp_url_from_values(Some("https://waltr.example/base/"), Some("0.0.0.0:9999"))
                .as_deref(),
            Ok("https://waltr.example/base/mcp")
        );
        assert!(mcp_url_from_values(Some("invalid"), Some("[::]:4567")).is_err());
        assert_eq!(
            mcp_url_from_values(None, None).as_deref(),
            Ok("http://127.0.0.1:14333/mcp")
        );
    }
}
