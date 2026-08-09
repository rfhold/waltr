#![allow(
    clippy::duration_suboptimal_units,
    clippy::ignored_unit_patterns,
    clippy::manual_let_else,
    clippy::match_same_arms,
    clippy::option_if_let_else,
    clippy::too_many_lines
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::{
    SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer as _},
    pkcs8::DecodePrivateKey as _,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;
use url::Url;

use super::{
    McpOAuthClock, McpOAuthEntropy, OAuthAuthorizationCode, OAuthAuthorizationDecision,
    OAuthAuthorizationStore, OAuthAuthorizationStoreError, OAuthAuthorizationStoreResult,
    OAuthAuthorizationTransaction, OAuthCachedClientMetadata, OAuthCleanupResult, OAuthClient,
    OAuthClientPromotionResult, OAuthClientRegistrationMethod, OAuthClientRegistrationOptions,
    OAuthDynamicClientPromotionRequest, OAuthDynamicClientRegistrationRequest,
    OAuthDynamicClientRegistrationResult, OAuthIssueGrantRequest, OAuthIssueGrantResult,
    OAuthKeyEncryption, OAuthRefreshRotationRequest, OAuthRefreshRotationResult,
    OAuthRegistrationOutcome, OAuthResumeAuthenticationRequest,
    OAuthResumeDynamicClientPromotionRequest, OAuthRevocationTarget, OAuthSigningKey,
    OAuthTokenMetadata, OAuthValidateAccessTokenRequest, duration_ms,
    generate_es256_signing_key_candidate,
};
use crate::{
    Error, McpAuthorizationServerMetadata, McpClientRegistrationError,
    McpClientRegistrationErrorCode, McpClientRegistrationResponse, McpJwtAccessTokenClaims,
    McpJwtAccessTokenHeader, McpJwtAudience, McpOAuthGrantType, McpOAuthResponseType,
    McpOAuthSecret, McpOAuthTokenType, McpPrincipalId, McpPublicClientRegistrationRequest,
    McpTokenEndpointAuthMethod, McpTokenError, McpTokenErrorCode, McpTokenResponse, Result,
    server::{BoxFuture, McpHostedTokenValidation, McpRevocationLease, McpTokenAuthorization},
};

/// One protected MCP resource and its supported OAuth scopes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthResource {
    /// Exact RFC 8707 resource identifier.
    pub resource: String,
    /// Supported scopes.
    pub scopes: Vec<String>,
}

/// Hosted authorization-server endpoint and lifetime policy.
#[derive(Clone, Debug)]
pub struct OAuthAuthorizationServerConfig {
    /// Exact issuer identifier.
    pub issuer: String,
    /// Exact authorization endpoint.
    pub authorization_endpoint: String,
    /// Exact authorization decision endpoint.
    pub authorization_decision_endpoint: String,
    /// Exact token endpoint.
    pub token_endpoint: String,
    /// Exact revocation endpoint.
    pub revocation_endpoint: String,
    /// Dynamic client registration endpoint, when enabled.
    pub registration_endpoint: Option<String>,
    /// Exact JWKS endpoint.
    pub jwks_uri: String,
    /// Supported resources.
    pub resources: Vec<OAuthResource>,
    /// Authorization transaction lifetime.
    pub authorization_transaction_lifetime: Duration,
    /// Authorization code lifetime.
    pub authorization_code_lifetime: Duration,
    /// Access-token lifetime.
    pub access_token_lifetime: Duration,
    /// Per-generation refresh-token lifetime.
    pub refresh_token_lifetime: Duration,
    /// Refresh-family absolute lifetime.
    pub refresh_family_lifetime: Duration,
    /// Permitted JWT clock skew.
    pub clock_skew: Duration,
    /// Retired-key verification overlap.
    pub signing_verification_overlap: Duration,
    /// Retired private-key rollback window.
    pub signing_private_key_overlap: Duration,
    /// Maximum interval between authoritative lease checks.
    pub revocation_poll_interval: Duration,
}

impl OAuthAuthorizationServerConfig {
    /// Create policy with secure default lifetimes and issuer-relative endpoints.
    #[must_use]
    pub fn new(issuer: impl Into<String>, resources: Vec<OAuthResource>) -> Self {
        let issuer = issuer.into();
        let base = issuer.trim_end_matches('/');
        Self {
            authorization_endpoint: format!("{base}/authorize"),
            authorization_decision_endpoint: format!("{base}/authorize/decision"),
            token_endpoint: format!("{base}/token"),
            revocation_endpoint: format!("{base}/revoke"),
            registration_endpoint: None,
            jwks_uri: format!("{base}/jwks.json"),
            issuer,
            resources,
            authorization_transaction_lifetime: Duration::from_secs(10 * 60),
            authorization_code_lifetime: Duration::from_secs(5 * 60),
            access_token_lifetime: Duration::from_secs(5 * 60),
            refresh_token_lifetime: Duration::from_secs(24 * 60 * 60),
            refresh_family_lifetime: Duration::from_secs(30 * 24 * 60 * 60),
            clock_skew: Duration::from_secs(30),
            signing_verification_overlap: Duration::from_secs(10 * 60),
            signing_private_key_overlap: Duration::from_secs(5 * 60),
            revocation_poll_interval: Duration::from_secs(5),
        }
    }
}

/// Request supplied to the integrating resource-owner authenticator.
#[derive(Clone, Debug)]
pub struct OAuthResourceOwnerAuthenticationRequest {
    /// Sanitized request headers.
    pub headers: HeaderMap,
    /// Opaque transaction identifier.
    pub transaction_id: String,
    /// Safe authorization-server continuation URL.
    pub continuation_url: String,
    /// Random value the host should bind to its secure correlation cookie.
    pub browser_correlation: McpOAuthSecret,
}

/// Resource-owner authentication result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthResourceOwnerAuthentication {
    /// Stable authenticated principal.
    Authenticated {
        /// Stable authenticated principal.
        principal_id: McpPrincipalId,
        /// Host-held browser correlation returned only when resuming login.
        browser_correlation: Option<McpOAuthSecret>,
    },
    /// Redirect to the integrating identity login flow.
    LoginRequired {
        /// Safe integrating-server login URL.
        login_url: String,
    },
    /// Authentication was denied.
    Denied,
    /// Authentication infrastructure is unavailable.
    Unavailable,
}

/// Object-safe resource-owner authentication adapter.
pub trait OAuthResourceOwnerAuthenticator: Send + Sync + 'static {
    /// Resolve an authorization request into a stable principal or login outcome.
    fn authenticate(
        &self,
        request: OAuthResourceOwnerAuthenticationRequest,
    ) -> BoxFuture<OAuthResourceOwnerAuthentication>;

    /// Build the correlation cookie set by the hosted server for an initial login redirect.
    ///
    /// Existing adapters need not implement this method. Implementations must return a
    /// transaction-specific cookie and must not persist or place the correlation in a URL.
    fn browser_correlation_cookie(
        &self,
        _transaction_id: &str,
        _browser_correlation: &McpOAuthSecret,
    ) -> Option<OAuthBrowserCorrelationCookie> {
        None
    }

    /// Build a cookie expiration directive after successful resumed authentication.
    fn clearing_browser_correlation_cookie(
        &self,
        _transaction_id: &str,
    ) -> Option<OAuthBrowserCorrelationCookie> {
        None
    }

    /// Best-effort finalization after hosted resumed authentication commits atomically.
    fn finalize_authentication(&self, _transaction_id: String) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// Validated browser-correlation cookie directive emitted by an authenticator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthBrowserCorrelationCookie {
    header: HeaderValue,
    max_age: Duration,
}

impl OAuthBrowserCorrelationCookie {
    /// Validate a complete `Set-Cookie` value for a transaction correlation cookie.
    ///
    /// # Errors
    ///
    /// Rejects values that are not host-only, HTTP-only, Lax, root-path cookies.
    pub fn new(value: &str, secure: bool) -> Result<Self> {
        if value.chars().any(char::is_control) {
            return Err(Error::protocol("invalid OAuth browser correlation cookie"));
        }
        let mut parts = value.split(';').map(str::trim);
        let Some((name, cookie_value)) = parts.next().and_then(|part| part.split_once('=')) else {
            return Err(Error::protocol("invalid OAuth browser correlation cookie"));
        };
        if !name.starts_with(if secure { "__Host-" } else { "mcp-oidc-" })
            || cookie_value.is_empty()
        {
            return Err(Error::protocol("invalid OAuth browser correlation cookie"));
        }
        let (mut http_only, mut cookie_secure, mut lax, mut root_path) =
            (false, false, false, false);
        let mut cookie_max_age = None;
        for part in parts {
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "httponly" if !http_only => http_only = true,
                "secure" if !cookie_secure => cookie_secure = true,
                "samesite=lax" if !lax => lax = true,
                "path=/" if !root_path => root_path = true,
                _ if lower.starts_with("max-age=") && cookie_max_age.is_none() => {
                    let Ok(seconds) = lower[8..].parse::<u64>() else {
                        return Err(Error::protocol("invalid OAuth browser correlation cookie"));
                    };
                    cookie_max_age = Some(Duration::from_secs(seconds));
                }
                _ => {
                    return Err(Error::protocol("invalid OAuth browser correlation cookie"));
                }
            }
        }
        if !http_only || !lax || !root_path || cookie_secure != secure {
            return Err(Error::protocol("invalid OAuth browser correlation cookie"));
        }
        let Some(max_age) = cookie_max_age else {
            return Err(Error::protocol("invalid OAuth browser correlation cookie"));
        };
        let header = HeaderValue::from_str(value)
            .map_err(|_| Error::protocol("invalid OAuth browser correlation cookie"))?;
        Ok(Self { header, max_age })
    }
}

/// Sanitized consent presentation model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthConsentModel {
    /// Client identifier.
    pub client_id: String,
    /// Untrusted client display name.
    pub client_display_name: Option<String>,
    /// Exact resource.
    pub resource: String,
    /// Canonical requested scopes.
    pub requested_scopes: Vec<String>,
    /// Exact previously granted scopes, empty when no exact consent exists.
    pub already_granted_scopes: Vec<String>,
    /// Opaque transaction identifier.
    pub transaction_id: String,
}

/// Consent presentation outcome.
pub enum OAuthConsentPresentation {
    /// Approve immediately (for policy-driven hosts).
    Approved,
    /// Return a host-rendered response.
    Response(Response),
}

impl std::fmt::Debug for OAuthConsentPresentation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Approved => formatter.write_str("Approved"),
            Self::Response(_) => formatter.write_str("Response(..)"),
        }
    }
}

/// Decision evidence supplied to the consent adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthConsentDecisionEvidence {
    /// Opaque transaction identifier.
    pub transaction_id: String,
    /// Opaque CSRF evidence.
    pub csrf: McpOAuthSecret,
    /// Request headers available to the integrating adapter.
    pub headers: HeaderMap,
}

/// Object-safe consent presentation and CSRF adapter.
pub trait OAuthConsentHandler: Send + Sync + 'static {
    /// Present consent or approve it through integrating policy.
    fn present(&self, model: OAuthConsentModel) -> BoxFuture<OAuthConsentPresentation>;
    /// Validate integrating-server CSRF evidence for a submitted decision.
    fn validate_decision(&self, evidence: OAuthConsentDecisionEvidence) -> BoxFuture<bool>;
}

/// Hosted generic OAuth authorization server.
#[derive(Clone)]
pub struct OAuthAuthorizationServer {
    state: Arc<AuthorizationState>,
}

/// Hosted revocation propagation health and its fail-closed polling bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OAuthRevocationHealth {
    /// Maximum configured interval between authoritative checks.
    pub authoritative_poll_interval: Duration,
    /// Active same-process notification subscribers.
    pub local_lease_count: usize,
}

impl std::fmt::Debug for OAuthAuthorizationServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthAuthorizationServer")
            .field("config", &self.state.config)
            .finish_non_exhaustive()
    }
}

struct AuthorizationState {
    config: OAuthAuthorizationServerConfig,
    store: Arc<dyn OAuthAuthorizationStore>,
    authenticator: Arc<dyn OAuthResourceOwnerAuthenticator>,
    consent: Arc<dyn OAuthConsentHandler>,
    clock: Arc<dyn McpOAuthClock>,
    entropy: Arc<dyn McpOAuthEntropy>,
    encryption: Arc<dyn OAuthKeyEncryption>,
    revocations: watch::Sender<u64>,
    registration: Option<OAuthClientRegistrationOptions>,
}

impl OAuthAuthorizationServer {
    /// Validate configuration and construct a hosted server.
    ///
    /// # Errors
    ///
    /// Returns an error when an endpoint, resource, or lifetime invariant is invalid.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: OAuthAuthorizationServerConfig,
        store: Arc<dyn OAuthAuthorizationStore>,
        authenticator: Arc<dyn OAuthResourceOwnerAuthenticator>,
        consent: Arc<dyn OAuthConsentHandler>,
        clock: Arc<dyn McpOAuthClock>,
        entropy: Arc<dyn McpOAuthEntropy>,
        encryption: Arc<dyn OAuthKeyEncryption>,
    ) -> Result<Self> {
        validate_config(&config)?;
        let (revocations, _) = watch::channel(0);
        Ok(Self {
            state: Arc::new(AuthorizationState {
                config,
                store,
                authenticator,
                consent,
                clock,
                entropy,
                encryption,
                revocations,
                registration: None,
            }),
        })
    }

    /// Install generic CIMD and/or dynamic registration policy.
    ///
    /// # Errors
    ///
    /// Returns an error when enabled DCR has no configured endpoint or a bound is invalid.
    pub fn with_client_registration(
        mut self,
        options: OAuthClientRegistrationOptions,
    ) -> Result<Self> {
        if options.dynamic_registration && self.state.config.registration_endpoint.is_none() {
            return Err(Error::protocol(
                "OAuth dynamic registration requires registration_endpoint",
            ));
        }
        if options.max_request_bytes == 0
            || options.max_field_bytes == 0
            || options.max_provisional_clients == 0
            || options.max_active_dynamic_clients == 0
            || options.max_clients_per_principal == 0
            || options.global_rate_limit == 0
            || options.source_rate_limit == 0
            || options.provisional_lifetime.is_zero()
            || options.inactive_client_lifetime.is_zero()
            || options.rate_limit_window.is_zero()
        {
            return Err(Error::protocol("OAuth registration policy is invalid"));
        }
        duration_ms(options.provisional_lifetime)?;
        duration_ms(options.inactive_client_lifetime)?;
        duration_ms(options.rate_limit_window)?;
        Arc::get_mut(&mut self.state)
            .ok_or_else(|| Error::protocol("OAuth registration must be configured before cloning"))?
            .registration = Some(options);
        Ok(self)
    }

    /// Validate and atomically persist a preregistered public client.
    ///
    /// # Errors
    ///
    /// Returns an error for confidential, malformed, duplicate, or over-quota clients.
    pub async fn preregister_public_client(
        &self,
        client: OAuthClient,
        allow_loopback_redirects: bool,
    ) -> Result<()> {
        if client.registration_method != OAuthClientRegistrationMethod::Preconfigured
            || client.client_id.is_empty()
            || client.client_id.len() > 2048
            || client.display_name.as_ref().is_some_and(|name| {
                name.is_empty() || name.len() > 256 || name.chars().any(char::is_control)
            })
            || client.client_secret_digest.is_some()
            || client.redirect_uris.is_empty()
            || client.redirect_uris.len() > self.state.store.config().max_redirect_uris_per_client
            || client.redirect_uris.iter().any(|redirect| {
                crate::validate_public_redirect_uri(redirect, allow_loopback_redirects).is_err()
            })
        {
            return Err(Error::protocol(
                "OAuth preregistered public client is invalid",
            ));
        }
        if !self.state.store.register_client(client).await? {
            return Err(Error::protocol(
                "OAuth preregistered client was not admitted",
            ));
        }
        Ok(())
    }

    /// Compose hosted authorization-server and configured registration routes.
    pub fn router(&self) -> Router {
        let config = &self.state.config;
        let mut router = Router::new()
            .route(&metadata_path(&config.issuer), get(metadata))
            .route(&endpoint_path(&config.jwks_uri), get(jwks))
            .route(
                &endpoint_path(&config.authorization_endpoint),
                get(authorize),
            )
            .route(
                &endpoint_path(&config.authorization_decision_endpoint),
                post(decide),
            )
            .route(&endpoint_path(&config.token_endpoint), post(token))
            .route(&endpoint_path(&config.revocation_endpoint), post(revoke));
        if self
            .state
            .registration
            .as_ref()
            .is_some_and(|options| options.dynamic_registration)
            && let Some(endpoint) = &config.registration_endpoint
        {
            router = router.route(&endpoint_path(endpoint), post(register));
        }
        router.with_state(self.state.clone())
    }

    /// Validate a hosted bearer token and create an active revocation lease.
    #[must_use]
    pub fn authorize_token(
        &self,
        token: crate::McpAccessToken,
        context: crate::server::McpAuthorizationContext,
    ) -> BoxFuture<McpHostedTokenValidation> {
        let state = self.state.clone();
        Box::pin(async move { validate_hosted_token(state, token.0, context).await })
    }

    /// Idempotently revoke an access token, refresh family, grant, or client and notify local leases after commit.
    ///
    /// # Errors
    ///
    /// Returns an error when the store is unavailable or the operation time is invalid.
    pub async fn revoke_target(
        &self,
        target: OAuthRevocationTarget,
    ) -> OAuthAuthorizationStoreResult<()> {
        self.state
            .store
            .revoke(target, now_ms(self.state.clock.now())?)
            .await?;
        self.state
            .revocations
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(())
    }

    /// Run bounded expired-state cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error when the store is unavailable or the operation time is invalid.
    pub async fn cleanup(&self, limit: usize) -> OAuthAuthorizationStoreResult<OAuthCleanupResult> {
        self.state
            .store
            .cleanup(now_ms(self.state.clock.now())?, limit)
            .await
    }

    /// Generate and persist a signing-key candidate, bootstrapping an empty issuer.
    ///
    /// # Errors
    ///
    /// Returns an error when entropy, wrapping, or persistence fails.
    pub async fn generate_signing_key_candidate(
        &self,
        bootstrap: bool,
    ) -> OAuthAuthorizationStoreResult<OAuthSigningKey> {
        let now = now_ms(self.state.clock.now())?;
        let kid = random_secret(self.state.entropy.as_ref(), 18)?;
        let candidate = generate_es256_signing_key_candidate(
            &self.state.config.issuer,
            kid,
            now,
            self.state.encryption.as_ref(),
            self.state.entropy.as_ref(),
        )?;
        self.state
            .store
            .put_signing_key_candidate(candidate, bootstrap)
            .await
    }

    /// Activate a pending signer while retaining the prior key for verification overlap.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation time is invalid or persistence fails.
    pub async fn activate_signing_key(
        &self,
        key_id: impl Into<String>,
    ) -> OAuthAuthorizationStoreResult<bool> {
        let now = now_ms(self.state.clock.now())?;
        self.state
            .store
            .activate_signing_key(
                self.state.config.issuer.clone(),
                key_id.into(),
                now,
                add_ms(now, self.state.config.signing_verification_overlap)?,
                add_ms(now, self.state.config.signing_private_key_overlap)?,
            )
            .await
    }

    /// Validate that the active key decrypts as PKCS#8 and exactly matches its public JWK.
    ///
    /// # Errors
    ///
    /// Returns an error when no active signer is available or key material is inconsistent.
    pub async fn validate_signing_key_readiness(&self) -> OAuthAuthorizationStoreResult<()> {
        let key = self
            .state
            .store
            .active_signing_key(self.state.config.issuer.clone())
            .await?
            .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
        super::validate_es256_signing_key_readiness(&key, self.state.encryption.as_ref())
    }

    /// Re-encrypt one active or retained signing key with the encryption provider's current key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is unknown, cannot be decrypted, or cannot be persisted.
    pub async fn rewrap_signing_key(
        &self,
        key_id: impl Into<String>,
    ) -> OAuthAuthorizationStoreResult<bool> {
        let key_id = key_id.into();
        let now = now_ms(self.state.clock.now())?;
        let key = self
            .state
            .store
            .verification_signing_keys(self.state.config.issuer.clone(), now)
            .await?
            .into_iter()
            .find(|key| key.key_id == key_id)
            .ok_or(OAuthAuthorizationStoreError::InvalidData)?;
        let encrypted = key
            .encrypted_private_key
            .as_ref()
            .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
        super::validate_es256_signing_key_readiness(&key, self.state.encryption.as_ref())?;
        let plaintext = self
            .state
            .encryption
            .decrypt("ES256", &key.key_id, encrypted)?;
        let rewrapped = self.state.encryption.encrypt(
            "ES256",
            &key.key_id,
            &plaintext,
            self.state.entropy.as_ref(),
        )?;
        self.state
            .store
            .rewrap_signing_key(key.issuer, key.key_id, rewrapped)
            .await
    }

    /// Report local lease subscribers and the cross-process authoritative polling bound.
    #[must_use]
    pub fn revocation_health(&self) -> OAuthRevocationHealth {
        OAuthRevocationHealth {
            authoritative_poll_interval: self.state.config.revocation_poll_interval,
            local_lease_count: self.state.revocations.receiver_count(),
        }
    }
}

async fn metadata(State(state): State<Arc<AuthorizationState>>) -> Response {
    let scopes = state
        .config
        .resources
        .iter()
        .flat_map(|resource| resource.scopes.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let metadata = McpAuthorizationServerMetadata {
        issuer: state.config.issuer.clone(),
        authorization_endpoint: Some(state.config.authorization_endpoint.clone()),
        token_endpoint: Some(state.config.token_endpoint.clone()),
        revocation_endpoint: Some(state.config.revocation_endpoint.clone()),
        jwks_uri: Some(state.config.jwks_uri.clone()),
        registration_endpoint: state.registration.as_ref().and_then(|options| {
            options
                .dynamic_registration
                .then(|| state.config.registration_endpoint.clone())
                .flatten()
        }),
        scopes_supported: scopes,
        response_types_supported: vec!["code".to_owned()],
        grant_types_supported: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
        token_endpoint_auth_methods_supported: vec!["none".to_owned()],
        code_challenge_methods_supported: vec!["S256".to_owned()],
        client_id_metadata_document_supported: state
            .registration
            .as_ref()
            .and_then(|options| options.metadata_fetcher.as_ref().map(|_| true)),
        protected_resources: state
            .config
            .resources
            .iter()
            .map(|value| value.resource.clone())
            .collect(),
        access_token_signing_alg_values_supported: vec!["ES256".to_owned()],
        authorization_response_iss_parameter_supported: Some(true),
        extra: BTreeMap::new(),
    };
    json_response(StatusCode::OK, &metadata, false)
}

async fn jwks(State(state): State<Arc<AuthorizationState>>) -> Response {
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    match state
        .store
        .verification_signing_keys(state.config.issuer.clone(), now)
        .await
    {
        Ok(keys) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "keys": keys.into_iter().map(|key| key.public_jwk).collect::<Vec<_>>() }),
            false,
        ),
        Err(_) => unavailable(),
    }
}

async fn resolve_client(
    state: &Arc<AuthorizationState>,
    client_id: &str,
    now: i64,
) -> OAuthAuthorizationStoreResult<Option<(OAuthClient, Option<String>)>> {
    if let Some(client) = state.store.client(client_id.to_owned(), now).await? {
        return Ok(Some((client, None)));
    }
    let Some(options) = &state.registration else {
        return Ok(None);
    };
    let Some(fetcher) = &options.metadata_fetcher else {
        return Ok(None);
    };
    let metadata = if let Some(metadata) = state
        .store
        .client_metadata(client_id.to_owned(), now)
        .await?
    {
        metadata
    } else {
        let Ok(retrieved) = fetcher.fetch(client_id.to_owned()).await else {
            return Ok(None);
        };
        let snapshot_id = URL_SAFE_NO_PAD.encode(&retrieved.document_digest);
        let expires_at_ms = add_ms(now, retrieved.freshness)?;
        let retention = state
            .config
            .authorization_transaction_lifetime
            .checked_add(state.config.authorization_code_lifetime)
            .ok_or(OAuthAuthorizationStoreError::InvalidData)?;
        let retain_until_ms = add_ms(expires_at_ms, retention)?;
        let metadata = OAuthCachedClientMetadata {
            snapshot_id,
            client_id: client_id.to_owned(),
            document: super::super::discovery::parse_unique_json(&retrieved.normalized_document)
                .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
            document_digest: retrieved.document_digest,
            retrieved_at_ms: now,
            expires_at_ms,
            retain_until_ms,
        };
        state.store.cache_client_metadata(metadata.clone()).await?;
        metadata
    };
    let document: crate::McpClientMetadataDocument =
        serde_json::from_value(metadata.document.clone())
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    document
        .validate_public_client(client_id, options.allow_loopback_redirects)
        .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    Ok(Some((
        OAuthClient {
            client_id: client_id.to_owned(),
            registration_method: OAuthClientRegistrationMethod::ClientMetadataDocument,
            display_name: document.client_name,
            client_secret_digest: None,
            redirect_uris: document.redirect_uris,
            created_at_ms: metadata.retrieved_at_ms,
            expires_at_ms: None,
            disabled_at_ms: None,
        },
        Some(metadata.snapshot_id),
    )))
}

async fn register(
    State(state): State<Arc<AuthorizationState>>,
    request: Request<Body>,
) -> Response {
    let Some(options) = state
        .registration
        .as_ref()
        .filter(|options| options.dynamic_registration)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_none_or(|value| !value.trim().eq_ignore_ascii_case("application/json"))
    {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidClientMetadata);
    }
    let trusted_source = options
        .source_resolver
        .as_ref()
        .and_then(|resolver| resolver.trusted_source(request.headers()));
    if trusted_source
        .as_ref()
        .is_some_and(|source| source.is_empty() || source.len() > options.max_field_bytes)
    {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidClientMetadata);
    }
    let Ok(body) = to_bytes(request.into_body(), options.max_request_bytes).await else {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidClientMetadata);
    };
    let Ok(raw) = super::super::discovery::parse_unique_json::<serde_json::Value>(&body) else {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidClientMetadata);
    };
    if raw.get("software_statement").is_some() {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidSoftwareStatement);
    }
    if raw.get("client_secret").is_some()
        || raw.get("client_secret_expires_at").is_some()
        || raw.get("registration_access_token").is_some()
        || raw.get("registration_client_uri").is_some()
        || raw.get("client_id").is_some()
    {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidClientMetadata);
    }
    let Ok(registration) = serde_json::from_value::<McpPublicClientRegistrationRequest>(raw) else {
        observe_registration(options, OAuthRegistrationOutcome::Rejected);
        return registration_error(McpClientRegistrationErrorCode::InvalidClientMetadata);
    };
    let registration = match registration.validate_and_normalize(
        options.allow_loopback_redirects,
        state.store.config().max_redirect_uris_per_client,
        options.max_field_bytes,
    ) {
        Ok(registration) => registration,
        Err(error) => {
            observe_registration(options, OAuthRegistrationOutcome::Rejected);
            return json_response(StatusCode::BAD_REQUEST, &error, true);
        }
    };
    let now = match now_ms(state.clock.now()) {
        Ok(now) => now,
        Err(_) => return unavailable(),
    };
    for _ in 0..4 {
        let client_id = match random_secret(state.entropy.as_ref(), 24) {
            Ok(client_id) => client_id,
            Err(_) => return unavailable(),
        };
        let admission = state
            .store
            .register_dynamic_client(OAuthDynamicClientRegistrationRequest {
                client: OAuthClient {
                    client_id: client_id.clone(),
                    registration_method: OAuthClientRegistrationMethod::Dynamic,
                    display_name: registration.client_name.clone(),
                    client_secret_digest: None,
                    redirect_uris: registration.redirect_uris.clone(),
                    created_at_ms: now,
                    expires_at_ms: None,
                    disabled_at_ms: None,
                },
                provisional_expires_at_ms: match add_ms(now, options.provisional_lifetime) {
                    Ok(value) => value,
                    Err(_) => return unavailable(),
                },
                inactivity_lifetime_ms: match duration_ms(options.inactive_client_lifetime) {
                    Ok(value) => value,
                    Err(_) => return unavailable(),
                },
                max_provisional_clients: options.max_provisional_clients,
                max_active_dynamic_clients: options.max_active_dynamic_clients,
                global_rate_limit: options.global_rate_limit,
                source_rate_limit: options.source_rate_limit,
                rate_limit_window_ms: match duration_ms(options.rate_limit_window) {
                    Ok(value) => value,
                    Err(_) => return unavailable(),
                },
                trusted_source: trusted_source.clone(),
            })
            .await;
        match admission {
            Ok(OAuthDynamicClientRegistrationResult::Registered) => {
                observe_registration(options, OAuthRegistrationOutcome::Accepted);
                return json_response(
                    StatusCode::CREATED,
                    &McpClientRegistrationResponse {
                        client_id,
                        client_secret: None,
                        client_secret_expires_at: None,
                        registration_access_token: None,
                        registration_client_uri: None,
                        client_id_issued_at: u64::try_from(now / 1000).ok(),
                        application_type: registration.application_type,
                        redirect_uris: registration.redirect_uris,
                        grant_types: vec![
                            McpOAuthGrantType::AuthorizationCode,
                            McpOAuthGrantType::RefreshToken,
                        ],
                        response_types: vec![McpOAuthResponseType::Code],
                        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
                        client_name: registration.client_name,
                    },
                    true,
                );
            }
            Ok(OAuthDynamicClientRegistrationResult::Duplicate) => {}
            Ok(OAuthDynamicClientRegistrationResult::RateLimited) => {
                observe_registration(options, OAuthRegistrationOutcome::Limited);
                return json_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    &McpClientRegistrationError {
                        error: McpClientRegistrationErrorCode::InvalidClientMetadata,
                        error_description: None,
                    },
                    true,
                );
            }
            Ok(
                OAuthDynamicClientRegistrationResult::CapacityExceeded
                | OAuthDynamicClientRegistrationResult::Disabled,
            )
            | Err(_) => {
                observe_registration(options, OAuthRegistrationOutcome::CapacityBlocked);
                return unavailable();
            }
        }
    }
    observe_registration(options, OAuthRegistrationOutcome::CapacityBlocked);
    unavailable()
}

fn registration_error(error: McpClientRegistrationErrorCode) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        &McpClientRegistrationError {
            error,
            error_description: None,
        },
        true,
    )
}

fn observe_registration(
    options: &OAuthClientRegistrationOptions,
    outcome: OAuthRegistrationOutcome,
) {
    if let Some(observer) = &options.observer {
        observer.dynamic_registration(outcome);
    }
}

async fn authorize(
    State(state): State<Arc<AuthorizationState>>,
    request: Request<Body>,
) -> Response {
    let headers = request.headers().clone();
    let query = request.uri().query().unwrap_or_default();
    let parameters = match strict_parameters(query.as_bytes()) {
        Ok(value) => value,
        Err(_) => return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    if parameters.contains_key("transaction_id") {
        return resume_authorize(state, headers, parameters).await;
    }
    let parsed = match parse_authorization(parameters) {
        Ok(value) => value,
        Err(error) => return local_authorization_error(StatusCode::BAD_REQUEST, error),
    };
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let Some((client, client_metadata_snapshot_id)) =
        (match resolve_client(&state, &parsed.client_id, now).await {
            Ok(value) => value,
            Err(_) => return unavailable(),
        })
    else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "unauthorized_client");
    };
    if client.client_secret_digest.is_some() || !client.redirect_uris.contains(&parsed.redirect_uri)
    {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    if let Err(error) = validate_authorization_bindings(&state.config, &parsed) {
        return authorization_redirect(
            &parsed.redirect_uri,
            &parsed.state,
            error,
            None,
            &state.config.issuer,
        );
    }
    let transaction_id = match random_secret(state.entropy.as_ref(), 24) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let correlation = match random_secret(state.entropy.as_ref(), 32) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let transaction = OAuthAuthorizationTransaction {
        transaction_id: transaction_id.clone(),
        client_id: parsed.client_id,
        client_metadata_snapshot_id,
        redirect_uri: parsed.redirect_uri,
        resource: parsed.resource,
        scopes: parsed.scopes,
        pkce_challenge: parsed.code_challenge,
        browser_correlation_digest: digest(&correlation),
        state_reference: parsed.state,
        principal_id: None,
        decision: None,
        created_at_ms: now,
        expires_at_ms: match add_ms(now, state.config.authorization_transaction_lifetime) {
            Ok(value) => value,
            Err(_) => return unavailable(),
        },
    };
    match state
        .store
        .create_authorization_transaction(transaction)
        .await
    {
        Ok(true) => {}
        Ok(false) => return unavailable(),
        Err(_) => return unavailable(),
    }
    authenticate_transaction(state, headers, transaction_id, correlation, false).await
}

async fn resume_authorize(
    state: Arc<AuthorizationState>,
    headers: HeaderMap,
    mut parameters: BTreeMap<String, String>,
) -> Response {
    if parameters.len() != 1 {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Some(transaction_id) = parameters
        .remove("transaction_id")
        .filter(|value| !value.is_empty() && value.len() <= 128)
    else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    authenticate_transaction(state, headers, transaction_id, String::new(), true).await
}

async fn authenticate_transaction(
    state: Arc<AuthorizationState>,
    headers: HeaderMap,
    transaction_id: String,
    correlation: String,
    resumed: bool,
) -> Response {
    let continuation_url = format!(
        "{}?transaction_id={}",
        state.config.authorization_endpoint,
        encode(&transaction_id)
    );
    let secret = McpOAuthSecret::new(correlation);
    let cookie = (!resumed)
        .then(|| {
            state
                .authenticator
                .browser_correlation_cookie(&transaction_id, &secret)
        })
        .flatten();
    let outcome = state
        .authenticator
        .authenticate(OAuthResourceOwnerAuthenticationRequest {
            headers,
            transaction_id: transaction_id.clone(),
            continuation_url,
            browser_correlation: secret,
        })
        .await;
    match outcome {
        OAuthResourceOwnerAuthentication::Authenticated {
            principal_id,
            browser_correlation,
        } => {
            let now = match now_ms(state.clock.now()) {
                Ok(value) => value,
                Err(_) => return unavailable(),
            };
            let authentication = if resumed {
                let Some(correlation) = browser_correlation else {
                    return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
                };
                let correlation_digest = digest(correlation.expose_secret());
                if let Some(options) = &state.registration {
                    state
                        .store
                        .resume_and_promote_client(OAuthResumeDynamicClientPromotionRequest {
                            promotion: OAuthDynamicClientPromotionRequest {
                                transaction_id: transaction_id.clone(),
                                principal_id,
                                now_ms: now,
                                max_clients_per_principal: options.max_clients_per_principal,
                                max_active_dynamic_clients: options.max_active_dynamic_clients,
                            },
                            browser_correlation_digest: correlation_digest,
                        })
                        .await
                        .map(|result| result == OAuthClientPromotionResult::Authenticated)
                } else {
                    state
                        .store
                        .resume_authorization_transaction(OAuthResumeAuthenticationRequest {
                            transaction_id: transaction_id.clone(),
                            principal_id,
                            browser_correlation_digest: correlation_digest,
                            now_ms: now,
                        })
                        .await
                }
            } else if let Some(options) = &state.registration {
                state
                    .store
                    .authenticate_and_promote_client(OAuthDynamicClientPromotionRequest {
                        transaction_id: transaction_id.clone(),
                        principal_id,
                        now_ms: now,
                        max_clients_per_principal: options.max_clients_per_principal,
                        max_active_dynamic_clients: options.max_active_dynamic_clients,
                    })
                    .await
                    .map(|result| result == OAuthClientPromotionResult::Authenticated)
            } else {
                state
                    .store
                    .authenticate_authorization_transaction(
                        transaction_id.clone(),
                        principal_id,
                        now,
                    )
                    .await
            };
            match authentication {
                Ok(true) if resumed => {
                    state
                        .authenticator
                        .finalize_authentication(transaction_id.clone())
                        .await;
                    let clear = state
                        .authenticator
                        .clearing_browser_correlation_cookie(&transaction_id);
                    let mut response = consent_or_issue(state, transaction_id).await;
                    if let Some(cookie) = clear {
                        response
                            .headers_mut()
                            .append(header::SET_COOKIE, cookie.header);
                    }
                    response
                }
                Ok(true) => consent_or_issue(state, transaction_id).await,
                Ok(false) if resumed => {
                    local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request")
                }
                Ok(false) => {
                    local_authorization_error(StatusCode::BAD_REQUEST, "unauthorized_client")
                }
                Err(_) => unavailable(),
            }
        }
        OAuthResourceOwnerAuthentication::LoginRequired { .. } if resumed => {
            local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request")
        }
        OAuthResourceOwnerAuthentication::LoginRequired { login_url } => {
            if safe_host_redirect(&state.config.issuer, &login_url) {
                let mut response = redirect_response(&login_url);
                if let Some(cookie) = cookie {
                    if cookie.max_age > state.config.authorization_transaction_lifetime {
                        return unavailable();
                    }
                    response
                        .headers_mut()
                        .append(header::SET_COOKIE, cookie.header);
                }
                response
            } else {
                unavailable()
            }
        }
        OAuthResourceOwnerAuthentication::Denied => {
            local_authorization_error(StatusCode::FORBIDDEN, "access_denied")
        }
        OAuthResourceOwnerAuthentication::Unavailable => unavailable(),
    }
}

async fn consent_or_issue(state: Arc<AuthorizationState>, transaction_id: String) -> Response {
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let Some(context) = (match state
        .store
        .authorization_context(transaction_id.clone(), now)
        .await
    {
        Ok(value) => value,
        Err(_) => return unavailable(),
    }) else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if context.consent_granted {
        if !matches!(
            state
                .store
                .decide_authorization_transaction(
                    transaction_id.clone(),
                    OAuthAuthorizationDecision::Approved,
                    now
                )
                .await,
            Ok(true)
        ) {
            return unavailable();
        }
        return issue_code_redirect(state, context.transaction).await;
    }
    let model = OAuthConsentModel {
        client_id: context.client.client_id,
        client_display_name: context.client.display_name,
        resource: context.transaction.resource.clone(),
        requested_scopes: context.transaction.scopes.clone(),
        already_granted_scopes: Vec::new(),
        transaction_id: transaction_id.clone(),
    };
    match state.consent.present(model).await {
        OAuthConsentPresentation::Approved => {
            if !matches!(
                state
                    .store
                    .decide_authorization_transaction(
                        transaction_id,
                        OAuthAuthorizationDecision::Approved,
                        now
                    )
                    .await,
                Ok(true)
            ) {
                return unavailable();
            }
            if !matches!(
                state
                    .store
                    .remember_authorization_consent(context.transaction.transaction_id.clone(), now)
                    .await,
                Ok(true)
            ) {
                return unavailable();
            }
            issue_code_redirect(state, context.transaction).await
        }
        OAuthConsentPresentation::Response(mut response) => {
            response.headers_mut().insert(
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            );
            cache_control_no_store(response.headers_mut());
            response
        }
    }
}

async fn decide(State(state): State<Arc<AuthorizationState>>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    let parameters = match strict_form(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if parameters
        .keys()
        .any(|key| !["transaction_id", "decision", "csrf"].contains(&key.as_str()))
    {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Some(transaction_id) = parameters
        .get("transaction_id")
        .filter(|value| !value.is_empty())
        .cloned()
    else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Some(csrf) = parameters
        .get("csrf")
        .filter(|value| !value.is_empty())
        .cloned()
    else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !state
        .consent
        .validate_decision(OAuthConsentDecisionEvidence {
            transaction_id: transaction_id.clone(),
            csrf: McpOAuthSecret::new(csrf),
            headers,
        })
        .await
    {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let decision = match parameters.get("decision").map(String::as_str) {
        Some("approve") => OAuthAuthorizationDecision::Approved,
        Some("deny") => OAuthAuthorizationDecision::Denied,
        _ => return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request"),
    };
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let Some(context) = (match state
        .store
        .authorization_context(transaction_id.clone(), now)
        .await
    {
        Ok(value) => value,
        Err(_) => return unavailable(),
    }) else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !matches!(
        state
            .store
            .decide_authorization_transaction(transaction_id.clone(), decision, now)
            .await,
        Ok(true)
    ) {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    if decision == OAuthAuthorizationDecision::Denied {
        return authorization_redirect(
            &context.transaction.redirect_uri,
            &context.transaction.state_reference,
            "access_denied",
            None,
            &state.config.issuer,
        );
    }
    if !matches!(
        state
            .store
            .remember_authorization_consent(transaction_id, now)
            .await,
        Ok(true)
    ) {
        return unavailable();
    }
    issue_code_redirect(state, context.transaction).await
}

async fn issue_code_redirect(
    state: Arc<AuthorizationState>,
    transaction: OAuthAuthorizationTransaction,
) -> Response {
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let code = match random_secret(state.entropy.as_ref(), 32) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let record = OAuthAuthorizationCode {
        code_digest: digest(&code),
        transaction_id: transaction.transaction_id,
        created_at_ms: now,
        expires_at_ms: match add_ms(now, state.config.authorization_code_lifetime) {
            Ok(value) => value.min(transaction.expires_at_ms),
            Err(_) => return unavailable(),
        },
        consumed_at_ms: None,
    };
    if !matches!(state.store.issue_authorization_code(record).await, Ok(true)) {
        return unavailable();
    }
    let mut url = match Url::parse(&transaction.redirect_uri) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    url.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &transaction.state_reference)
        .append_pair("iss", &state.config.issuer);
    redirect_response(url.as_str())
}

async fn token(State(state): State<Arc<AuthorizationState>>, request: Request<Body>) -> Response {
    let parameters = match strict_form(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    match parameters.get("grant_type").map(String::as_str) {
        Some("authorization_code") => exchange_code(state, parameters).await,
        Some("refresh_token") => refresh(state, parameters).await,
        _ => token_error(McpTokenErrorCode::UnsupportedGrantType),
    }
}

async fn exchange_code(
    state: Arc<AuthorizationState>,
    mut p: BTreeMap<String, String>,
) -> Response {
    if p.keys().any(|key| {
        ![
            "grant_type",
            "code",
            "redirect_uri",
            "code_verifier",
            "client_id",
            "resource",
        ]
        .contains(&key.as_str())
    }) {
        return token_error(McpTokenErrorCode::InvalidRequest);
    }
    let Some(code) = take_required(&mut p, "code") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    let Some(redirect_uri) = take_required(&mut p, "redirect_uri") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    let Some(verifier) = take_required(&mut p, "code_verifier") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    let Some(client_id) = take_required(&mut p, "client_id") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    let Some(resource) = take_required(&mut p, "resource") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    if code.len() != 43
        || redirect_uri.len() > 2048
        || client_id.len() > 2048
        || resource.len() > 2048
        || !valid_verifier(&verifier)
    {
        return token_error(McpTokenErrorCode::InvalidGrant);
    }
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let code_digest = digest(&code);
    let Some(context) = (match state
        .store
        .authorization_code_context(code_digest.clone(), now)
        .await
    {
        Ok(value) => value,
        Err(_) => return unavailable(),
    }) else {
        return token_error(McpTokenErrorCode::InvalidGrant);
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let grant_id = match random_secret(state.entropy.as_ref(), 18) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let family_id = match random_secret(state.entropy.as_ref(), 18) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let refresh_token = match random_secret(state.entropy.as_ref(), 32) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let prepared = match prepare_access_token(&state, &context.transaction, &grant_id, now).await {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let request = OAuthIssueGrantRequest {
        code_digest,
        client_id,
        redirect_uri,
        resource,
        pkce_challenge: challenge,
        grant_id,
        refresh_family_id: family_id,
        refresh_token_digest: digest(&refresh_token),
        refresh_token_expires_at_ms: match add_ms(now, state.config.refresh_token_lifetime) {
            Ok(value) => value,
            Err(_) => return unavailable(),
        },
        refresh_family_expires_at_ms: match add_ms(now, state.config.refresh_family_lifetime) {
            Ok(value) => value,
            Err(_) => return unavailable(),
        },
        access_token: prepared.metadata.clone(),
        now_ms: now,
    };
    match state.store.redeem_authorization_code(request).await {
        Ok(OAuthIssueGrantResult::Issued(grant)) => token_success(
            prepared.jwt,
            refresh_token,
            &grant.scopes,
            state.config.access_token_lifetime,
        ),
        Ok(OAuthIssueGrantResult::InvalidGrant) => token_error(McpTokenErrorCode::InvalidGrant),
        Err(_) => unavailable(),
    }
}

async fn refresh(state: Arc<AuthorizationState>, mut p: BTreeMap<String, String>) -> Response {
    if p.keys().any(|key| {
        ![
            "grant_type",
            "refresh_token",
            "client_id",
            "scope",
            "resource",
        ]
        .contains(&key.as_str())
    }) {
        return token_error(McpTokenErrorCode::InvalidRequest);
    }
    let Some(refresh_token) = take_required(&mut p, "refresh_token") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    let Some(client_id) = take_required(&mut p, "client_id") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    let Some(resource) = take_required(&mut p, "resource") else {
        return token_error(McpTokenErrorCode::InvalidRequest);
    };
    if refresh_token.len() != 43 || client_id.len() > 2048 || resource.len() > 2048 {
        return token_error(McpTokenErrorCode::InvalidGrant);
    }
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let refresh_digest = digest(&refresh_token);
    let Some(context) = (match state
        .store
        .refresh_token_context(refresh_digest.clone(), now)
        .await
    {
        Ok(value) => value,
        Err(_) => return unavailable(),
    }) else {
        return token_error(McpTokenErrorCode::InvalidGrant);
    };
    let scopes = match p.remove("scope") {
        Some(value) => match canonical_scopes(&value) {
            Ok(value) => value,
            Err(_) => return token_error(McpTokenErrorCode::InvalidScope),
        },
        None => context.grant.scopes.clone(),
    };
    if context.grant.client_id != client_id
        || context.grant.resource != resource
        || !scopes
            .iter()
            .all(|scope| context.grant.scopes.contains(scope))
    {
        return token_error(McpTokenErrorCode::InvalidGrant);
    }
    let replacement = match random_secret(state.entropy.as_ref(), 32) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let transaction = OAuthAuthorizationTransaction {
        transaction_id: String::new(),
        client_id: context.grant.client_id.clone(),
        client_metadata_snapshot_id: None,
        redirect_uri: String::new(),
        resource: context.grant.resource.clone(),
        scopes: scopes.clone(),
        pkce_challenge: String::new(),
        browser_correlation_digest: Vec::new(),
        state_reference: String::new(),
        principal_id: Some(context.grant.principal_id.clone()),
        decision: Some(OAuthAuthorizationDecision::Approved),
        created_at_ms: now,
        expires_at_ms: now.saturating_add(1),
    };
    let prepared =
        match prepare_access_token(&state, &transaction, &context.grant.grant_id, now).await {
            Ok(value) => value,
            Err(_) => return unavailable(),
        };
    let token_expiry = match add_ms(now, state.config.refresh_token_lifetime) {
        Ok(value) => value.min(context.family_expires_at_ms),
        Err(_) => return unavailable(),
    };
    let request = OAuthRefreshRotationRequest {
        refresh_token_digest: refresh_digest,
        client_id,
        resource,
        scopes: scopes.clone(),
        replacement_refresh_token_digest: digest(&replacement),
        replacement_refresh_token_expires_at_ms: token_expiry,
        access_token: prepared.metadata,
        now_ms: now,
    };
    match state.store.rotate_refresh_token(request).await {
        Ok(OAuthRefreshRotationResult::Rotated { .. }) => token_success(
            prepared.jwt,
            replacement,
            &scopes,
            state.config.access_token_lifetime,
        ),
        Ok(OAuthRefreshRotationResult::ReplayDetected) => {
            state
                .revocations
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            token_error(McpTokenErrorCode::InvalidGrant)
        }
        Ok(OAuthRefreshRotationResult::InvalidGrant) => {
            token_error(McpTokenErrorCode::InvalidGrant)
        }
        Err(_) => unavailable(),
    }
}

async fn revoke(State(state): State<Arc<AuthorizationState>>, request: Request<Body>) -> Response {
    let mut p = match strict_form(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if p.keys()
        .any(|key| !["token", "token_type_hint", "client_id"].contains(&key.as_str()))
    {
        return revocation_error();
    }
    let Some(token) = take_required(&mut p, "token") else {
        return revocation_error();
    };
    let Some(client_id) = take_required(&mut p, "client_id") else {
        return revocation_error();
    };
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return unavailable(),
    };
    let hint = p.remove("token_type_hint");
    let target = if hint.as_deref() == Some("refresh_token") || !token.contains('.') {
        OAuthRevocationTarget::RefreshToken {
            digest: digest(&token),
            client_id,
        }
    } else if let Some(jti) = unverified_jti(&token) {
        OAuthRevocationTarget::AccessToken { jti, client_id }
    } else {
        return no_store(StatusCode::OK, Body::empty());
    };
    if state.store.revoke(target, now).await.is_err() {
        return unavailable();
    }
    state
        .revocations
        .send_modify(|generation| *generation = generation.wrapping_add(1));
    no_store(StatusCode::OK, Body::empty())
}

struct PreparedToken {
    jwt: String,
    metadata: OAuthTokenMetadata,
}

async fn prepare_access_token(
    state: &AuthorizationState,
    transaction: &OAuthAuthorizationTransaction,
    grant_id: &str,
    now: i64,
) -> OAuthAuthorizationStoreResult<PreparedToken> {
    let key = state
        .store
        .active_signing_key(state.config.issuer.clone())
        .await?
        .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
    let encrypted = key
        .encrypted_private_key
        .as_ref()
        .ok_or(OAuthAuthorizationStoreError::KeyUnavailable)?;
    super::validate_es256_signing_key_readiness(&key, state.encryption.as_ref())?;
    let private = state.encryption.decrypt("ES256", &key.key_id, encrypted)?;
    let secret = SecretKey::from_pkcs8_der(&private)
        .map_err(|_| OAuthAuthorizationStoreError::KeyUnavailable)?;
    let jti = random_secret(state.entropy.as_ref(), 18)?;
    let exp_ms = add_ms(now, state.config.access_token_lifetime)?;
    let now_seconds =
        u64::try_from(now / 1000).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    let claims = McpJwtAccessTokenClaims {
        iss: state.config.issuer.clone(),
        sub: transaction
            .principal_id
            .clone()
            .ok_or(OAuthAuthorizationStoreError::InvalidData)?,
        aud: McpJwtAudience::One(transaction.resource.clone()),
        exp: u64::try_from(exp_ms / 1000).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
        iat: now_seconds,
        nbf: now_seconds.saturating_sub(state.config.clock_skew.as_secs()),
        jti: jti.clone(),
        scope: Some(transaction.scopes.join(" ")),
        client_id: Some(transaction.client_id.clone()),
        grant_id: Some(grant_id.to_owned()),
    };
    let header = McpJwtAccessTokenHeader {
        alg: "ES256".to_owned(),
        typ: "at+jwt".to_owned(),
        kid: key.key_id.clone(),
    };
    let encoded_header = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&header).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
    );
    let encoded_claims = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&claims).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
    );
    let message = format!("{encoded_header}.{encoded_claims}");
    let signature: Signature = SigningKey::from(secret).sign(message.as_bytes());
    Ok(PreparedToken {
        jwt: format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())),
        metadata: OAuthTokenMetadata {
            jti,
            signing_key_id: key.key_id,
            issued_at_ms: now,
            expires_at_ms: exp_ms,
        },
    })
}

async fn validate_hosted_token(
    state: Arc<AuthorizationState>,
    token: String,
    context: crate::server::McpAuthorizationContext,
) -> McpHostedTokenValidation {
    let now = match now_ms(state.clock.now()) {
        Ok(value) => value,
        Err(_) => return McpHostedTokenValidation::Unavailable,
    };
    let (_header, claims) = match verify_jwt(&state, &token, now).await {
        Ok(value) => value,
        Err(OAuthAuthorizationStoreError::Unavailable) => {
            return McpHostedTokenValidation::Unavailable;
        }
        Err(_) => {
            return McpHostedTokenValidation::Unauthorized {
                error_description: Some("invalid access token".to_owned()),
            };
        }
    };
    let resource = match &claims.aud {
        McpJwtAudience::One(value) => value.clone(),
        McpJwtAudience::Many(_) => {
            return McpHostedTokenValidation::Unauthorized {
                error_description: Some("invalid access token audience".to_owned()),
            };
        }
    };
    if resource != context.resource {
        return McpHostedTokenValidation::Unauthorized {
            error_description: Some("invalid access token audience".to_owned()),
        };
    }
    let scopes = claims
        .scope
        .as_deref()
        .map(canonical_scopes)
        .transpose()
        .unwrap_or_default()
        .unwrap_or_default();
    let missing = context
        .required_scopes
        .iter()
        .filter(|scope| !scopes.contains(scope))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return McpHostedTokenValidation::InsufficientScope {
            required_scopes: missing,
            error_description: None,
        };
    }
    let Some(client_id) = claims.client_id.clone() else {
        return McpHostedTokenValidation::Unauthorized {
            error_description: None,
        };
    };
    let Some(grant_id) = claims.grant_id.clone() else {
        return McpHostedTokenValidation::Unauthorized {
            error_description: None,
        };
    };
    let principal_id = match McpPrincipalId::new(claims.sub.clone()) {
        Ok(value) => value,
        Err(_) => {
            return McpHostedTokenValidation::Unauthorized {
                error_description: None,
            };
        }
    };
    let validation = OAuthValidateAccessTokenRequest {
        jti: claims.jti.clone(),
        client_id,
        grant_id,
        principal_id: principal_id.clone(),
        resource,
        scopes,
        now_ms: now,
    };
    match state.store.validate_access_token(validation.clone()).await {
        Ok(true) => {
            let notifications = state.revocations.subscribe();
            let interval = state.config.revocation_poll_interval;
            let store = state.store.clone();
            let clock = state.clock.clone();
            let lease = McpRevocationLease::new(
                move || {
                    let store = store.clone();
                    let validation = validation.clone();
                    let clock = clock.clone();
                    Box::pin(async move {
                        store
                            .validate_access_token(OAuthValidateAccessTokenRequest {
                                now_ms: now_ms(clock.now()).unwrap_or(i64::MAX),
                                ..validation
                            })
                            .await
                            .map_err(|_| ())
                    })
                },
                interval,
                notifications,
            );
            McpHostedTokenValidation::Authorized(McpTokenAuthorization {
                principal_id,
                expires_at: UNIX_EPOCH.checked_add(Duration::from_secs(claims.exp)),
                revocation: Some(lease),
            })
        }
        Ok(false) => McpHostedTokenValidation::Unauthorized {
            error_description: None,
        },
        Err(_) => McpHostedTokenValidation::Unavailable,
    }
}

async fn verify_jwt(
    state: &AuthorizationState,
    token: &str,
    now: i64,
) -> OAuthAuthorizationStoreResult<(McpJwtAccessTokenHeader, McpJwtAccessTokenClaims)> {
    use p256::ecdsa::{VerifyingKey, signature::Verifier as _};
    let mut pieces = token.split('.');
    let (Some(encoded_header), Some(encoded_claims), Some(encoded_signature), None) =
        (pieces.next(), pieces.next(), pieces.next(), pieces.next())
    else {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    };
    let header: McpJwtAccessTokenHeader = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded_header)
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
    )
    .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    if header.alg != "ES256" || header.typ != "at+jwt" || header.kid.is_empty() {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    let keys = state
        .store
        .verification_signing_keys(state.config.issuer.clone(), now)
        .await?;
    let key = keys
        .into_iter()
        .find(|key| key.key_id == header.kid)
        .ok_or(OAuthAuthorizationStoreError::InvalidData)?;
    let x = decode_jwk_coordinate(&key.public_jwk, "x")?;
    let y = decode_jwk_coordinate(&key.public_jwk, "y")?;
    let mut point = Vec::with_capacity(65);
    point.push(4);
    point.extend_from_slice(&x);
    point.extend_from_slice(&y);
    let verifying = VerifyingKey::from_sec1_bytes(&point)
        .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    let signature = Signature::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded_signature)
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
    )
    .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    verifying
        .verify(
            format!("{encoded_header}.{encoded_claims}").as_bytes(),
            &signature,
        )
        .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    let claims: McpJwtAccessTokenClaims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded_claims)
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
    )
    .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    let now_seconds =
        u64::try_from(now / 1000).map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    let skew = state.config.clock_skew.as_secs();
    if claims.iss != state.config.issuer
        || claims.exp.saturating_add(skew) <= now_seconds
        || claims.nbf > now_seconds.saturating_add(skew)
        || claims.iat > now_seconds.saturating_add(skew)
    {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    Ok((header, claims))
}

struct ParsedAuthorization {
    client_id: String,
    redirect_uri: String,
    state: String,
    resource: String,
    scopes: Vec<String>,
    code_challenge: String,
}
fn parse_authorization(
    mut p: BTreeMap<String, String>,
) -> std::result::Result<ParsedAuthorization, &'static str> {
    if p.keys().any(|key| {
        ![
            "response_type",
            "client_id",
            "redirect_uri",
            "scope",
            "state",
            "code_challenge",
            "code_challenge_method",
            "resource",
        ]
        .contains(&key.as_str())
    }) {
        return Err("invalid_request");
    }
    if take_required(&mut p, "response_type").as_deref() != Some("code") {
        return Err("unsupported_response_type");
    }
    if take_required(&mut p, "code_challenge_method").as_deref() != Some("S256") {
        return Err("invalid_request");
    }
    let client_id = take_required(&mut p, "client_id").ok_or("invalid_request")?;
    let redirect_uri = take_required(&mut p, "redirect_uri").ok_or("invalid_request")?;
    let state = take_required(&mut p, "state").ok_or("invalid_request")?;
    let resource = take_required(&mut p, "resource").ok_or("invalid_request")?;
    if [&client_id, &redirect_uri, &state, &resource]
        .into_iter()
        .any(|value| value.len() > 2048)
    {
        return Err("invalid_request");
    }
    Ok(ParsedAuthorization {
        client_id,
        redirect_uri,
        state,
        resource,
        scopes: canonical_scopes(&p.remove("scope").unwrap_or_default())
            .map_err(|_| "invalid_scope")?,
        code_challenge: take_required(&mut p, "code_challenge")
            .filter(|value| valid_challenge(value))
            .ok_or("invalid_request")?,
    })
}
fn validate_authorization_bindings(
    config: &OAuthAuthorizationServerConfig,
    parsed: &ParsedAuthorization,
) -> std::result::Result<(), &'static str> {
    let resource = config
        .resources
        .iter()
        .find(|value| value.resource == parsed.resource)
        .ok_or("invalid_request")?;
    if parsed
        .scopes
        .iter()
        .all(|scope| resource.scopes.contains(scope))
    {
        Ok(())
    } else {
        Err("invalid_scope")
    }
}

fn validate_config(config: &OAuthAuthorizationServerConfig) -> Result<()> {
    let issuer =
        Url::parse(&config.issuer).map_err(|_| Error::protocol("OAuth issuer is invalid"))?;
    if issuer.scheme() != "https"
        || !issuer.username().is_empty()
        || issuer.password().is_some()
        || issuer.query().is_some()
        || issuer.fragment().is_some()
    {
        return Err(Error::protocol(
            "OAuth issuer must be an exact HTTPS URL without query or fragment",
        ));
    }
    for endpoint in [
        &config.authorization_endpoint,
        &config.authorization_decision_endpoint,
        &config.token_endpoint,
        &config.revocation_endpoint,
        &config.jwks_uri,
    ] {
        let url = Url::parse(endpoint).map_err(|_| Error::protocol("OAuth endpoint is invalid"))?;
        if url.scheme() != "https"
            || url.host_str() != issuer.host_str()
            || url.port_or_known_default() != issuer.port_or_known_default()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::protocol(
                "OAuth endpoints must be same-host exact HTTPS URLs",
            ));
        }
    }
    if let Some(endpoint) = &config.registration_endpoint {
        let url = Url::parse(endpoint).map_err(|_| Error::protocol("OAuth endpoint is invalid"))?;
        if url.scheme() != "https"
            || url.host_str() != issuer.host_str()
            || url.port_or_known_default() != issuer.port_or_known_default()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::protocol(
                "OAuth endpoints must be same-origin exact HTTPS URLs",
            ));
        }
    }
    if config.resources.is_empty()
        || config
            .resources
            .iter()
            .any(|resource| Url::parse(&resource.resource).is_err() || resource.scopes.is_empty())
    {
        return Err(Error::protocol(
            "OAuth resources and scopes must be non-empty valid URLs",
        ));
    }
    if [
        config.authorization_transaction_lifetime,
        config.authorization_code_lifetime,
        config.access_token_lifetime,
        config.refresh_token_lifetime,
        config.refresh_family_lifetime,
        config.revocation_poll_interval,
    ]
    .iter()
    .any(|lifetime| lifetime.as_secs() == 0)
        || config.refresh_token_lifetime > config.refresh_family_lifetime
    {
        return Err(Error::protocol("OAuth lifetimes are invalid"));
    }
    let required_verification_overlap = config
        .access_token_lifetime
        .checked_add(config.clock_skew)
        .ok_or_else(|| Error::protocol("OAuth signing verification overlap is invalid"))?;
    if config.signing_verification_overlap < required_verification_overlap {
        return Err(Error::protocol(
            "OAuth signing verification overlap must cover access-token lifetime plus clock skew",
        ));
    }
    if config.signing_private_key_overlap > config.signing_verification_overlap {
        return Err(Error::protocol(
            "OAuth signing private-key overlap must not exceed verification overlap",
        ));
    }
    Ok(())
}

async fn strict_form(
    request: Request<Body>,
) -> std::result::Result<BTreeMap<String, String>, Response> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            !value.split(';').next().is_some_and(|value| {
                value
                    .trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        })
    {
        return Err(token_error(McpTokenErrorCode::InvalidRequest));
    }
    let body = to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|_| token_error(McpTokenErrorCode::InvalidRequest))?;
    strict_parameters(&body).map_err(|_| token_error(McpTokenErrorCode::InvalidRequest))
}
fn strict_parameters(bytes: &[u8]) -> std::result::Result<BTreeMap<String, String>, ()> {
    const MAX_PARAMETER_NAME_BYTES: usize = 64;
    const MAX_PARAMETER_VALUE_BYTES: usize = 4096;
    let mut values = BTreeMap::new();
    for (name, value) in url::form_urlencoded::parse(bytes) {
        if name.is_empty()
            || name.len() > MAX_PARAMETER_NAME_BYTES
            || value.len() > MAX_PARAMETER_VALUE_BYTES
            || values
                .insert(name.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(());
        }
    }
    Ok(values)
}
fn canonical_scopes(value: &str) -> std::result::Result<Vec<String>, ()> {
    if value.len() > 2048 {
        return Err(());
    }
    let mut scopes = value
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if scopes
        .iter()
        .any(|scope| scope.is_empty() || scope.chars().any(char::is_control))
    {
        return Err(());
    }
    scopes.sort();
    scopes.dedup();
    Ok(scopes)
}
fn take_required(values: &mut BTreeMap<String, String>, name: &str) -> Option<String> {
    values.remove(name).filter(|value| !value.is_empty())
}
fn valid_verifier(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}
fn valid_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn digest(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}
fn random_secret(
    entropy: &dyn McpOAuthEntropy,
    bytes: usize,
) -> OAuthAuthorizationStoreResult<String> {
    let mut value = vec![0_u8; bytes];
    entropy
        .fill_bytes(&mut value)
        .map_err(|_| OAuthAuthorizationStoreError::Unavailable)?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}
fn now_ms(time: SystemTime) -> OAuthAuthorizationStoreResult<i64> {
    i64::try_from(
        time.duration_since(UNIX_EPOCH)
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?
            .as_millis(),
    )
    .map_err(|_| OAuthAuthorizationStoreError::InvalidData)
}
fn add_ms(now: i64, duration: Duration) -> OAuthAuthorizationStoreResult<i64> {
    now.checked_add(
        i64::try_from(duration.as_millis())
            .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?,
    )
    .ok_or(OAuthAuthorizationStoreError::InvalidData)
}
fn endpoint_path(endpoint: &str) -> String {
    Url::parse(endpoint)
        .expect("validated endpoint")
        .path()
        .to_owned()
}
fn metadata_path(issuer: &str) -> String {
    let issuer = Url::parse(issuer).expect("validated issuer");
    let path = issuer.path().trim_end_matches('/');
    format!("/.well-known/oauth-authorization-server{path}")
}
fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
fn safe_host_redirect(issuer: &str, location: &str) -> bool {
    Url::parse(issuer)
        .ok()
        .zip(Url::parse(location).ok())
        .is_some_and(|(issuer, location)| {
            location.scheme() == issuer.scheme()
                && location.host_str() == issuer.host_str()
                && location.port_or_known_default() == issuer.port_or_known_default()
                && location.username().is_empty()
                && location.password().is_none()
        })
}
fn decode_jwk_coordinate(
    value: &serde_json::Value,
    name: &str,
) -> OAuthAuthorizationStoreResult<Vec<u8>> {
    let encoded = value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or(OAuthAuthorizationStoreError::InvalidData)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| OAuthAuthorizationStoreError::InvalidData)?;
    if bytes.len() == 32 {
        Ok(bytes)
    } else {
        Err(OAuthAuthorizationStoreError::InvalidData)
    }
}
fn unverified_jti(token: &str) -> Option<String> {
    let claims = token.split('.').nth(1)?;
    let value: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;
    value.get("jti")?.as_str().map(str::to_owned)
}

fn token_success(
    access_token: String,
    refresh_token: String,
    scopes: &[String],
    lifetime: Duration,
) -> Response {
    json_response(
        StatusCode::OK,
        &McpTokenResponse {
            access_token: McpOAuthSecret::new(access_token),
            token_type: McpOAuthTokenType::Bearer,
            expires_in: Some(lifetime.as_secs()),
            refresh_token: Some(McpOAuthSecret::new(refresh_token)),
            scope: Some(scopes.join(" ")),
        },
        true,
    )
}
fn token_error(error: McpTokenErrorCode) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        &McpTokenError {
            error,
            error_description: None,
            error_uri: None,
        },
        true,
    )
}
fn revocation_error() -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        &serde_json::json!({"error":"invalid_request"}),
        true,
    )
}
fn local_authorization_error(status: StatusCode, error: &str) -> Response {
    let mut response = no_store(status, Body::from(error.to_owned()));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
fn authorization_redirect(
    redirect_uri: &str,
    state: &str,
    error: &str,
    description: Option<&str>,
    issuer: &str,
) -> Response {
    let Ok(mut url) = Url::parse(redirect_uri) else {
        return local_authorization_error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("error", error)
            .append_pair("state", state)
            .append_pair("iss", issuer);
        if let Some(description) = description {
            query.append_pair("error_description", description);
        }
    }
    redirect_response(url.as_str())
}
fn json_response(status: StatusCode, value: &impl Serialize, sensitive: bool) -> Response {
    let body = match serde_json::to_vec(value) {
        Ok(value) => Body::from(value),
        Err(_) => return unavailable(),
    };
    let mut response = if sensitive {
        no_store(status, body)
    } else {
        (status, body).into_response()
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if !sensitive {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
fn no_store(status: StatusCode, body: Body) -> Response {
    let mut response = (status, body).into_response();
    cache_control_no_store(response.headers_mut());
    response
}
fn redirect_response(location: &str) -> Response {
    let mut response = Redirect::to(location).into_response();
    cache_control_no_store(response.headers_mut());
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
fn cache_control_no_store(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
}
fn unavailable() -> Response {
    no_store(StatusCode::SERVICE_UNAVAILABLE, Body::empty())
}
