use std::{
    collections::BTreeMap,
    fmt,
    net::IpAddr,
    time::{Duration, SystemTime},
};

use futures_util::StreamExt as _;
use mcp::{
    McpOAuthGrantType, McpOAuthSecret, McpRefreshTokenRequest, McpTokenError, McpTokenErrorCode,
    McpTokenResponse,
};
use reqwest::{Client, Response, Url, header::CONTENT_TYPE};

use super::{
    mcp_oauth_state, should_refresh_mcp_token, validate_mcp_access_token, validate_mcp_credential,
};
use crate::{
    AuthTarget, AuthenticationFailureReason, FileCredentialStore, McpOAuthState, McpRefreshClaim,
    McpRefreshClaimResult, OAuthCredential, SessionMcpServerRuntimeSettings,
    SessionOAuthJwtMetadata, SessionOAuthJwtToken, SessionOAuthProviderSettings,
    SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle, StoredCredential,
};

const TOKEN_RESPONSE_LIMIT: usize = 64 * 1024;

/// Whether credential refresh follows the normal expiry threshold or is explicitly forced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Refresh only credentials at or near expiry.
    Normal,
    /// Refresh regardless of expiry while retaining every other validation requirement.
    Forced,
}

/// Sanitized error from outbound credential refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRefreshError {
    target: AuthTarget,
    reason: AuthenticationFailureReason,
    message: String,
    disposition: RefreshFailureDisposition,
    internal: bool,
}

impl CredentialRefreshError {
    /// Target whose credential could not be refreshed.
    #[must_use]
    pub const fn target(&self) -> &AuthTarget {
        &self.target
    }

    /// Machine-readable failure reason.
    #[must_use]
    pub const fn reason(&self) -> AuthenticationFailureReason {
        self.reason
    }

    /// Sanitized human-readable failure message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the failure came from credential storage rather than OAuth validation/exchange.
    #[must_use]
    pub const fn is_internal(&self) -> bool {
        self.internal
    }
}

impl fmt::Display for CredentialRefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CredentialRefreshError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshFailureDisposition {
    RetrySafe,
    ReauthorizationRequired,
    Indeterminate,
}

/// Reusable provider and remote MCP outbound credential refresh coordinator.
#[derive(Debug, Clone)]
pub struct CredentialRefreshCoordinator {
    credential_store: FileCredentialStore,
    allow_loopback_http: bool,
}

impl CredentialRefreshCoordinator {
    /// Create a coordinator over the supplied durable credential store.
    #[must_use]
    pub const fn new(credential_store: FileCredentialStore) -> Self {
        Self {
            credential_store,
            allow_loopback_http: false,
        }
    }

    #[cfg(test)]
    pub(super) const fn with_loopback_test_mode(mut self) -> Self {
        self.allow_loopback_http = true;
        self
    }

    /// Refresh one provider credential and return the committed current credential.
    ///
    /// `Ok(None)` means normal runtime resolution found no usable OAuth credential.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for invalid configuration, rejected exchanges, unsafe
    /// destinations, invalid responses, or storage failures.
    pub async fn refresh_provider(
        &self,
        provider_key: &str,
        definition: &SessionOAuthProviderSettings,
        mode: RefreshMode,
    ) -> Result<Option<OAuthCredential>, CredentialRefreshError> {
        let target = AuthTarget::provider(provider_key);
        let credential_key = target.credential_key();
        let initial = self.read_provider(&target, &credential_key).await?;
        let Some(initial) = initial else {
            return if mode == RefreshMode::Normal {
                Ok(None)
            } else {
                Err(refresh_error(
                    target,
                    AuthenticationFailureReason::CredentialRequired,
                    "provider OAuth credential is required",
                    RefreshFailureDisposition::ReauthorizationRequired,
                ))
            };
        };
        if mode == RefreshMode::Normal && !should_refresh_provider(&initial) {
            return Ok(Some(initial));
        }

        let _guard = self
            .credential_store
            .lock_provider_refresh(&credential_key)
            .await;
        let current = self.read_provider(&target, &credential_key).await?;
        let Some(current) = current else {
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "provider OAuth credential is required",
                RefreshFailureDisposition::ReauthorizationRequired,
            ));
        };
        if !credentials_match(&initial, &current)
            || (mode == RefreshMode::Normal && !should_refresh_provider(&current))
        {
            return Ok(Some(current));
        }
        if current.refresh.is_empty() {
            return if mode == RefreshMode::Normal {
                Ok(None)
            } else {
                Err(refresh_error(
                    target,
                    AuthenticationFailureReason::CredentialRequired,
                    "provider OAuth credential does not include a refresh token",
                    RefreshFailureDisposition::ReauthorizationRequired,
                ))
            };
        }

        let refreshed =
            refresh_provider_oauth(&target, definition, &current, self.allow_loopback_http).await?;
        let mut committed = false;
        let candidate = refreshed.clone();
        let baseline = current.clone();
        let stored = self
            .credential_store
            .modify(&credential_key, |stored| match stored {
                Some(StoredCredential::OAuth(existing))
                    if credentials_match(&existing, &baseline) =>
                {
                    committed = true;
                    Some(StoredCredential::OAuth(candidate))
                }
                other => other,
            })
            .await
            .map_err(|_| storage_error(target.clone(), "failed to store provider credential"))?;
        if committed {
            return Ok(Some(refreshed));
        }
        match stored {
            Some(StoredCredential::OAuth(current)) => Ok(Some(current)),
            _ => Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "provider OAuth credential changed during refresh",
                RefreshFailureDisposition::ReauthorizationRequired,
            )),
        }
    }

    /// Refresh one remote MCP credential and return the committed current state.
    ///
    /// `Ok(None)` means no credential exists for normal runtime resolution.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error while preserving generation, claim, resource-binding,
    /// and indeterminate token-family protections.
    #[allow(clippy::too_many_lines)]
    pub async fn refresh_mcp(
        &self,
        server: &SessionMcpServerRuntimeSettings,
        mode: RefreshMode,
    ) -> Result<Option<McpOAuthState>, CredentialRefreshError> {
        let target = AuthTarget::mcp_server(&server.key);
        let credential_key = target.credential_key();
        let stored = self
            .credential_store
            .read(&credential_key)
            .await
            .map_err(|_| storage_error(target.clone(), "failed to read MCP credential"))?;
        let Some(stored) = stored else {
            return if mode == RefreshMode::Normal {
                Ok(None)
            } else {
                Err(refresh_error(
                    target,
                    AuthenticationFailureReason::CredentialRequired,
                    "MCP OAuth credential is required",
                    RefreshFailureDisposition::ReauthorizationRequired,
                ))
            };
        };
        let legacy = match &stored {
            StoredCredential::OAuth(credential) => Some(credential.clone()),
            StoredCredential::McpOAuth(_) => None,
        };
        let Some(mut current) = mcp_oauth_state(stored, &server.url) else {
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::ConfigurationRequired,
                "MCP OAuth credential binding is invalid",
                RefreshFailureDisposition::RetrySafe,
            ));
        };
        validate_mcp_state(&target, server, &current)?;
        if let Some(legacy) = legacy {
            let upgraded = current.clone();
            let stored = self
                .credential_store
                .upgrade_legacy_mcp(&credential_key, &legacy, upgraded)
                .await
                .map_err(|_| storage_error(target.clone(), "failed to upgrade MCP credential"))?;
            let Some(StoredCredential::McpOAuth(stored)) = stored else {
                return Err(refresh_error(
                    target,
                    AuthenticationFailureReason::CredentialRequired,
                    "MCP OAuth credential changed during legacy upgrade",
                    RefreshFailureDisposition::ReauthorizationRequired,
                ));
            };
            current = stored;
            validate_mcp_state(&target, server, &current)?;
        }
        if mode == RefreshMode::Normal && !should_refresh_mcp_token(&current) {
            return Ok(Some(current));
        }
        if current
            .token
            .refresh_token
            .as_deref()
            .is_none_or(str::is_empty)
        {
            if mode == RefreshMode::Normal && !current.token.expires_by(now_ms()) {
                return Ok(Some(current));
            }
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential does not include a refresh token",
                RefreshFailureDisposition::ReauthorizationRequired,
            ));
        }

        let generation = current.token.refresh_generation;
        let claim = McpRefreshClaim {
            generation,
            claim_id: uuid::Uuid::now_v7().to_string(),
            lease_expires_at_ms: now_ms().saturating_add(30_000),
        };
        match self
            .credential_store
            .claim_mcp_refresh(&credential_key, claim.clone(), now_ms())
            .await
            .map_err(|_| storage_error(target.clone(), "failed to claim MCP credential refresh"))?
        {
            McpRefreshClaimResult::Current(current) => {
                validate_mcp_state(&target, server, &current)?;
                if !mcp_refresh_result_is_usable(mode, &current) {
                    return Err(refresh_error(
                        target,
                        AuthenticationFailureReason::CredentialRequired,
                        "MCP OAuth refresh did not produce a usable credential",
                        RefreshFailureDisposition::ReauthorizationRequired,
                    ));
                }
                return Ok(Some(*current));
            }
            McpRefreshClaimResult::ReauthorizationRequired => {
                return Err(refresh_error(
                    target,
                    AuthenticationFailureReason::CredentialRequired,
                    "MCP OAuth refresh requires reauthorization",
                    RefreshFailureDisposition::ReauthorizationRequired,
                ));
            }
            McpRefreshClaimResult::Claimed(_) => {}
        }

        let refreshed = match refresh_mcp_oauth(&target, &current, self.allow_loopback_http).await {
            Ok(credential) => credential,
            Err(error) => {
                let indeterminate = matches!(
                    error.disposition,
                    RefreshFailureDisposition::Indeterminate
                        | RefreshFailureDisposition::ReauthorizationRequired
                );
                self.credential_store
                    .abort_mcp_refresh(&credential_key, generation, &claim.claim_id, indeterminate)
                    .await
                    .map_err(|_| {
                        storage_error(target.clone(), "failed to abort MCP credential refresh")
                    })?;
                if mode == RefreshMode::Normal
                    && error.disposition == RefreshFailureDisposition::Indeterminate
                    && !current.token.expires_by(now_ms())
                {
                    return Ok(Some(current));
                }
                return Err(error);
            }
        };
        if refreshed.token.expires_by(now_ms()) {
            self.credential_store
                .abort_mcp_refresh(&credential_key, generation, &claim.claim_id, true)
                .await
                .map_err(|_| {
                    storage_error(target.clone(), "failed to abort MCP credential refresh")
                })?;
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::ResponseInvalid,
                "MCP OAuth refresh produced an expired credential",
                RefreshFailureDisposition::Indeterminate,
            ));
        }
        let committed = self
            .credential_store
            .commit_mcp_refresh(
                &credential_key,
                generation,
                &claim.claim_id,
                refreshed.clone(),
            )
            .await
            .map_err(|_| storage_error(target.clone(), "failed to store MCP credential"))?;
        if committed {
            return Ok(Some(refreshed));
        }
        let stored = self
            .credential_store
            .read(&credential_key)
            .await
            .map_err(|_| storage_error(target.clone(), "failed to read MCP credential"))?;
        let Some(StoredCredential::McpOAuth(current)) = stored else {
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential changed during refresh",
                RefreshFailureDisposition::ReauthorizationRequired,
            ));
        };
        validate_mcp_state(&target, server, &current)?;
        if !mcp_refresh_result_is_usable(mode, &current) {
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential changed during refresh",
                RefreshFailureDisposition::ReauthorizationRequired,
            ));
        }
        Ok(Some(current))
    }

    async fn read_provider(
        &self,
        target: &AuthTarget,
        credential_key: &str,
    ) -> Result<Option<OAuthCredential>, CredentialRefreshError> {
        self.credential_store
            .read(credential_key)
            .await
            .map_err(|_| storage_error(target.clone(), "failed to read provider credential"))
            .map(|stored| match stored {
                Some(StoredCredential::OAuth(credential)) => Some(credential),
                _ => None,
            })
    }
}

fn validate_mcp_state(
    target: &AuthTarget,
    server: &SessionMcpServerRuntimeSettings,
    credential: &McpOAuthState,
) -> Result<(), CredentialRefreshError> {
    validate_mcp_credential(server, credential).map_err(|_| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP OAuth credential binding is invalid",
            RefreshFailureDisposition::RetrySafe,
        )
    })?;
    validate_mcp_access_token(&server.key, credential).map_err(|_| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth credential does not contain a supported bearer access token",
            RefreshFailureDisposition::ReauthorizationRequired,
        )
    })
}

fn credentials_match(left: &OAuthCredential, right: &OAuthCredential) -> bool {
    left.access == right.access
        && left.refresh == right.refresh
        && left.expires_at_ms == right.expires_at_ms
        && left.metadata == right.metadata
}

fn should_refresh_provider(credential: &OAuthCredential) -> bool {
    credential.expires_at_ms <= now_ms().saturating_add(super::OAUTH_REFRESH_AHEAD_MS)
}

fn mcp_refresh_result_is_usable(mode: RefreshMode, credential: &McpOAuthState) -> bool {
    match mode {
        RefreshMode::Normal => !should_refresh_mcp_token(credential),
        RefreshMode::Forced => !credential.token.expires_by(now_ms()),
    }
}

async fn refresh_provider_oauth(
    target: &AuthTarget,
    definition: &SessionOAuthProviderSettings,
    credential: &OAuthCredential,
    allow_loopback_http: bool,
) -> Result<OAuthCredential, CredentialRefreshError> {
    if definition.client_id.is_empty() || definition.token_url.is_empty() {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ConfigurationRequired,
            "provider OAuth refresh configuration is missing",
            RefreshFailureDisposition::RetrySafe,
        ));
    }
    let pairs = vec![
        ("grant_type", "refresh_token"),
        ("client_id", definition.client_id.as_str()),
        ("refresh_token", credential.refresh.as_str()),
    ];
    let (token_url, client) =
        oauth_client_for(target, &definition.token_url, allow_loopback_http).await?;
    let builder = client.post(token_url);
    let response = match definition.token_request_style {
        SessionOAuthTokenRequestStyle::Json => builder.json(&pairs_to_json(&pairs)).send().await,
        SessionOAuthTokenRequestStyle::FormUrlencoded => builder.form(&pairs).send().await,
    }
    .map_err(|_| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::NetworkFailed,
            "provider OAuth token request failed",
            RefreshFailureDisposition::Indeterminate,
        )
    })?;
    validate_response_peer(target, &response, allow_loopback_http)?;
    let status = response.status();
    let body = read_json_body(target, response).await?;
    if !status.is_success() {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::TokenExchangeFailed,
            "provider OAuth token endpoint rejected the refresh request",
            if status.is_server_error() {
                RefreshFailureDisposition::Indeterminate
            } else {
                RefreshFailureDisposition::ReauthorizationRequired
            },
        ));
    }
    token_response_to_credential(target, definition, credential, &body)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn refresh_mcp_oauth(
    target: &AuthTarget,
    credential: &McpOAuthState,
    allow_loopback_http: bool,
) -> Result<McpOAuthState, CredentialRefreshError> {
    let refresh_token = credential
        .token
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            refresh_error(
                target.clone(),
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential does not include a refresh token",
                RefreshFailureDisposition::ReauthorizationRequired,
            )
        })?;
    if credential.endpoints.token.is_empty() || credential.registration.client_id.is_empty() {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP OAuth refresh configuration is missing",
            RefreshFailureDisposition::RetrySafe,
        ));
    }
    let request = McpRefreshTokenRequest {
        grant_type: McpOAuthGrantType::RefreshToken,
        refresh_token: McpOAuthSecret::new(refresh_token),
        client_id: credential.registration.client_id.clone(),
        scope: None,
        resource: Some(credential.resource.clone()),
    };
    let (token_url, client) =
        oauth_client_for(target, &credential.endpoints.token, allow_loopback_http).await?;
    let response = client
        .post(token_url)
        .form(&request)
        .send()
        .await
        .map_err(|_| {
            refresh_error(
                target.clone(),
                AuthenticationFailureReason::NetworkFailed,
                "MCP OAuth token request failed",
                RefreshFailureDisposition::Indeterminate,
            )
        })?;
    validate_response_peer(target, &response, allow_loopback_http)?;
    let status = response.status();
    let body = read_json_body(target, response).await?;
    if !status.is_success() {
        let token_error = serde_json::from_slice::<McpTokenError>(&body).ok();
        let invalid_grant = token_error
            .as_ref()
            .is_some_and(|error| error.error == McpTokenErrorCode::InvalidGrant);
        return Err(refresh_error(
            target.clone(),
            if invalid_grant {
                AuthenticationFailureReason::CredentialRejected
            } else {
                AuthenticationFailureReason::TokenExchangeFailed
            },
            if invalid_grant {
                "MCP OAuth refresh token was rejected"
            } else {
                "MCP OAuth token endpoint rejected the refresh request"
            },
            if status.is_server_error() {
                RefreshFailureDisposition::Indeterminate
            } else {
                RefreshFailureDisposition::ReauthorizationRequired
            },
        ));
    }
    let token: McpTokenResponse = serde_json::from_slice(&body).map_err(|_| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth token response was invalid",
            RefreshFailureDisposition::Indeterminate,
        )
    })?;
    if token.access_token.expose_secret().is_empty() {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth token response did not include an access token",
            RefreshFailureDisposition::Indeterminate,
        ));
    }
    let mut next = credential.clone();
    token
        .access_token
        .expose_secret()
        .clone_into(&mut next.token.access_token);
    "Bearer".clone_into(&mut next.token.token_type);
    if let Some(refresh_token) = token.refresh_token
        && !refresh_token.expose_secret().is_empty()
    {
        next.token.refresh_token = Some(refresh_token.expose_secret().to_owned());
    }
    if let Some(scope) = token.scope {
        next.token.granted_scopes = scope.split_whitespace().map(ToOwned::to_owned).collect();
    }
    if let Some(seconds) = token.expires_in {
        next.token.expires_at_ms = now_ms().saturating_add(
            i64::try_from(seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
        );
    } else {
        next.token.set_expiry_unknown();
    }
    next.token.refresh_generation = credential.token.refresh_generation.saturating_add(1);
    if next.token.expires_by(now_ms()) {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "MCP OAuth refresh produced an expired credential",
            RefreshFailureDisposition::Indeterminate,
        ));
    }
    Ok(next)
}

fn pairs_to_json(pairs: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Object(
        pairs
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_owned(),
                    serde_json::Value::String((*value).to_owned()),
                )
            })
            .collect(),
    )
}

fn token_response_to_credential(
    target: &AuthTarget,
    definition: &SessionOAuthProviderSettings,
    current: &OAuthCredential,
    body: &[u8],
) -> Result<OAuthCredential, CredentialRefreshError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "provider OAuth token response was invalid",
            RefreshFailureDisposition::Indeterminate,
        )
    })?;
    let mut credential = OAuthCredential {
        access: value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        refresh: value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(&current.refresh)
            .to_owned(),
        expires_at_ms: value
            .get("expires_in")
            .and_then(serde_json::Value::as_i64)
            .map_or(0, |seconds| {
                now_ms().saturating_add(seconds.saturating_mul(1000))
            }),
        metadata: current.metadata.clone(),
    };
    for field in &definition.token_fields {
        let Some(field_value) = value.get(&field.source) else {
            continue;
        };
        match field.target {
            SessionOAuthTokenFieldTarget::Access => {
                field_value
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut credential.access);
            }
            SessionOAuthTokenFieldTarget::Refresh => {
                if let Some(refresh) = field_value.as_str().filter(|value| !value.is_empty()) {
                    refresh.clone_into(&mut credential.refresh);
                }
            }
            SessionOAuthTokenFieldTarget::ExpiresAtMs => {
                credential.expires_at_ms = field_value.as_i64().unwrap_or_default();
            }
            SessionOAuthTokenFieldTarget::ExpiresInSeconds => {
                credential.expires_at_ms = now_ms().saturating_add(
                    field_value
                        .as_i64()
                        .unwrap_or_default()
                        .saturating_mul(1000),
                );
            }
            SessionOAuthTokenFieldTarget::Metadata => {
                credential
                    .metadata
                    .insert(field.source.clone(), json_scalar_to_string(field_value));
            }
        }
    }
    extract_jwt_metadata(target, definition, &value, &mut credential.metadata)?;
    if credential.access.is_empty() {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "provider OAuth token response did not include an access token",
            RefreshFailureDisposition::Indeterminate,
        ));
    }
    Ok(credential)
}

fn extract_jwt_metadata(
    target: &AuthTarget,
    definition: &SessionOAuthProviderSettings,
    response: &serde_json::Value,
    metadata: &mut BTreeMap<String, String>,
) -> Result<(), CredentialRefreshError> {
    for extractor in &definition.jwt_metadata {
        if let Some(value) = jwt_metadata_value(extractor, response) {
            metadata.insert(extractor.metadata_key.clone(), value);
        }
        if extractor.required && !metadata.contains_key(&extractor.metadata_key) {
            return Err(refresh_error(
                target.clone(),
                AuthenticationFailureReason::ResponseInvalid,
                "provider OAuth token response did not include required metadata",
                RefreshFailureDisposition::Indeterminate,
            ));
        }
    }
    Ok(())
}

fn jwt_metadata_value(
    extractor: &SessionOAuthJwtMetadata,
    response: &serde_json::Value,
) -> Option<String> {
    let field = match extractor.token {
        SessionOAuthJwtToken::Access => "access_token",
        SessionOAuthJwtToken::Id => "id_token",
    };
    let jwt = response.get(field)?.as_str()?;
    let header = jsonwebtoken::decode_header(jwt).ok()?;
    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.insecure_disable_signature_validation();
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_aud = false;
    let claims = jsonwebtoken::decode::<serde_json::Value>(
        jwt,
        &jsonwebtoken::DecodingKey::from_secret(&[]),
        &validation,
    )
    .ok()?
    .claims;
    extractor
        .claim_names
        .iter()
        .find_map(|claim| jwt_claim_string(&claims, claim))
        .map(ToOwned::to_owned)
}

fn jwt_claim_string<'a>(value: &'a serde_json::Value, claim: &str) -> Option<&'a str> {
    if let Some(value) = value.get(claim).and_then(serde_json::Value::as_str) {
        return Some(value);
    }
    claim.match_indices('.').rev().find_map(|(index, _)| {
        let (prefix, suffix) = claim.split_at(index);
        let mut current = value.get(prefix)?;
        for segment in suffix.trim_start_matches('.').split('.') {
            current = current.get(segment)?;
        }
        current.as_str()
    })
}

fn json_scalar_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

async fn oauth_client_for(
    target: &AuthTarget,
    value: &str,
    allow_loopback_http: bool,
) -> Result<(Url, Client), CredentialRefreshError> {
    let url = Url::parse(value).map_err(|_| refresh_configuration_error(target))?;
    let host = url
        .host_str()
        .ok_or_else(|| refresh_configuration_error(target))?;
    let loopback_host = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.scheme() != "https"
            && !(allow_loopback_http && url.scheme() == "http" && loopback_host))
    {
        return Err(refresh_configuration_error(target));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| refresh_configuration_error(target))?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| {
            refresh_error(
                target.clone(),
                AuthenticationFailureReason::NetworkFailed,
                "OAuth token endpoint DNS resolution failed",
                RefreshFailureDisposition::RetrySafe,
            )
        })?
        .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !allows_address(address.ip(), allow_loopback_http))
    {
        return Err(refresh_configuration_error(target));
    }
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10));
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder.build().map_err(|_| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::NetworkFailed,
            "failed to build OAuth token client",
            RefreshFailureDisposition::RetrySafe,
        )
    })?;
    Ok((url, client))
}

fn validate_response_peer(
    target: &AuthTarget,
    response: &Response,
    allow_loopback_http: bool,
) -> Result<(), CredentialRefreshError> {
    if response
        .remote_addr()
        .is_some_and(|peer| !allows_address(peer.ip(), allow_loopback_http))
    {
        return Err(refresh_configuration_error(target));
    }
    Ok(())
}

async fn read_json_body(
    target: &AuthTarget,
    response: Response,
) -> Result<Vec<u8>, CredentialRefreshError> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if content_type != "application/json"
        && !(content_type.starts_with("application/") && content_type.ends_with("+json"))
    {
        return Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "OAuth token response content type is not JSON",
            RefreshFailureDisposition::Indeterminate,
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > TOKEN_RESPONSE_LIMIT as u64)
    {
        return Err(oversized_response_error(target));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            refresh_error(
                target.clone(),
                AuthenticationFailureReason::NetworkFailed,
                "OAuth token response read failed",
                RefreshFailureDisposition::Indeterminate,
            )
        })?;
        if chunk.len() > TOKEN_RESPONSE_LIMIT.saturating_sub(body.len()) {
            return Err(oversized_response_error(target));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn refresh_configuration_error(target: &AuthTarget) -> CredentialRefreshError {
    refresh_error(
        target.clone(),
        AuthenticationFailureReason::ConfigurationRequired,
        "OAuth token endpoint is not a permitted destination",
        RefreshFailureDisposition::RetrySafe,
    )
}

fn oversized_response_error(target: &AuthTarget) -> CredentialRefreshError {
    refresh_error(
        target.clone(),
        AuthenticationFailureReason::ResponseInvalid,
        "OAuth token response exceeds the size limit",
        RefreshFailureDisposition::Indeterminate,
    )
}

fn storage_error(target: AuthTarget, message: &str) -> CredentialRefreshError {
    CredentialRefreshError {
        target,
        reason: AuthenticationFailureReason::NetworkFailed,
        message: message.to_owned(),
        disposition: RefreshFailureDisposition::RetrySafe,
        internal: true,
    }
}

fn refresh_error(
    target: AuthTarget,
    reason: AuthenticationFailureReason,
    message: impl Into<String>,
    disposition: RefreshFailureDisposition,
) -> CredentialRefreshError {
    CredentialRefreshError {
        target,
        reason,
        message: message.into(),
        disposition,
        internal: false,
    }
}

fn allows_address(address: IpAddr, allow_loopback: bool) -> bool {
    if address.is_loopback() {
        return allow_loopback;
    }
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || octets[0] >= 240)
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return allows_address(IpAddr::V4(mapped), allow_loopback);
            }
            let segments = address.segments();
            !(address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
        }
    }
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}
