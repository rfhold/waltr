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
use oauth2::{AccessToken, RefreshToken, Scope};
use reqwest::{Client, Response, Url, header::CONTENT_TYPE};

use super::McpResolvedConnection;
use super::{should_refresh_mcp_token, validate_mcp_access_token, validate_mcp_credential};
use crate::{
    AuthTarget, AuthenticationFailureReason, CredentialStore, OAuthCredential,
    OAuthCredentialContext, OAuthCredentialRecord, OAuthRefreshClaim, OAuthRefreshClaimResult,
    SessionOAuthJwtMetadata, SessionOAuthJwtToken, SessionOAuthProviderSettings,
    SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle,
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
    credential_store: CredentialStore,
    allow_loopback_http: bool,
    configured_mcp_oauth_origins: Vec<Url>,
}

impl CredentialRefreshCoordinator {
    /// Create a coordinator over the supplied durable credential store.
    #[must_use]
    pub fn new(credential_store: impl Into<CredentialStore>) -> Self {
        Self {
            credential_store: credential_store.into(),
            allow_loopback_http: false,
            configured_mcp_oauth_origins: Vec::new(),
        }
    }

    /// Permit private HTTPS peers for host-supplied MCP OAuth origins.
    #[must_use]
    pub fn with_trusted_mcp_oauth_origins(mut self, origins: Vec<Url>) -> Self {
        self.configured_mcp_oauth_origins = origins;
        self
    }

    /// Permit private HTTPS peers for one host-supplied MCP OAuth origin.
    #[must_use]
    pub fn with_trusted_mcp_oauth_origin(self, origin: Url) -> Self {
        self.with_trusted_mcp_oauth_origins(vec![origin])
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
        let record = self.read_record(&target, &credential_key).await?;
        let Some(record) = record else {
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
        validate_provider_credential(&target, &record)?;
        if mode == RefreshMode::Normal && !should_refresh_provider(&record.credential) {
            return Ok(Some(record.credential));
        }
        if record.credential.token.refresh_token.is_none() {
            return if mode == RefreshMode::Normal && !record.credential.token.expires_by(now_ms()) {
                Ok(Some(record.credential))
            } else if mode == RefreshMode::Normal {
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
        let claim = refresh_claim(record.revision);
        if let Some(current) = self.claim_refresh(&target, &credential_key, &claim).await? {
            validate_provider_credential(&target, &current)?;
            return Ok(Some(current.credential));
        }
        let refreshed = match refresh_provider_oauth(
            &target,
            definition,
            &record.credential,
            self.allow_loopback_http,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => {
                self.abort_refresh(&target, &credential_key, &claim, &error)
                    .await?;
                return Err(error);
            }
        };
        self.commit_refresh(&target, &credential_key, claim, refreshed)
            .await
            .map(|record| Some(record.credential))
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
        server: &McpResolvedConnection,
        mode: RefreshMode,
    ) -> Result<Option<OAuthCredential>, CredentialRefreshError> {
        let target = AuthTarget::mcp_server(&server.key);
        let credential_key = target.credential_key();
        let record = self.read_record(&target, &credential_key).await?;
        let Some(record) = record else {
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
        validate_mcp_state(&target, server, &record)?;
        if mode == RefreshMode::Normal && !should_refresh_mcp_token(&record.credential) {
            return Ok(Some(record.credential));
        }
        if record.credential.token.refresh_token.is_none() {
            if mode == RefreshMode::Normal && !record.credential.token.expires_by(now_ms()) {
                return Ok(Some(record.credential));
            }
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential does not include a refresh token",
                RefreshFailureDisposition::ReauthorizationRequired,
            ));
        }

        let claim = refresh_claim(record.revision);
        if let Some(current) = self.claim_refresh(&target, &credential_key, &claim).await? {
            validate_mcp_state(&target, server, &current)?;
            if !mcp_refresh_result_is_usable(&current.credential) {
                return Err(refresh_error(
                    target,
                    AuthenticationFailureReason::CredentialRequired,
                    "MCP OAuth refresh did not produce a usable credential",
                    RefreshFailureDisposition::ReauthorizationRequired,
                ));
            }
            return Ok(Some(current.credential));
        }

        let refreshed = match refresh_mcp_oauth(
            &target,
            &record.credential,
            self.allow_loopback_http,
            &self.configured_mcp_oauth_origins,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => {
                self.abort_refresh(&target, &credential_key, &claim, &error)
                    .await?;
                if mode == RefreshMode::Normal
                    && error.disposition == RefreshFailureDisposition::Indeterminate
                    && !record.credential.token.expires_by(now_ms())
                {
                    return Ok(Some(record.credential));
                }
                return Err(error);
            }
        };
        if refreshed.token.expires_by(now_ms()) {
            self.credential_store
                .abort_refresh(&credential_key, claim.clone(), true)
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
        let current = self
            .commit_refresh(&target, &credential_key, claim, refreshed)
            .await?;
        validate_mcp_state(&target, server, &current)?;
        if !mcp_refresh_result_is_usable(&current.credential) {
            return Err(refresh_error(
                target,
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential changed during refresh",
                RefreshFailureDisposition::ReauthorizationRequired,
            ));
        }
        Ok(Some(current.credential))
    }

    async fn read_record(
        &self,
        target: &AuthTarget,
        credential_key: &str,
    ) -> Result<Option<OAuthCredentialRecord>, CredentialRefreshError> {
        let record = self
            .credential_store
            .read(credential_key)
            .await
            .map_err(|_| storage_error(target.clone(), "failed to read OAuth credential"))?;
        match record {
            Some(record) if record.reauthorization_required => Err(refresh_error(
                target.clone(),
                AuthenticationFailureReason::CredentialRequired,
                "OAuth credential requires reauthorization",
                RefreshFailureDisposition::ReauthorizationRequired,
            )),
            record => Ok(record),
        }
    }

    async fn claim_refresh(
        &self,
        target: &AuthTarget,
        credential_key: &str,
        claim: &OAuthRefreshClaim,
    ) -> Result<Option<OAuthCredentialRecord>, CredentialRefreshError> {
        match self
            .credential_store
            .claim_refresh(credential_key, claim.clone())
            .await
            .map_err(|_| {
                storage_error(target.clone(), "failed to claim OAuth credential refresh")
            })? {
            OAuthRefreshClaimResult::Claimed(_) => Ok(None),
            OAuthRefreshClaimResult::Current(current) => Ok(Some(*current)),
            OAuthRefreshClaimResult::ReauthorizationRequired => Err(refresh_error(
                target.clone(),
                AuthenticationFailureReason::CredentialRequired,
                "OAuth refresh requires reauthorization",
                RefreshFailureDisposition::ReauthorizationRequired,
            )),
        }
    }

    async fn abort_refresh(
        &self,
        target: &AuthTarget,
        credential_key: &str,
        claim: &OAuthRefreshClaim,
        error: &CredentialRefreshError,
    ) -> Result<(), CredentialRefreshError> {
        let indeterminate = matches!(
            error.disposition,
            RefreshFailureDisposition::Indeterminate
                | RefreshFailureDisposition::ReauthorizationRequired
        );
        self.credential_store
            .abort_refresh(credential_key, claim.clone(), indeterminate)
            .await
            .map_err(|_| {
                storage_error(
                    target.clone(),
                    &format!(
                        "failed to abort {} credential refresh",
                        oauth_target_label(target)
                    ),
                )
            })
    }

    async fn commit_refresh(
        &self,
        target: &AuthTarget,
        credential_key: &str,
        claim: OAuthRefreshClaim,
        refreshed: OAuthCredential,
    ) -> Result<OAuthCredentialRecord, CredentialRefreshError> {
        let committed = self
            .credential_store
            .commit_refresh(credential_key, claim, refreshed.clone())
            .await
            .map_err(|_| {
                storage_error(
                    target.clone(),
                    &format!("failed to store {} credential", oauth_target_label(target)),
                )
            })?;
        if committed {
            return self
                .read_record(target, credential_key)
                .await?
                .ok_or_else(|| {
                    storage_error(target.clone(), "stored OAuth credential is missing")
                });
        }
        self.read_record(target, credential_key)
            .await?
            .ok_or_else(|| {
                refresh_error(
                    target.clone(),
                    AuthenticationFailureReason::CredentialRequired,
                    "OAuth credential changed during refresh",
                    RefreshFailureDisposition::ReauthorizationRequired,
                )
            })
    }
}

const fn oauth_target_label(target: &AuthTarget) -> &'static str {
    match target {
        AuthTarget::Provider { .. } => "provider",
        AuthTarget::McpServer { .. } => "MCP",
    }
}

fn validate_mcp_state(
    target: &AuthTarget,
    server: &McpResolvedConnection,
    record: &OAuthCredentialRecord,
) -> Result<(), CredentialRefreshError> {
    let credential = &record.credential;
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

fn validate_provider_credential(
    target: &AuthTarget,
    record: &OAuthCredentialRecord,
) -> Result<(), CredentialRefreshError> {
    if record.credential.version == crate::OAUTH_CREDENTIAL_VERSION
        && matches!(
            record.credential.context,
            OAuthCredentialContext::Provider { .. }
        )
        && !record.credential.token.access_token.secret().is_empty()
        && record
            .credential
            .token
            .token_type
            .eq_ignore_ascii_case("bearer")
    {
        Ok(())
    } else {
        Err(refresh_error(
            target.clone(),
            AuthenticationFailureReason::ResponseInvalid,
            "provider OAuth credential is invalid",
            RefreshFailureDisposition::ReauthorizationRequired,
        ))
    }
}

fn should_refresh_provider(credential: &OAuthCredential) -> bool {
    credential
        .token
        .expires_by(now_ms().saturating_add(super::OAUTH_REFRESH_AHEAD_MS))
}

fn mcp_refresh_result_is_usable(credential: &OAuthCredential) -> bool {
    !credential.token.expires_by(now_ms())
}

fn refresh_claim(revision: u64) -> OAuthRefreshClaim {
    OAuthRefreshClaim {
        revision,
        claim_id: uuid::Uuid::now_v7().to_string(),
        lease_expires_at_ms: now_ms().saturating_add(30_000),
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
        (
            "refresh_token",
            credential
                .token
                .refresh_token
                .as_ref()
                .map(RefreshToken::secret)
                .ok_or_else(|| {
                    refresh_error(
                        target.clone(),
                        AuthenticationFailureReason::CredentialRequired,
                        "provider OAuth credential does not include a refresh token",
                        RefreshFailureDisposition::ReauthorizationRequired,
                    )
                })?,
        ),
    ];
    let (token_url, client) =
        oauth_client_for(target, &definition.token_url, allow_loopback_http, &[]).await?;
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
    validate_response_peer(target, &response, allow_loopback_http, &[])?;
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
    credential: &OAuthCredential,
    allow_loopback_http: bool,
    trusted_private_origins: &[Url],
) -> Result<OAuthCredential, CredentialRefreshError> {
    let context = credential.mcp_context().ok_or_else(|| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP OAuth credential binding is invalid",
            RefreshFailureDisposition::RetrySafe,
        )
    })?;
    let refresh_token = credential
        .token
        .refresh_token
        .as_ref()
        .map(RefreshToken::secret)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            refresh_error(
                target.clone(),
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth credential does not include a refresh token",
                RefreshFailureDisposition::ReauthorizationRequired,
            )
        })?;
    if context.endpoints.token.is_empty() || context.registration.client_id.is_empty() {
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
        client_id: context.registration.client_id.clone(),
        scope: None,
        resource: Some(context.resource.clone()),
    };
    let (token_url, client) = oauth_client_for(
        target,
        &context.endpoints.token,
        allow_loopback_http,
        trusted_private_origins,
    )
    .await?;
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
    validate_response_peer(
        target,
        &response,
        allow_loopback_http,
        trusted_private_origins,
    )?;
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
    next.token.access_token = AccessToken::new(token.access_token.expose_secret().to_owned());
    "Bearer".clone_into(&mut next.token.token_type);
    if let Some(refresh_token) = token.refresh_token
        && !refresh_token.expose_secret().is_empty()
    {
        next.token.refresh_token =
            Some(RefreshToken::new(refresh_token.expose_secret().to_owned()));
    }
    if let Some(scope) = token.scope {
        next.token.scopes = scope
            .split_whitespace()
            .map(|scope| Scope::new(scope.to_owned()))
            .collect();
    }
    if let Some(seconds) = token.expires_in {
        next.token.expires_at_ms = Some(
            now_ms().saturating_add(
                i64::try_from(seconds)
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1000),
            ),
        );
    } else {
        next.token.expires_at_ms = None;
    }
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

#[allow(
    clippy::too_many_lines,
    reason = "provider token mapping keeps configured field and JWT extraction in one validation path"
)]
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
    let access = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let refresh = value
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            current
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().to_owned())
        });
    let expires_at_ms = value
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .map(|seconds| now_ms().saturating_add(seconds.saturating_mul(1000)));
    let metadata = current.provider_metadata().cloned().ok_or_else(|| {
        refresh_error(
            target.clone(),
            AuthenticationFailureReason::ConfigurationRequired,
            "provider OAuth credential binding is invalid",
            RefreshFailureDisposition::RetrySafe,
        )
    })?;
    let mut credential = OAuthCredential::provider(
        crate::OAuthTokenSet::bearer(access, refresh, expires_at_ms),
        metadata,
    );
    for field in &definition.token_fields {
        let Some(field_value) = value.get(&field.source) else {
            continue;
        };
        match field.target {
            SessionOAuthTokenFieldTarget::Access => {
                credential.token.access_token =
                    AccessToken::new(field_value.as_str().unwrap_or_default().to_owned());
            }
            SessionOAuthTokenFieldTarget::Refresh => {
                if let Some(refresh) = field_value.as_str().filter(|value| !value.is_empty()) {
                    credential.token.refresh_token = Some(RefreshToken::new(refresh.to_owned()));
                }
            }
            SessionOAuthTokenFieldTarget::ExpiresAtMs => {
                credential.token.expires_at_ms = field_value.as_i64();
            }
            SessionOAuthTokenFieldTarget::ExpiresInSeconds => {
                credential.token.expires_at_ms = Some(
                    now_ms().saturating_add(
                        field_value
                            .as_i64()
                            .unwrap_or_default()
                            .saturating_mul(1000),
                    ),
                );
            }
            SessionOAuthTokenFieldTarget::Metadata => {
                credential
                    .provider_metadata_mut()
                    .expect("provider credential context")
                    .insert(field.source.clone(), json_scalar_to_string(field_value));
            }
        }
    }
    extract_jwt_metadata(
        target,
        definition,
        &value,
        credential
            .provider_metadata_mut()
            .expect("provider credential context"),
    )?;
    if credential.token.access_token.secret().is_empty() {
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
    trusted_private_origins: &[Url],
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
        || addresses.iter().any(|address| {
            !allows_address(
                &url,
                address.ip(),
                allow_loopback_http,
                trusted_private_origins,
            )
        })
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
    trusted_private_origins: &[Url],
) -> Result<(), CredentialRefreshError> {
    if response.remote_addr().is_some_and(|peer| {
        !allows_address(
            response.url(),
            peer.ip(),
            allow_loopback_http,
            trusted_private_origins,
        )
    }) {
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

fn allows_address(
    url: &Url,
    address: IpAddr,
    allow_loopback: bool,
    trusted_private_origins: &[Url],
) -> bool {
    if address.is_loopback() {
        return allow_loopback;
    }
    if trusted_private_origins
        .iter()
        .any(|trusted| trusted.origin() == url.origin())
        && match address {
            IpAddr::V4(address) => address.is_private(),
            IpAddr::V6(address) => address.is_unique_local(),
        }
    {
        return true;
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
                return allows_address(
                    url,
                    IpAddr::V4(mapped),
                    allow_loopback,
                    trusted_private_origins,
                );
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

#[cfg(test)]
mod destination_tests {
    use super::*;

    #[test]
    fn trusted_mcp_origin_accepts_only_rfc1918_and_ula_on_the_exact_origin() {
        let trusted = Url::parse("https://preview-waltr.example/mcp").expect("trusted URL");
        let token = Url::parse("https://preview-waltr.example/token").expect("token URL");
        let other = Url::parse("https://other.example/token").expect("token URL");
        let other_port = Url::parse("https://preview-waltr.example:444/token").expect("token URL");

        assert!(allows_address(
            &token,
            "172.16.3.53".parse().expect("private IP"),
            false,
            std::slice::from_ref(&trusted),
        ));
        assert!(allows_address(
            &token,
            "fd00::1".parse().expect("private IP"),
            false,
            std::slice::from_ref(&trusted),
        ));
        assert!(!allows_address(
            &other,
            "172.16.3.53".parse().expect("private IP"),
            false,
            std::slice::from_ref(&trusted),
        ));
        assert!(!allows_address(
            &other_port,
            "172.16.3.53".parse().expect("private IP"),
            false,
            std::slice::from_ref(&trusted),
        ));
        assert!(!allows_address(
            &token,
            "127.0.0.1".parse().expect("loopback IP"),
            false,
            std::slice::from_ref(&trusted),
        ));
        assert!(!allows_address(
            &token,
            "169.254.169.254".parse().expect("link-local IP"),
            false,
            std::slice::from_ref(&trusted),
        ));
    }
}
