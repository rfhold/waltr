use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ModelApi, ModelCost};

/// Durable inference settings stored on a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceSettings {
    /// Provider settings used for inference.
    pub provider: InferenceProviderSettings,
}

impl InferenceSettings {
    /// Disable reasoning using the representation supported by the selected provider API.
    #[must_use]
    pub fn with_reasoning_disabled(mut self) -> Self {
        self.provider.reasoning_effort = match self.provider.api {
            ModelApi::OpenAiResponses | ModelApi::OpenAiResponsesWebSocket => {
                Some("none".to_owned())
            }
            ModelApi::AnthropicMessages | ModelApi::OpenAiChatCompletions => None,
        };
        self
    }
}

/// Provider settings needed to perform inference without consulting server config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceProviderSettings {
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
    /// Optional `OpenAI` reasoning effort.
    pub reasoning_effort: Option<String>,
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
    /// JWT claims copied into provider metadata after token exchange.
    #[serde(default)]
    pub jwt_metadata: Vec<SessionOAuthJwtMetadata>,
}

/// JWT token containing provider metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionOAuthJwtToken {
    /// OAuth access token.
    Access,
    /// `OpenID` Connect ID token.
    Id,
}

/// Provider metadata extracted from an unverified token response JWT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOAuthJwtMetadata {
    /// Token response field containing the JWT.
    pub token: SessionOAuthJwtToken,
    /// Credential metadata key populated by this extractor.
    pub metadata_key: String,
    /// Claim names checked in order.
    #[serde(default)]
    pub claim_names: Vec<String>,
    /// Whether metadata must exist after preserving and extracting values.
    #[serde(default)]
    pub required: bool,
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
    CredentialStore, FileCredentialStore, McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration,
    McpOAuthRegistrationMethod, OAUTH_CREDENTIAL_VERSION, OAuthCredential, OAuthCredentialContext,
    OAuthCredentialRecord, OAuthCredentialStore, OAuthCredentialStoreFuture, OAuthRefreshClaim,
    OAuthRefreshClaimResult, OAuthTokenSet, StoredCredential,
};

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn inference_settings_contain_only_provider_settings() {
        let settings: InferenceSettings = serde_json::from_value(json!({
            "provider": {
                "key": "test",
                "api": "OpenAiResponses",
                "url": "",
                "model_id": "test-model",
                "model_cost": null,
                "headers": {},
                "oauth": null,
                "reasoning_effort": null
            }
        }))
        .expect("settings deserialize");

        assert_eq!(settings.provider.key, "test");
        assert_eq!(
            serde_json::to_value(settings).expect("settings serialize"),
            json!({
                "provider": {
                    "key": "test",
                    "api": "OpenAiResponses",
                    "url": "",
                    "model_id": "test-model",
                    "model_cost": null,
                    "headers": {},
                    "oauth": null,
                    "reasoning_effort": null
                }
            })
        );
    }

    #[test]
    fn provider_oauth_settings_default_missing_jwt_metadata() {
        let settings: SessionOAuthProviderSettings = serde_json::from_value(json!({
            "token_url": "https://issuer.example/token",
            "client_id": "client",
            "redirect_uri": "http://localhost/callback",
            "token_request_style": "FormUrlencoded",
            "token_fields": []
        }))
        .expect("legacy provider OAuth settings");

        assert!(settings.jwt_metadata.is_empty());
    }

    #[test]
    fn disabled_reasoning_is_provider_neutral_and_preserves_settings() {
        for (api, expected) in [
            (ModelApi::OpenAiResponses, Some("none")),
            (ModelApi::OpenAiResponsesWebSocket, Some("none")),
            (ModelApi::AnthropicMessages, None),
            (ModelApi::OpenAiChatCompletions, None),
        ] {
            let settings = InferenceSettings {
                provider: InferenceProviderSettings {
                    key: "provider-key".to_owned(),
                    api,
                    url: "https://provider.example/api".to_owned(),
                    model_id: "model".to_owned(),
                    model_cost: None,
                    headers: BTreeMap::from([("x-test".to_owned(), "value".to_owned())]),
                    oauth: None,
                    reasoning_effort: Some("high".to_owned()),
                },
            };

            let disabled = settings.clone().with_reasoning_disabled();

            assert_eq!(disabled.provider.reasoning_effort.as_deref(), expected);
            assert_eq!(disabled.provider.key, settings.provider.key);
            assert_eq!(disabled.provider.url, settings.provider.url);
            assert_eq!(disabled.provider.model_id, settings.provider.model_id);
            assert_eq!(disabled.provider.headers, settings.provider.headers);
        }
    }
}
