//! Server-owned default configuration.

use std::collections::HashMap;

use waltr::grpc::waltr::config::v1::{
    Configuration, Model, ModelCapabilities, ModelType, OAuthProvider, OauthJwtMetadata,
    OauthTokenField, OauthTokenRequestStyle, OauthUrlParam, Provider, ProviderType,
};

/// Return the built-in configuration used when no config file exists.
#[must_use]
pub fn default_configuration() -> Configuration {
    Configuration {
        providers: HashMap::from([
            (
                "anthropic".to_owned(),
                provider(
                    "anthropic",
                    ProviderType::AnthropicMessages,
                    "https://api.anthropic.com/v1/messages",
                    true,
                    "claude-haiku-4-5",
                    HashMap::new(),
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
                    "gpt-5-nano",
                    HashMap::new(),
                    None,
                ),
            ),
            (
                "openai-codex".to_owned(),
                provider(
                    "openai-codex",
                    ProviderType::OpenaiResponses,
                    "https://chatgpt.com/backend-api/codex/responses",
                    true,
                    "gpt-5-codex",
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

fn provider(
    name: &str,
    provider_type: ProviderType,
    url: &str,
    oauth_compatible: bool,
    model_key: &str,
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
        models: HashMap::from([(
            model_key.to_owned(),
            Model {
                name: model_key.to_owned(),
                r#type: ModelType::Llm as i32,
                capabilities: Some(ModelCapabilities {
                    tools: true,
                    image: false,
                }),
                ..Model::default()
            },
        )]),
        ..Provider::default()
    }
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
