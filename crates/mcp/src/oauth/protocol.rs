use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// A sensitive OAuth string whose debug representation is always redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpOAuthSecret(String);

impl McpOAuthSecret {
    /// Wrap a sensitive OAuth value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Expose the sensitive value for protocol transmission.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for McpOAuthSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// OAuth authorization endpoint response type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum McpOAuthResponseType {
    /// Authorization Code response.
    #[serde(rename = "code")]
    Code,
}

/// OAuth grant type used by token and registration requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum McpOAuthGrantType {
    /// Authorization Code grant.
    #[serde(rename = "authorization_code")]
    AuthorizationCode,
    /// Refresh Token grant.
    #[serde(rename = "refresh_token")]
    RefreshToken,
}

/// PKCE code challenge method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpPkceChallengeMethod {
    /// SHA-256 code challenge method required by MCP.
    #[serde(rename = "S256")]
    S256,
    /// Plain method retained only for standards-compatible parsing.
    #[serde(rename = "plain")]
    Plain,
}

/// Token endpoint client authentication method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTokenEndpointAuthMethod {
    /// Public client without client authentication.
    #[serde(rename = "none")]
    None,
}

/// OAuth authorization request parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorizationRequest {
    /// Authorization response type.
    pub response_type: McpOAuthResponseType,
    /// Exact public client identifier.
    pub client_id: String,
    /// Registered redirect URI.
    pub redirect_uri: String,
    /// Requested space-delimited scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Opaque request correlation value.
    pub state: McpOAuthSecret,
    /// PKCE challenge.
    pub code_challenge: String,
    /// PKCE challenge method.
    pub code_challenge_method: McpPkceChallengeMethod,
    /// RFC 8707 protected resource identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// Successful authorization response parameters.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorizationSuccess {
    /// Single-use authorization code.
    pub code: McpOAuthSecret,
    /// Request correlation value.
    pub state: McpOAuthSecret,
    /// RFC 9207 authorization server issuer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
}

impl fmt::Debug for McpAuthorizationSuccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpAuthorizationSuccess")
            .field("code", &self.code)
            .field("state", &self.state)
            .field("iss", &self.iss)
            .finish()
    }
}

/// Standard OAuth authorization endpoint error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthorizationErrorCode {
    /// Request is malformed.
    InvalidRequest,
    /// Client is not authorized to make this request.
    UnauthorizedClient,
    /// Resource owner denied the request.
    AccessDenied,
    /// Response type is unsupported.
    UnsupportedResponseType,
    /// Requested scope is invalid or unsupported.
    InvalidScope,
    /// Authorization server encountered an error.
    ServerError,
    /// Authorization server is temporarily unavailable.
    TemporarilyUnavailable,
}

/// Standard authorization error response parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorizationErrorParameters {
    /// Standard error code.
    pub error: McpAuthorizationErrorCode,
    /// Human-readable diagnostic text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
    /// Error documentation URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_uri: Option<String>,
    /// Request correlation value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<McpOAuthSecret>,
}

/// Parsed authorization redirect parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAuthorizationResponse {
    /// Successful authorization response.
    Success(McpAuthorizationSuccess),
    /// Standard authorization error response.
    Error(McpAuthorizationErrorParameters),
}

impl McpAuthorizationResponse {
    /// Parse redirect query parameters while rejecting duplicates and mixed outcomes.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate, unknown, missing, or mixed success/error parameters.
    pub fn parse_query(query: &str) -> Result<Self> {
        let mut parameters = BTreeMap::new();
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if parameters
                .insert(name.into_owned(), value.into_owned())
                .is_some()
            {
                return Err(Error::protocol(
                    "OAuth authorization response contains a duplicate parameter",
                ));
            }
        }
        let allowed = if parameters.contains_key("error") {
            ["error", "error_description", "error_uri", "state"].as_slice()
        } else {
            ["code", "state", "iss"].as_slice()
        };
        if parameters
            .keys()
            .any(|name| !allowed.contains(&name.as_str()))
        {
            return Err(Error::protocol(
                "OAuth authorization response contains an unknown or mixed parameter",
            ));
        }
        if let Some(error) = parameters.remove("error") {
            let error = match error.as_str() {
                "invalid_request" => McpAuthorizationErrorCode::InvalidRequest,
                "unauthorized_client" => McpAuthorizationErrorCode::UnauthorizedClient,
                "access_denied" => McpAuthorizationErrorCode::AccessDenied,
                "unsupported_response_type" => McpAuthorizationErrorCode::UnsupportedResponseType,
                "invalid_scope" => McpAuthorizationErrorCode::InvalidScope,
                "server_error" => McpAuthorizationErrorCode::ServerError,
                "temporarily_unavailable" => McpAuthorizationErrorCode::TemporarilyUnavailable,
                _ => return Err(Error::protocol("unknown OAuth authorization error code")),
            };
            return Ok(Self::Error(McpAuthorizationErrorParameters {
                error,
                error_description: parameters.remove("error_description"),
                error_uri: parameters.remove("error_uri"),
                state: parameters.remove("state").map(McpOAuthSecret::new),
            }));
        }
        let code = parameters
            .remove("code")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::protocol("OAuth authorization response requires code"))?;
        let state = parameters
            .remove("state")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::protocol("OAuth authorization response requires state"))?;
        Ok(Self::Success(McpAuthorizationSuccess {
            code: McpOAuthSecret::new(code),
            state: McpOAuthSecret::new(state),
            iss: parameters.remove("iss"),
        }))
    }
}

/// Public client identity sent to a token endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPublicClientIdentity {
    /// Exact public client identifier.
    pub client_id: String,
}

/// Authorization Code token request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorizationCodeTokenRequest {
    /// Grant type, which must be `authorization_code`.
    pub grant_type: McpOAuthGrantType,
    /// Authorization code received from the authorization endpoint.
    pub code: McpOAuthSecret,
    /// Redirect URI used in the authorization request.
    pub redirect_uri: String,
    /// PKCE verifier.
    pub code_verifier: McpOAuthSecret,
    /// Public client identifier.
    pub client_id: String,
    /// RFC 8707 protected resource identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl fmt::Debug for McpAuthorizationCodeTokenRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpAuthorizationCodeTokenRequest")
            .field("grant_type", &self.grant_type)
            .field("code", &self.code)
            .field("redirect_uri", &self.redirect_uri)
            .field("code_verifier", &self.code_verifier)
            .field("client_id", &self.client_id)
            .field("resource", &self.resource)
            .finish()
    }
}

/// Refresh Token request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRefreshTokenRequest {
    /// Grant type, which must be `refresh_token`.
    pub grant_type: McpOAuthGrantType,
    /// Refresh token issued by the authorization server.
    pub refresh_token: McpOAuthSecret,
    /// Public client identifier.
    pub client_id: String,
    /// Requested scope reduction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// RFC 8707 protected resource identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl fmt::Debug for McpRefreshTokenRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpRefreshTokenRequest")
            .field("grant_type", &self.grant_type)
            .field("refresh_token", &self.refresh_token)
            .field("client_id", &self.client_id)
            .field("scope", &self.scope)
            .field("resource", &self.resource)
            .finish()
    }
}

/// OAuth bearer token type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpOAuthTokenType {
    /// Bearer access token.
    #[serde(rename = "Bearer", alias = "bearer")]
    Bearer,
}

/// Successful token endpoint response.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTokenResponse {
    /// Issued access token.
    pub access_token: McpOAuthSecret,
    /// Access token type.
    pub token_type: McpOAuthTokenType,
    /// Lifetime in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    /// Refresh token, when issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<McpOAuthSecret>,
    /// Granted space-delimited scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl fmt::Debug for McpTokenResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpTokenResponse")
            .field("access_token", &self.access_token)
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &self.refresh_token)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Standard token endpoint error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTokenErrorCode {
    /// Request is malformed.
    InvalidRequest,
    /// Client authentication failed.
    InvalidClient,
    /// Grant is invalid, expired, revoked, or mismatched.
    InvalidGrant,
    /// Client is not authorized for the grant.
    UnauthorizedClient,
    /// Grant type is unsupported.
    UnsupportedGrantType,
    /// Requested scope is invalid or unsupported.
    InvalidScope,
}

/// Standard token endpoint error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTokenError {
    /// Standard error code.
    pub error: McpTokenErrorCode,
    /// Human-readable diagnostic text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
    /// Error documentation URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_uri: Option<String>,
}

/// Revocation token type hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpRevocationTokenTypeHint {
    /// Access token hint.
    AccessToken,
    /// Refresh token hint.
    RefreshToken,
}

/// RFC 7009 token revocation request.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRevocationRequest {
    /// Token to revoke. Revocation is idempotent even when this token is unknown.
    pub token: McpOAuthSecret,
    /// Optional token type hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_type_hint: Option<McpRevocationTokenTypeHint>,
    /// Public client identifier.
    pub client_id: String,
}

impl fmt::Debug for McpRevocationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpRevocationRequest")
            .field("token", &self.token)
            .field("token_type_hint", &self.token_type_hint)
            .field("client_id", &self.client_id)
            .finish()
    }
}

/// Successful RFC 7009 revocation response, whose HTTP body is empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpRevocationResponse;

/// Standard token revocation endpoint error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpRevocationErrorCode {
    /// Request is malformed.
    InvalidRequest,
    /// Client authentication failed.
    InvalidClient,
    /// The token type cannot be revoked by this server.
    UnsupportedTokenType,
}

/// Token revocation endpoint error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRevocationError {
    /// Standard error code.
    pub error: McpRevocationErrorCode,
    /// Human-readable diagnostic text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
    /// Error documentation URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_uri: Option<String>,
}

/// Public OAuth client registration metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPublicClientRegistrationRequest {
    /// Registered redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Requested grant types.
    pub grant_types: Vec<McpOAuthGrantType>,
    /// Requested response types.
    pub response_types: Vec<McpOAuthResponseType>,
    /// Public-client token endpoint authentication method.
    pub token_endpoint_auth_method: McpTokenEndpointAuthMethod,
    /// Human-readable client name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

impl McpPublicClientRegistrationRequest {
    /// Validate and normalize an RFC 7591 public-client request.
    ///
    /// # Errors
    ///
    /// Returns a standards-shaped registration error for unsupported metadata.
    pub fn validate_and_normalize(
        mut self,
        allow_loopback_http: bool,
        max_redirect_uris: usize,
        max_field_bytes: usize,
    ) -> std::result::Result<Self, McpClientRegistrationError> {
        let submitted_redirect_count = self.redirect_uris.len();
        self.redirect_uris.sort();
        self.redirect_uris.dedup();
        let invalid_redirect = self.redirect_uris.is_empty()
            || submitted_redirect_count > max_redirect_uris
            || self.redirect_uris.len() > max_redirect_uris
            || self.redirect_uris.iter().any(|redirect| {
                redirect.len() > max_field_bytes
                    || validate_public_redirect_uri(redirect, allow_loopback_http).is_err()
            });
        if invalid_redirect {
            return Err(McpClientRegistrationError {
                error: McpClientRegistrationErrorCode::InvalidRedirectUri,
                error_description: None,
            });
        }
        if self.client_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > max_field_bytes || name.chars().any(char::is_control)
        }) || self.grant_types.len() > 8
            || self.response_types.len() > 8
            || self.token_endpoint_auth_method != McpTokenEndpointAuthMethod::None
            || self.response_types != [McpOAuthResponseType::Code]
            || self
                .grant_types
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                != std::collections::BTreeSet::from([
                    McpOAuthGrantType::AuthorizationCode,
                    McpOAuthGrantType::RefreshToken,
                ])
        {
            return Err(McpClientRegistrationError {
                error: McpClientRegistrationErrorCode::InvalidClientMetadata,
                error_description: None,
            });
        }
        self.grant_types = vec![
            McpOAuthGrantType::AuthorizationCode,
            McpOAuthGrantType::RefreshToken,
        ];
        Ok(self)
    }
}

/// Dynamic client registration response.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpClientRegistrationResponse {
    /// Issued client identifier.
    pub client_id: String,
    /// Issued client secret, which public-client helpers reject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<McpOAuthSecret>,
    /// Client secret expiration as Unix seconds, when a secret is issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<u64>,
    /// Registration access token, when issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_access_token: Option<McpOAuthSecret>,
    /// Registration management URI, when issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_client_uri: Option<String>,
    /// Client identifier issue time as Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<u64>,
    /// Registered redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Registered grant types.
    pub grant_types: Vec<McpOAuthGrantType>,
    /// Registered response types.
    pub response_types: Vec<McpOAuthResponseType>,
    /// Registered token endpoint authentication method.
    pub token_endpoint_auth_method: McpTokenEndpointAuthMethod,
    /// Human-readable client name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

impl fmt::Debug for McpClientRegistrationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpClientRegistrationResponse")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .field("client_secret_expires_at", &self.client_secret_expires_at)
            .field("registration_access_token", &self.registration_access_token)
            .field("registration_client_uri", &self.registration_client_uri)
            .field("client_id_issued_at", &self.client_id_issued_at)
            .field("redirect_uris", &self.redirect_uris)
            .field("grant_types", &self.grant_types)
            .field("response_types", &self.response_types)
            .field(
                "token_endpoint_auth_method",
                &self.token_endpoint_auth_method,
            )
            .field("client_name", &self.client_name)
            .finish()
    }
}

impl McpClientRegistrationResponse {
    /// Validate that a DCR response registered the requested public client
    /// without issuing registration credentials.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, confidential, or mismatched responses.
    pub fn validate_public_response(
        &self,
        request: &McpPublicClientRegistrationRequest,
    ) -> Result<()> {
        let canonical = |values: &[String]| {
            let mut values = values.to_vec();
            values.sort();
            values.dedup();
            values
        };
        let response_grants = self
            .grant_types
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let request_grants = request
            .grant_types
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if self.client_id.is_empty()
            || self.client_id.len() > 2048
            || self.client_secret.is_some()
            || self.client_secret_expires_at.is_some()
            || self.registration_access_token.is_some()
            || self.registration_client_uri.is_some()
            || canonical(&self.redirect_uris) != canonical(&request.redirect_uris)
            || response_grants != request_grants
            || self.response_types != request.response_types
            || self.token_endpoint_auth_method != McpTokenEndpointAuthMethod::None
            || self.client_name != request.client_name
        {
            return Err(Error::protocol(
                "OAuth dynamic registration response is not a matching public client",
            ));
        }
        Ok(())
    }
}

/// RFC 7591 dynamic client registration error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpClientRegistrationErrorCode {
    /// A redirect URI is invalid.
    InvalidRedirectUri,
    /// Client metadata is invalid.
    InvalidClientMetadata,
    /// A software statement is invalid.
    InvalidSoftwareStatement,
    /// A software statement is valid but not approved.
    UnapprovedSoftwareStatement,
}

/// RFC 7591 dynamic client registration error response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpClientRegistrationError {
    /// Standard registration error code.
    pub error: McpClientRegistrationErrorCode,
    /// Human-readable diagnostic text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

/// OAuth Client ID Metadata Document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpClientMetadataDocument {
    /// Exact document URL used as the client identifier.
    pub client_id: String,
    /// Registered redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Allowed grant types.
    pub grant_types: Vec<McpOAuthGrantType>,
    /// Allowed response types.
    pub response_types: Vec<McpOAuthResponseType>,
    /// Public-client token endpoint authentication method.
    pub token_endpoint_auth_method: McpTokenEndpointAuthMethod,
    /// Human-readable client name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// Unknown client metadata extensions.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl McpClientMetadataDocument {
    /// Verify that the document's client identifier exactly matches its retrieval URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the client identifier differs.
    pub fn validate_client_id(
        &self,
        expected_client_id: &str,
    ) -> std::result::Result<(), McpClientMetadataError> {
        if self.client_id == expected_client_id {
            Ok(())
        } else {
            Err(McpClientMetadataError::ClientIdMismatch)
        }
    }

    /// Validate the metadata as an MCP public Authorization Code client.
    ///
    /// # Errors
    ///
    /// Returns an error when public-client, grant, response, PKCE, or redirect
    /// requirements are not met.
    pub fn validate_public_client(
        &self,
        expected_client_id: &str,
        allow_loopback_http: bool,
    ) -> std::result::Result<(), McpClientMetadataError> {
        self.validate_client_id(expected_client_id)?;
        if self.client_id.len() > 2048
            || self.redirect_uris.is_empty()
            || self.redirect_uris.len() > 16
            || self.grant_types.len() > 8
            || self.response_types.len() > 8
            || self.extra.len() > 64
            || self
                .redirect_uris
                .iter()
                .any(|redirect_uri| redirect_uri.len() > 2048)
            || self
                .client_name
                .as_ref()
                .is_some_and(|client_name| client_name.len() > 256)
        {
            return Err(McpClientMetadataError::InvalidRedirectUri);
        }
        for redirect_uri in &self.redirect_uris {
            validate_public_redirect_uri(redirect_uri, allow_loopback_http)?;
        }
        if self.token_endpoint_auth_method != McpTokenEndpointAuthMethod::None {
            return Err(McpClientMetadataError::UnsupportedClientMetadata);
        }
        let grants = self
            .grant_types
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if grants
            != std::collections::BTreeSet::from([
                McpOAuthGrantType::AuthorizationCode,
                McpOAuthGrantType::RefreshToken,
            ])
            || self.response_types != [McpOAuthResponseType::Code]
            || self
                .extra
                .get("code_challenge_methods")
                .and_then(Value::as_array)
                .is_none_or(|methods| methods.len() != 1 || methods[0].as_str() != Some("S256"))
        {
            return Err(McpClientMetadataError::UnsupportedClientMetadata);
        }
        Ok(())
    }
}

/// Client ID Metadata Document validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpClientMetadataError {
    /// The document's `client_id` did not exactly match its retrieval URL.
    ClientIdMismatch,
    /// A redirect URI is malformed or unsafe.
    InvalidRedirectUri,
    /// Public-client protocol metadata is unsupported.
    UnsupportedClientMetadata,
}

impl fmt::Display for McpClientMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientIdMismatch => {
                formatter.write_str("OAuth client metadata client_id mismatch")
            }
            Self::InvalidRedirectUri => formatter.write_str("OAuth client redirect URI is invalid"),
            Self::UnsupportedClientMetadata => {
                formatter.write_str("OAuth public client metadata is unsupported")
            }
        }
    }
}

impl std::error::Error for McpClientMetadataError {}

/// Validate a public-client redirect URI.
///
/// HTTPS redirects are accepted. Native loopback HTTP redirects require an
/// explicit policy. Fragments, credentials, and wildcard hosts are rejected.
///
/// # Errors
///
/// Returns an error when the URI is malformed or violates the selected policy.
pub fn validate_public_redirect_uri(
    value: &str,
    allow_loopback_http: bool,
) -> std::result::Result<(), McpClientMetadataError> {
    if value.trim() != value || value.contains('\\') {
        return Err(McpClientMetadataError::InvalidRedirectUri);
    }
    let url = url::Url::parse(value).map_err(|_| McpClientMetadataError::InvalidRedirectUri)?;
    let host = url
        .host_str()
        .ok_or(McpClientMetadataError::InvalidRedirectUri)?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if url.cannot_be_a_base()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || host.contains('*')
        || !(url.scheme() == "https" || allow_loopback_http && url.scheme() == "http" && loopback)
    {
        return Err(McpClientMetadataError::InvalidRedirectUri);
    }
    Ok(())
}

/// JWT audience claim in its permitted single-string or string-array form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpJwtAudience {
    /// One audience.
    One(String),
    /// Multiple audiences.
    Many(Vec<String>),
}

/// RFC 9068 JWT access-token claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpJwtAccessTokenClaims {
    /// Token issuer.
    pub iss: String,
    /// Subject identifier.
    pub sub: String,
    /// Intended audience.
    pub aud: McpJwtAudience,
    /// Expiration time as Unix seconds.
    pub exp: u64,
    /// Issued-at time as Unix seconds.
    pub iat: u64,
    /// Not-before time as Unix seconds.
    pub nbf: u64,
    /// JWT identifier.
    pub jti: String,
    /// Space-delimited granted scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Client identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Authorization grant linked to the persisted token record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
}

/// JOSE header fields required by the RFC 9068 access-token profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpJwtAccessTokenHeader {
    /// Asymmetric signing algorithm.
    pub alg: String,
    /// JWT type, expected to be `at+jwt`.
    pub typ: String,
    /// Signing key identifier.
    pub kid: String,
}

impl McpJwtAccessTokenHeader {
    /// Validate the JOSE header for an RFC 9068 access token.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing key ID, wrong type, or symmetric/unsigned algorithm.
    pub fn validate(&self) -> Result<()> {
        if self.typ != "at+jwt" {
            return Err(Error::protocol("OAuth access token JWT typ must be at+jwt"));
        }
        if self.kid.is_empty() {
            return Err(Error::protocol("OAuth access token JWT requires kid"));
        }
        if self.alg == "none" || self.alg.starts_with("HS") {
            return Err(Error::protocol(
                "OAuth access token JWT requires asymmetric signing",
            ));
        }
        Ok(())
    }
}
