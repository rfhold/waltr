use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, SystemTime},
};

use futures_util::StreamExt as _;
use oauth2::{AccessToken, RefreshToken};
use reqwest::{Client, Response, Url, header::CONTENT_TYPE};

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

/// Provider inference credential refresh coordinator.
#[derive(Debug, Clone)]
pub struct CredentialRefreshCoordinator {
    credential_store: CredentialStore,
    #[cfg(test)]
    test_root: Option<reqwest::Certificate>,
}

impl CredentialRefreshCoordinator {
    /// Create a coordinator over the supplied durable credential store.
    #[must_use]
    pub fn new(credential_store: impl Into<CredentialStore>) -> Self {
        Self {
            credential_store: credential_store.into(),
            #[cfg(test)]
            test_root: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_test_root(mut self, root: reqwest::Certificate) -> Self {
        self.test_root = Some(root);
        self
    }

    /// Refresh one provider credential and return the committed current credential.
    ///
    /// `Ok(None)` means normal runtime resolution found no usable OAuth credential.
    ///
    /// # Errors
    ///
    /// Returns a sanitized error for invalid configuration, failed transport, rejected
    /// exchanges, invalid responses, or storage failures.
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
        let refreshed = match self
            .refresh_provider_oauth(&target, definition, &record.credential)
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

    fn oauth_client_for(
        &self,
        target: &AuthTarget,
        value: &str,
    ) -> Result<(Url, Client), CredentialRefreshError> {
        let url = oauth_token_url(target, value)?;
        let builder =
            crate::outbound_http::client_builder().map_err(|_| oauth_client_error(target))?;
        #[cfg(test)]
        let builder = match &self.test_root {
            // The local TLS fixture has its own root independent of platform trust.
            Some(root) => Client::builder().tls_certs_only([root.clone()]),
            None => builder,
        };
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| oauth_client_error(target))?;
        Ok((url, client))
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
        AuthTarget::Host { .. } => "host",
    }
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

fn refresh_claim(revision: u64) -> OAuthRefreshClaim {
    OAuthRefreshClaim {
        revision,
        claim_id: uuid::Uuid::now_v7().to_string(),
        lease_expires_at_ms: now_ms().saturating_add(30_000),
    }
}

impl CredentialRefreshCoordinator {
    async fn refresh_provider_oauth(
        &self,
        target: &AuthTarget,
        definition: &SessionOAuthProviderSettings,
        credential: &OAuthCredential,
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
        let (token_url, client) = self.oauth_client_for(target, &definition.token_url)?;
        let builder = client.post(token_url);
        let response = match definition.token_request_style {
            SessionOAuthTokenRequestStyle::Json => {
                builder.json(&pairs_to_json(&pairs)).send().await
            }
            SessionOAuthTokenRequestStyle::FormUrlencoded => builder.form(&pairs).send().await,
        }
        .map_err(|error| {
            refresh_error(
                target.clone(),
                AuthenticationFailureReason::NetworkFailed,
                "provider OAuth token request failed",
                if error.is_connect() {
                    RefreshFailureDisposition::RetrySafe
                } else {
                    RefreshFailureDisposition::Indeterminate
                },
            )
        })?;
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
    credential.authority_id.clone_from(&current.authority_id);
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

fn oauth_token_url(target: &AuthTarget, value: &str) -> Result<Url, CredentialRefreshError> {
    let url = Url::parse(value).map_err(|_| refresh_configuration_error(target))?;
    if url.scheme() != "https"
        || !url.has_host()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(refresh_configuration_error(target));
    }
    Ok(url)
}

fn oauth_client_error(target: &AuthTarget) -> CredentialRefreshError {
    refresh_error(
        target.clone(),
        AuthenticationFailureReason::NetworkFailed,
        "failed to build OAuth token client",
        RefreshFailureDisposition::RetrySafe,
    )
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
        "OAuth token endpoint must be an HTTPS URL without userinfo or a fragment",
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

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn token_url_checks_shape_not_address_class() {
        let target = AuthTarget::provider("test");
        for url in [
            "https://172.16.3.53/token",
            "https://[fd00::1]/token",
            "https://localhost:8443/token?provider=test",
        ] {
            assert!(oauth_token_url(&target, url).is_ok());
        }
        for url in [
            "http://localhost/token",
            "https://user:pass@localhost/token",
            "https://localhost/token#fragment",
            "not a URL",
        ] {
            let error = oauth_token_url(&target, url).expect_err("invalid endpoint");
            assert_eq!(
                error.reason(),
                AuthenticationFailureReason::ConfigurationRequired
            );
        }
    }
}
