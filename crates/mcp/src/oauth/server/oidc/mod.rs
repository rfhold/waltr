#![allow(clippy::too_many_lines)]

mod store;

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    response::{IntoResponse as _, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie::{Cookie, SameSite, time};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, Jwk, JwkSet, KeyOperations, PublicKeyUse},
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256, Sha384, Sha512};
use tokio::sync::Mutex;
use url::{Position, Url};
use zeroize::Zeroizing;

pub use store::*;

use super::{
    McpOAuthClock, McpOAuthEntropy, OAuthBrowserCorrelationCookie,
    OAuthResourceOwnerAuthentication, OAuthResourceOwnerAuthenticationRequest,
    OAuthResourceOwnerAuthenticator,
};
use crate::{McpOAuthSecret, McpPrincipalId, Result, server::BoxFuture};

const ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];
const BODY_LIMIT: usize = 64 * 1024;
const JWKS_LIMIT: usize = 1024 * 1024;
const MAX_KEYS: usize = 64;
const CLOCK_SKEW_SECONDS: u64 = 60;
const MAX_TOKEN_AGE_SECONDS: u64 = 10 * 60;
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP_TIMEOUT: Duration = Duration::from_mins(1);

/// OIDC endpoint and cookie transport policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OidcEndpointPolicy {
    /// Require HTTPS for every endpoint and Secure cookies.
    HttpsOnly,
    /// Explicitly permit HTTP only on loopback hosts for tests and local development.
    LoopbackTest,
}

impl OidcEndpointPolicy {
    fn allows(self, url: &Url) -> bool {
        url.scheme() == "https"
            || self == Self::LoopbackTest
                && url.scheme() == "http"
                && url.host_str().is_some_and(is_loopback)
    }
}

/// Validated one-shot resource-owner OIDC configuration.
#[derive(Clone)]
pub struct OidcResourceOwnerConfig {
    issuer: String,
    client_id: String,
    client_secret: Option<McpOAuthSecret>,
    callback_uri: Url,
    continuation_endpoint: Url,
    login_uri: Url,
    scopes: Vec<String>,
    attempt_lifetime: Duration,
    http_timeout: Duration,
    endpoint_policy: OidcEndpointPolicy,
}

impl fmt::Debug for OidcResourceOwnerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcResourceOwnerConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("callback_uri", &self.callback_uri)
            .field("continuation_endpoint", &self.continuation_endpoint)
            .field("login_uri", &self.login_uri)
            .field("scopes", &self.scopes)
            .field("attempt_lifetime", &self.attempt_lifetime)
            .field("http_timeout", &self.http_timeout)
            .field("endpoint_policy", &self.endpoint_policy)
            .finish()
    }
}

impl OidcResourceOwnerConfig {
    /// Validate OIDC and local hosted-authorization endpoint settings.
    ///
    /// # Errors
    ///
    /// Returns an error when a setting is malformed, unbounded, or violates transport policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: Option<McpOAuthSecret>,
        callback_uri: impl AsRef<str>,
        continuation_endpoint: impl AsRef<str>,
        scopes: Vec<String>,
        attempt_lifetime: Duration,
        endpoint_policy: OidcEndpointPolicy,
    ) -> std::result::Result<Self, OidcResourceOwnerError> {
        let issuer = clean("issuer", issuer.into())?;
        let client_id = clean("client ID", client_id.into())?;
        let issuer_url = endpoint(&issuer, endpoint_policy)?;
        if issuer.len() > 2048 || client_id.len() > 2048 || issuer_url.query().is_some() {
            return Err(OidcResourceOwnerError::Configuration(
                "OIDC issuer or client ID is invalid",
            ));
        }
        if client_secret.as_ref().is_some_and(|secret| {
            secret.expose_secret().is_empty()
                || secret.expose_secret().len() > 4096
                || secret.expose_secret().chars().any(char::is_control)
        }) {
            return Err(OidcResourceOwnerError::Configuration(
                "OIDC client secret is invalid",
            ));
        }
        let callback_uri = endpoint(callback_uri.as_ref(), endpoint_policy)?;
        let continuation_endpoint = endpoint(continuation_endpoint.as_ref(), endpoint_policy)?;
        if callback_uri.query().is_some()
            || callback_uri.fragment().is_some()
            || continuation_endpoint.query().is_some()
            || continuation_endpoint.fragment().is_some()
            || origin(&callback_uri) != origin(&continuation_endpoint)
            || callback_uri == continuation_endpoint
            || callback_uri.as_str().len() > 2048
            || continuation_endpoint.as_str().len() > 2048
        {
            return Err(OidcResourceOwnerError::Configuration(
                "local OIDC endpoints are invalid",
            ));
        }
        if attempt_lifetime.is_zero() || attempt_lifetime > Duration::from_mins(10) {
            return Err(OidcResourceOwnerError::Configuration(
                "OIDC attempt lifetime must be between one second and ten minutes",
            ));
        }
        if scopes.is_empty() || scopes.len() > 32 {
            return Err(OidcResourceOwnerError::Configuration(
                "OIDC scope set is invalid",
            ));
        }
        let mut normalized = Vec::with_capacity(scopes.len());
        for scope in scopes {
            if scope.is_empty() || scope.len() > 256 || scope.chars().any(char::is_whitespace) {
                return Err(OidcResourceOwnerError::Configuration(
                    "OIDC scope is invalid",
                ));
            }
            if !normalized.contains(&scope) {
                normalized.push(scope);
            }
        }
        if !normalized.iter().any(|scope| scope == "openid") {
            return Err(OidcResourceOwnerError::Configuration(
                "OIDC scopes must include openid",
            ));
        }
        let mut login_uri = continuation_endpoint.clone();
        let continuation_parent = continuation_endpoint
            .path()
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        login_uri.set_path(&format!("{continuation_parent}/oidc/login"));
        Ok(Self {
            issuer,
            client_id,
            client_secret,
            callback_uri,
            continuation_endpoint,
            login_uri,
            scopes: normalized,
            attempt_lifetime,
            http_timeout: DEFAULT_HTTP_TIMEOUT,
            endpoint_policy,
        })
    }

    /// Override the bounded total timeout for discovery, JWKS, and token requests.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or one greater than one minute.
    pub fn with_http_timeout(
        mut self,
        timeout: Duration,
    ) -> std::result::Result<Self, OidcResourceOwnerError> {
        if timeout.is_zero() || timeout > MAX_HTTP_TIMEOUT {
            return Err(OidcResourceOwnerError::Configuration(
                "OIDC HTTP timeout must be between one millisecond and one minute",
            ));
        }
        self.http_timeout = timeout;
        Ok(self)
    }

    /// Exact configured upstream issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    /// OIDC client identifier.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    /// Confidential client secret, when configured.
    #[must_use]
    pub const fn client_secret(&self) -> Option<&McpOAuthSecret> {
        self.client_secret.as_ref()
    }
    /// Exact registered callback URI.
    #[must_use]
    pub const fn callback_uri(&self) -> &Url {
        &self.callback_uri
    }
    /// Known hosted authorization continuation endpoint.
    #[must_use]
    pub const fn continuation_endpoint(&self) -> &Url {
        &self.continuation_endpoint
    }
    /// Requested OIDC scopes, including `openid`.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
    /// Absolute one-shot attempt lifetime.
    #[must_use]
    pub const fn attempt_lifetime(&self) -> Duration {
        self.attempt_lifetime
    }
    /// Total timeout applied to every OIDC backchannel request.
    #[must_use]
    pub const fn http_timeout(&self) -> Duration {
        self.http_timeout
    }
    /// Validated endpoint and cookie policy.
    #[must_use]
    pub const fn endpoint_policy(&self) -> OidcEndpointPolicy {
        self.endpoint_policy
    }
}

/// Cryptographically verified, provider-neutral ID-token identity and claims.
#[derive(Clone)]
pub struct OidcVerifiedIdentity {
    /// Exact verified issuer.
    pub issuer: String,
    /// Nonempty verified subject.
    pub subject: String,
    /// Verified token audiences.
    pub audiences: Vec<String>,
    /// Signature-verified provider claims. Standard security claims remain included.
    pub claims: serde_json::Map<String, serde_json::Value>,
}

impl fmt::Debug for OidcVerifiedIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcVerifiedIdentity")
            .field("issuer", &self.issuer)
            .field("subject", &self.subject)
            .field("audiences", &self.audiences)
            .field("claims", &"[redacted]")
            .finish()
    }
}

/// Principal mapping result after OIDC verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OidcPrincipalMapping {
    /// Stable generic MCP principal.
    Principal(McpPrincipalId),
    /// Local policy denied this verified identity.
    Denied,
    /// Mapping infrastructure is unavailable.
    Unavailable,
}

/// Maps only verified provider-neutral OIDC identity into an MCP principal.
pub trait OidcPrincipalMapper: Send + Sync + 'static {
    /// Map a verified identity.
    fn map(&self, identity: OidcVerifiedIdentity) -> BoxFuture<OidcPrincipalMapping>;
}

/// Convenience mapper that uses the OIDC subject directly.
#[derive(Clone, Copy, Debug, Default)]
pub struct OidcSubjectPrincipalMapper;

impl OidcPrincipalMapper for OidcSubjectPrincipalMapper {
    fn map(&self, identity: OidcVerifiedIdentity) -> BoxFuture<OidcPrincipalMapping> {
        Box::pin(async move {
            McpPrincipalId::new(identity.subject).map_or(
                OidcPrincipalMapping::Denied,
                OidcPrincipalMapping::Principal,
            )
        })
    }
}

/// One-shot OIDC implementation of the hosted resource-owner authenticator.
#[derive(Clone)]
pub struct OidcResourceOwnerAuthenticator {
    state: Arc<OidcState>,
}

impl fmt::Debug for OidcResourceOwnerAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcResourceOwnerAuthenticator")
            .field("config", &self.state.config)
            .finish_non_exhaustive()
    }
}

struct OidcState {
    config: OidcResourceOwnerConfig,
    provider: Provider,
    store: Arc<dyn OidcResourceOwnerStore>,
    mapper: Arc<dyn OidcPrincipalMapper>,
    clock: Arc<dyn McpOAuthClock>,
    entropy: Arc<dyn McpOAuthEntropy>,
}

impl OidcResourceOwnerAuthenticator {
    /// Discover and validate the provider, including its initial JWKS.
    ///
    /// # Errors
    ///
    /// Returns an error when HTTP client construction, discovery, or initial JWKS loading fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn discover(
        config: OidcResourceOwnerConfig,
        store: Arc<dyn OidcResourceOwnerStore>,
        mapper: Arc<dyn OidcPrincipalMapper>,
        clock: Arc<dyn McpOAuthClock>,
        entropy: Arc<dyn McpOAuthEntropy>,
        builder: reqwest::ClientBuilder,
    ) -> std::result::Result<Self, OidcResourceOwnerError> {
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.http_timeout)
            .build()
            .map_err(|_| OidcResourceOwnerError::Unavailable)?;
        let provider = Provider::discover(&client, &config).await?;
        Ok(Self {
            state: Arc::new(OidcState {
                config,
                provider,
                store,
                mapper,
                clock,
                entropy,
            }),
        })
    }

    /// Issuer-relative one-shot login and exact callback routes.
    pub fn router(&self) -> Router {
        Router::new()
            .route(self.state.config.login_uri.path(), get(login))
            .route(self.state.config.callback_uri.path(), get(callback))
            .with_state(self.state.clone())
    }

    /// Delete a bounded number of expired attempts.
    ///
    /// # Errors
    ///
    /// Returns an error when the clock or backing store is unavailable or invalid.
    pub async fn cleanup(&self, limit: usize) -> OidcStoreResult<usize> {
        self.state
            .store
            .cleanup(now_ms(self.state.clock.now())?, limit)
            .await
    }
}

impl OAuthResourceOwnerAuthenticator for OidcResourceOwnerAuthenticator {
    fn authenticate(
        &self,
        request: OAuthResourceOwnerAuthenticationRequest,
    ) -> BoxFuture<OAuthResourceOwnerAuthentication> {
        let state = self.state.clone();
        Box::pin(async move {
            if request.browser_correlation.expose_secret().is_empty() {
                return resume(state, request).await;
            }
            initial(state, request).await
        })
    }

    fn browser_correlation_cookie(
        &self,
        transaction_id: &str,
        correlation: &McpOAuthSecret,
    ) -> Option<OAuthBrowserCorrelationCookie> {
        correlation_cookie(
            &self.state.config,
            transaction_id,
            correlation.expose_secret(),
            false,
        )
        .ok()
        .and_then(|cookie| {
            OAuthBrowserCorrelationCookie::new(
                &cookie,
                self.state.config.endpoint_policy == OidcEndpointPolicy::HttpsOnly,
            )
            .ok()
        })
    }

    fn clearing_browser_correlation_cookie(
        &self,
        transaction_id: &str,
    ) -> Option<OAuthBrowserCorrelationCookie> {
        correlation_cookie(&self.state.config, transaction_id, "deleted", true)
            .ok()
            .and_then(|cookie| {
                OAuthBrowserCorrelationCookie::new(
                    &cookie,
                    self.state.config.endpoint_policy == OidcEndpointPolicy::HttpsOnly,
                )
                .ok()
            })
    }

    fn finalize_authentication(&self, transaction_id: String) -> BoxFuture<()> {
        let state = self.state.clone();
        Box::pin(async move {
            if let Ok(now) = now_ms(state.clock.now()) {
                let _finalization = state.store.finalize_completed(transaction_id, now).await;
            }
        })
    }
}

async fn initial(
    state: Arc<OidcState>,
    request: OAuthResourceOwnerAuthenticationRequest,
) -> OAuthResourceOwnerAuthentication {
    let Ok(now) = now_ms(state.clock.now()) else {
        return OAuthResourceOwnerAuthentication::Unavailable;
    };
    let Ok(state_secret) = random(&*state.entropy, 32) else {
        return OAuthResourceOwnerAuthentication::Unavailable;
    };
    let Ok(nonce) = random(&*state.entropy, 32) else {
        return OAuthResourceOwnerAuthentication::Unavailable;
    };
    let Ok(verifier) = random(&*state.entropy, 32) else {
        return OAuthResourceOwnerAuthentication::Unavailable;
    };
    let Ok(expires) = add_ms(now, state.config.attempt_lifetime) else {
        return OAuthResourceOwnerAuthentication::Unavailable;
    };
    let attempt = OidcResourceOwnerAttempt {
        transaction_id: request.transaction_id,
        state_digest: digest(&state_secret),
        browser_correlation_digest: digest(request.browser_correlation.expose_secret()),
        nonce,
        pkce_verifier: verifier,
        created_at_ms: now,
        expires_at_ms: expires,
        callback_claimed_at_ms: None,
        completed_at_ms: None,
        principal_id: None,
        consumed_at_ms: None,
    };
    if !matches!(state.store.create(attempt).await, Ok(true)) {
        return OAuthResourceOwnerAuthentication::Unavailable;
    }
    let mut login = state.config.login_uri.clone();
    login.query_pairs_mut().append_pair("state", &state_secret);
    OAuthResourceOwnerAuthentication::LoginRequired {
        login_url: login.into(),
    }
}

async fn resume(
    state: Arc<OidcState>,
    request: OAuthResourceOwnerAuthenticationRequest,
) -> OAuthResourceOwnerAuthentication {
    let Ok(Some(cookie)) = strict_cookie(
        &request.headers,
        &cookie_name(&state.config, &request.transaction_id),
    ) else {
        return OAuthResourceOwnerAuthentication::Denied;
    };
    let Ok(now) = now_ms(state.clock.now()) else {
        return OAuthResourceOwnerAuthentication::Unavailable;
    };
    match state
        .store
        .completed(request.transaction_id, digest(&cookie), now)
        .await
    {
        Ok(Some(principal_id)) => OAuthResourceOwnerAuthentication::Authenticated {
            principal_id,
            browser_correlation: Some(McpOAuthSecret::new(cookie)),
        },
        Ok(None) => OAuthResourceOwnerAuthentication::Denied,
        Err(_) => OAuthResourceOwnerAuthentication::Unavailable,
    }
}

async fn login(State(state): State<Arc<OidcState>>, request: Request<Body>) -> Response {
    let Ok(parameters) = strict_query(request.uri().query().unwrap_or_default(), &["state"]) else {
        return bad_request();
    };
    let Some(oidc_state) = parameters.get("state") else {
        return bad_request();
    };
    let Ok(now) = now_ms(state.clock.now()) else {
        return unavailable();
    };
    let Some(attempt) = (match state.store.by_state(digest(oidc_state), now).await {
        Ok(v) => v,
        Err(_) => return unavailable(),
    }) else {
        return bad_request();
    };
    if validate_attempt_cookie(&state.config, &attempt, request.headers()).is_err() {
        return bad_request();
    }
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(attempt.pkce_verifier.as_bytes()));
    let Ok(url) =
        state
            .provider
            .authorization_url(&state.config, oidc_state, &attempt.nonce, &challenge)
    else {
        return unavailable();
    };
    redirect(url.as_str(), None)
}

async fn callback(State(state): State<Arc<OidcState>>, request: Request<Body>) -> Response {
    let Ok(parameters) = strict_query(
        request.uri().query().unwrap_or_default(),
        &["code", "error", "state", "iss"],
    ) else {
        return bad_request();
    };
    let Some(oidc_state) = parameters.get("state") else {
        return bad_request();
    };
    if parameters
        .get("iss")
        .is_some_and(|issuer| issuer != &state.config.issuer)
    {
        return bad_request();
    }
    let code = parameters.get("code");
    let error = parameters.get("error");
    if code.is_some() == error.is_some() {
        return bad_request();
    }
    let Ok(now) = now_ms(state.clock.now()) else {
        return unavailable();
    };
    let Some(attempt) = (match state.store.by_state(digest(oidc_state), now).await {
        Ok(v) => v,
        Err(_) => return unavailable(),
    }) else {
        return bad_request();
    };
    if validate_attempt_cookie(&state.config, &attempt, request.headers()).is_err() {
        return bad_request();
    }
    if error.is_some() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(code) = code else {
        return bad_request();
    };
    let claimed = match state
        .store
        .claim_callback(attempt.transaction_id.clone(), now)
        .await
    {
        Ok(Some(v)) => v,
        Ok(None) => return bad_request(),
        Err(_) => return unavailable(),
    };
    let Ok(tokens) = state
        .provider
        .exchange(&state.config, code, &claimed.pkce_verifier)
        .await
    else {
        return unavailable();
    };
    let Ok(identity) = state
        .provider
        .verify(
            &state.config,
            &tokens.id_token,
            &claimed.nonce,
            &tokens.access_token,
            now,
        )
        .await
    else {
        return bad_request();
    };
    let principal = match state.mapper.map(identity).await {
        OidcPrincipalMapping::Principal(v) => v,
        OidcPrincipalMapping::Denied => return StatusCode::FORBIDDEN.into_response(),
        OidcPrincipalMapping::Unavailable => return unavailable(),
    };
    if !matches!(
        state
            .store
            .complete(claimed.transaction_id.clone(), principal, now)
            .await,
        Ok(true)
    ) {
        return unavailable();
    }
    let mut continuation = state.config.continuation_endpoint.clone();
    continuation
        .query_pairs_mut()
        .append_pair("transaction_id", &claimed.transaction_id);
    redirect(continuation.as_str(), None)
}

#[derive(Clone)]
struct Provider {
    client: reqwest::Client,
    authorization_endpoint: Url,
    token_endpoint: Url,
    token_auth: TokenAuth,
    algorithms: BTreeSet<String>,
    jwks_uri: Url,
    keys: Arc<Mutex<HashMap<String, Jwk>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenAuth {
    Basic,
    Post,
    None,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Option<Vec<String>>,
    code_challenge_methods_supported: Option<Vec<String>>,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    id_token_signing_alg_values_supported: Vec<String>,
}

impl Provider {
    async fn discover(
        client: &reqwest::Client,
        config: &OidcResourceOwnerConfig,
    ) -> std::result::Result<Self, OidcResourceOwnerError> {
        let issuer = Url::parse(&config.issuer).map_err(|_| OidcResourceOwnerError::Discovery)?;
        let urls = discovery_urls(&issuer)?;
        let mut response = client
            .get(urls[0].clone())
            .send()
            .await
            .map_err(|_| OidcResourceOwnerError::Unavailable)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND && urls.len() == 2 {
            response = client
                .get(urls[1].clone())
                .send()
                .await
                .map_err(|_| OidcResourceOwnerError::Unavailable)?;
        }
        if !response.status().is_success() {
            return Err(OidcResourceOwnerError::Discovery);
        }
        let metadata: Metadata =
            super::super::discovery::parse_unique_json(&bounded(response, BODY_LIMIT).await?)
                .map_err(|_| OidcResourceOwnerError::Discovery)?;
        if metadata.issuer != config.issuer
            || !metadata
                .response_types_supported
                .iter()
                .any(|v| v == "code")
            || metadata
                .grant_types_supported
                .as_ref()
                .is_some_and(|v| !v.iter().any(|v| v == "authorization_code"))
            || !metadata
                .code_challenge_methods_supported
                .as_ref()
                .is_some_and(|v| v.iter().any(|v| v == "S256"))
        {
            return Err(OidcResourceOwnerError::Discovery);
        }
        let authorization_endpoint =
            endpoint(&metadata.authorization_endpoint, config.endpoint_policy)?;
        let token_endpoint = endpoint(&metadata.token_endpoint, config.endpoint_policy)?;
        let jwks_uri = endpoint(&metadata.jwks_uri, config.endpoint_policy)?;
        let algorithms = metadata
            .id_token_signing_alg_values_supported
            .into_iter()
            .filter(|name| parse_algorithm(name).is_some())
            .collect::<BTreeSet<_>>();
        if algorithms.is_empty() {
            return Err(OidcResourceOwnerError::Discovery);
        }
        let advertised = metadata.token_endpoint_auth_methods_supported.as_deref();
        let supports = |name: &str| {
            advertised.map_or(name == "client_secret_basic", |v| {
                v.iter().any(|v| v == name)
            })
        };
        let token_auth = if config.client_secret.is_some() && supports("client_secret_basic") {
            TokenAuth::Basic
        } else if config.client_secret.is_some() && supports("client_secret_post") {
            TokenAuth::Post
        } else if config.client_secret.is_none() && supports("none") {
            TokenAuth::None
        } else {
            return Err(OidcResourceOwnerError::Discovery);
        };
        let keys = fetch_keys(client, &jwks_uri, &algorithms).await?;
        Ok(Self {
            client: client.clone(),
            authorization_endpoint,
            token_endpoint,
            token_auth,
            algorithms,
            jwks_uri,
            keys: Arc::new(Mutex::new(keys)),
        })
    }

    fn authorization_url(
        &self,
        config: &OidcResourceOwnerConfig,
        state: &str,
        nonce: &str,
        challenge: &str,
    ) -> std::result::Result<Url, OidcResourceOwnerError> {
        if [state, nonce, challenge]
            .iter()
            .any(|v| v.is_empty() || v.chars().any(char::is_control))
        {
            return Err(OidcResourceOwnerError::InvalidRequest);
        }
        let mut url = self.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", config.callback_uri.as_str())
            .append_pair("scope", &config.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url)
    }

    async fn exchange(
        &self,
        config: &OidcResourceOwnerConfig,
        code: &str,
        verifier: &str,
    ) -> std::result::Result<Tokens, OidcResourceOwnerError> {
        if code.is_empty() || code.len() > 4096 {
            return Err(OidcResourceOwnerError::InvalidRequest);
        }
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", config.callback_uri.as_str()),
            ("code_verifier", verifier),
        ];
        let mut request = self.client.post(self.token_endpoint.clone());
        match self.token_auth {
            TokenAuth::Basic => {
                request = request.basic_auth(
                    &config.client_id,
                    config
                        .client_secret
                        .as_ref()
                        .map(McpOAuthSecret::expose_secret),
                );
            }
            TokenAuth::Post => {
                form.push(("client_id", &config.client_id));
                form.push((
                    "client_secret",
                    config
                        .client_secret
                        .as_ref()
                        .expect("selected with secret")
                        .expose_secret(),
                ));
            }
            TokenAuth::None => form.push(("client_id", &config.client_id)),
        }
        let response = request
            .form(&form)
            .send()
            .await
            .map_err(|_| OidcResourceOwnerError::Unavailable)?;
        if !response.status().is_success() {
            return Err(OidcResourceOwnerError::Provider);
        }
        let raw: RawTokens =
            super::super::discovery::parse_unique_json(&bounded(response, BODY_LIMIT).await?)
                .map_err(|_| OidcResourceOwnerError::Provider)?;
        let tokens = Tokens {
            access_token: Zeroizing::new(raw.access_token),
            id_token: Zeroizing::new(raw.id_token),
            token_type: raw.token_type,
        };
        if tokens.access_token.is_empty()
            || tokens.id_token.is_empty()
            || !tokens.token_type.eq_ignore_ascii_case("bearer")
        {
            return Err(OidcResourceOwnerError::Provider);
        }
        Ok(tokens)
    }

    #[allow(clippy::too_many_arguments)]
    async fn verify(
        &self,
        config: &OidcResourceOwnerConfig,
        token: &str,
        nonce: &str,
        access: &str,
        now_ms: i64,
    ) -> std::result::Result<OidcVerifiedIdentity, OidcResourceOwnerError> {
        let protected = strict_jwt_json(token, 0)?;
        if protected.get("crit").is_some() {
            return Err(OidcResourceOwnerError::InvalidToken);
        }
        strict_jwt_json(token, 1)?;
        let header = decode_header(token).map_err(|_| OidcResourceOwnerError::InvalidToken)?;
        let name = algorithm_name(header.alg).ok_or(OidcResourceOwnerError::InvalidToken)?;
        if !ALGORITHMS.contains(&header.alg)
            || !self.algorithms.contains(name)
            || header
                .typ
                .as_deref()
                .is_some_and(|v| !v.eq_ignore_ascii_case("JWT"))
        {
            return Err(OidcResourceOwnerError::InvalidToken);
        }
        let kid = header
            .kid
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or(OidcResourceOwnerError::InvalidToken)?;
        let mut key = self.keys.lock().await.get(kid).cloned();
        if key.is_none() {
            let refreshed = fetch_keys(&self.client, &self.jwks_uri, &self.algorithms).await?;
            key = refreshed.get(kid).cloned();
            *self.keys.lock().await = refreshed;
        }
        let key = key.ok_or(OidcResourceOwnerError::InvalidToken)?;
        if key
            .common
            .key_algorithm
            .is_some_and(|v| v.to_string() != name)
        {
            return Err(OidcResourceOwnerError::InvalidToken);
        }
        let mut validation = Validation::new(header.alg);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        validation.required_spec_claims.clear();
        let value = decode::<serde_json::Value>(
            token,
            &DecodingKey::from_jwk(&key).map_err(|_| OidcResourceOwnerError::InvalidToken)?,
            &validation,
        )
        .map_err(|_| OidcResourceOwnerError::InvalidToken)?
        .claims;
        validate_claims(&value, config, nonce, access, header.alg, now_ms)
    }
}

struct Tokens {
    access_token: Zeroizing<String>,
    id_token: Zeroizing<String>,
    token_type: String,
}

#[derive(Deserialize)]
struct RawTokens {
    access_token: String,
    id_token: String,
    token_type: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tokens([redacted])")
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_claims(
    value: &serde_json::Value,
    config: &OidcResourceOwnerConfig,
    nonce: &str,
    access: &str,
    algorithm: Algorithm,
    now_ms: i64,
) -> std::result::Result<OidcVerifiedIdentity, OidcResourceOwnerError> {
    let claims = value
        .as_object()
        .cloned()
        .ok_or(OidcResourceOwnerError::InvalidToken)?;
    let text = |name: &str| {
        claims
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
    };
    let number = |name: &str| claims.get(name).and_then(serde_json::Value::as_u64);
    let issuer = text("iss").ok_or(OidcResourceOwnerError::InvalidToken)?;
    let subject = text("sub").ok_or(OidcResourceOwnerError::InvalidToken)?;
    let audiences = match claims.get("aud") {
        Some(serde_json::Value::String(v)) if !v.is_empty() => vec![v.clone()],
        Some(serde_json::Value::Array(v)) if !v.is_empty() => v
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|v| !v.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or(OidcResourceOwnerError::InvalidToken)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
        _ => return Err(OidcResourceOwnerError::InvalidToken),
    };
    let exp = number("exp").ok_or(OidcResourceOwnerError::InvalidToken)?;
    let iat = number("iat").ok_or(OidcResourceOwnerError::InvalidToken)?;
    let now = u64::try_from(now_ms / 1000).map_err(|_| OidcResourceOwnerError::InvalidToken)?;
    if issuer != config.issuer
        || !audiences.iter().any(|v| v == &config.client_id)
        || claims.get("nonce").and_then(serde_json::Value::as_str) != Some(nonce)
        || exp.saturating_add(CLOCK_SKEW_SECONDS) <= now
        || iat > now.saturating_add(CLOCK_SKEW_SECONDS)
        || now.saturating_sub(iat) > MAX_TOKEN_AGE_SECONDS + CLOCK_SKEW_SECONDS
        || number("nbf").is_some_and(|nbf| nbf > now.saturating_add(CLOCK_SKEW_SECONDS))
        || (audiences.len() > 1 && text("azp") != Some(config.client_id.as_str()))
        || text("azp").is_some_and(|azp| azp != config.client_id)
    {
        return Err(OidcResourceOwnerError::InvalidToken);
    }
    if let Some(expected) = text("at_hash") {
        let actual = access_hash(access, algorithm)?;
        if !constant_time(expected.as_bytes(), actual.as_bytes()) {
            return Err(OidcResourceOwnerError::InvalidToken);
        }
    }
    Ok(OidcVerifiedIdentity {
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
        audiences,
        claims,
    })
}

async fn fetch_keys(
    client: &reqwest::Client,
    uri: &Url,
    algorithms: &BTreeSet<String>,
) -> std::result::Result<HashMap<String, Jwk>, OidcResourceOwnerError> {
    let response = client
        .get(uri.clone())
        .send()
        .await
        .map_err(|_| OidcResourceOwnerError::Unavailable)?;
    if !response.status().is_success() {
        return Err(OidcResourceOwnerError::Provider);
    }
    let set: JwkSet =
        super::super::discovery::parse_unique_json(&bounded(response, JWKS_LIMIT).await?)
            .map_err(|_| OidcResourceOwnerError::Provider)?;
    if set.keys.is_empty() || set.keys.len() > MAX_KEYS {
        return Err(OidcResourceOwnerError::Provider);
    }
    let mut result = HashMap::new();
    for key in set.keys {
        let Some(kid) = key.common.key_id.clone().filter(|v| !v.is_empty()) else {
            continue;
        };
        let usable = key
            .common
            .public_key_use
            .as_ref()
            .is_none_or(|v| v == &PublicKeyUse::Signature)
            && key
                .common
                .key_operations
                .as_ref()
                .is_none_or(|v| v.contains(&KeyOperations::Verify))
            && !matches!(key.algorithm, AlgorithmParameters::OctetKey(_))
            && key
                .common
                .key_algorithm
                .is_none_or(|v| algorithms.contains(&v.to_string()))
            && DecodingKey::from_jwk(&key).is_ok();
        if usable && result.insert(kid, key).is_some() {
            return Err(OidcResourceOwnerError::Provider);
        }
    }
    if result.is_empty() {
        return Err(OidcResourceOwnerError::Provider);
    }
    Ok(result)
}

async fn bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, OidcResourceOwnerError> {
    if response.content_length().is_some_and(|v| v > limit as u64) {
        return Err(OidcResourceOwnerError::Provider);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| OidcResourceOwnerError::Unavailable)?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(OidcResourceOwnerError::Provider);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_attempt_cookie(
    config: &OidcResourceOwnerConfig,
    attempt: &OidcResourceOwnerAttempt,
    headers: &HeaderMap,
) -> std::result::Result<(), ()> {
    let value = strict_cookie(headers, &cookie_name(config, &attempt.transaction_id))?.ok_or(())?;
    constant_time(&digest(&value), &attempt.browser_correlation_digest)
        .then_some(())
        .ok_or(())
}

fn strict_cookie(headers: &HeaderMap, name: &str) -> std::result::Result<Option<String>, ()> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let raw = header.to_str().map_err(|_| ())?;
        if raw.chars().any(char::is_control) {
            return Err(());
        }
        for pair in raw.split(';') {
            let (candidate, value) = pair.trim().split_once('=').ok_or(())?;
            if candidate == name && (value.is_empty() || found.replace(value.to_owned()).is_some())
            {
                return Err(());
            }
        }
    }
    Ok(found)
}

fn cookie_name(config: &OidcResourceOwnerConfig, transaction_id: &str) -> String {
    let prefix = if config.endpoint_policy == OidcEndpointPolicy::HttpsOnly {
        "__Host-mcp-oidc-"
    } else {
        "mcp-oidc-"
    };
    format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(digest(transaction_id))[..22].to_owned()
    )
}

fn correlation_cookie(
    config: &OidcResourceOwnerConfig,
    transaction_id: &str,
    value: &str,
    clear: bool,
) -> std::result::Result<String, ()> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(());
    }
    let seconds = if clear {
        0
    } else {
        i64::try_from(config.attempt_lifetime.as_secs()).map_err(|_| ())?
    };
    Ok(
        Cookie::build((cookie_name(config, transaction_id), value.to_owned()))
            .http_only(true)
            .secure(config.endpoint_policy == OidcEndpointPolicy::HttpsOnly)
            .same_site(SameSite::Lax)
            .path("/")
            .max_age(time::Duration::seconds(seconds))
            .build()
            .to_string(),
    )
}

fn strict_query(raw: &str, allowed: &[&str]) -> std::result::Result<HashMap<String, String>, ()> {
    if raw.len() > 8192 {
        return Err(());
    }
    let mut values = HashMap::new();
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if !allowed.contains(&name.as_ref())
            || name.is_empty()
            || value.is_empty()
            || value.len() > 4096
            || name.chars().chain(value.chars()).any(char::is_control)
            || values
                .insert(name.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(());
        }
    }
    Ok(values)
}

fn redirect(location: &str, cookie: Option<String>) -> Response {
    let Ok(location) = HeaderValue::from_str(location) else {
        return unavailable();
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if let Some(cookie) = cookie.and_then(|v| HeaderValue::from_str(&v).ok()) {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}
fn bad_request() -> Response {
    (StatusCode::BAD_REQUEST, "invalid OIDC request").into_response()
}
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "OIDC authentication unavailable",
    )
        .into_response()
}

fn clean(_name: &str, value: String) -> std::result::Result<String, OidcResourceOwnerError> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        Err(OidcResourceOwnerError::Configuration(
            "OIDC setting is invalid",
        ))
    } else {
        Ok(value)
    }
}
fn endpoint(
    value: &str,
    policy: OidcEndpointPolicy,
) -> std::result::Result<Url, OidcResourceOwnerError> {
    let url = Url::parse(value)
        .map_err(|_| OidcResourceOwnerError::Configuration("OIDC endpoint is invalid"))?;
    if !policy.allows(&url)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(OidcResourceOwnerError::Configuration(
            "OIDC endpoint is unsafe",
        ));
    }
    Ok(url)
}
fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|v| v.is_loopback())
}
fn discovery_urls(issuer: &Url) -> std::result::Result<Vec<Url>, OidcResourceOwnerError> {
    let primary = Url::parse(&format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    ))
    .map_err(|_| OidcResourceOwnerError::Discovery)?;
    let suffix = if issuer.path() == "/" {
        ""
    } else {
        issuer.path()
    };
    let fallback = Url::parse(&format!(
        "{}/.well-known/openid-configuration{suffix}",
        &issuer[..Position::BeforePath]
    ))
    .map_err(|_| OidcResourceOwnerError::Discovery)?;
    Ok(if primary == fallback {
        vec![primary]
    } else {
        vec![primary, fallback]
    })
}
fn random(entropy: &dyn McpOAuthEntropy, bytes: usize) -> Result<String> {
    let mut value = vec![0; bytes];
    entropy.fill_bytes(&mut value)?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}
fn digest(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}
fn constant_time(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0_u8, |d, (l, r)| d | (l ^ r)) == 0
}
fn now_ms(now: SystemTime) -> OidcStoreResult<i64> {
    i64::try_from(
        now.duration_since(UNIX_EPOCH)
            .map_err(|_| OidcStoreError::InvalidData)?
            .as_millis(),
    )
    .map_err(|_| OidcStoreError::InvalidData)
}
fn add_ms(now: i64, duration: Duration) -> OidcStoreResult<i64> {
    now.checked_add(i64::try_from(duration.as_millis()).map_err(|_| OidcStoreError::InvalidData)?)
        .ok_or(OidcStoreError::InvalidData)
}
fn parse_algorithm(value: &str) -> Option<Algorithm> {
    ALGORITHMS
        .iter()
        .copied()
        .find(|v| algorithm_name(*v) == Some(value))
}
const fn algorithm_name(value: Algorithm) -> Option<&'static str> {
    Some(match value {
        Algorithm::RS256 => "RS256",
        Algorithm::RS384 => "RS384",
        Algorithm::RS512 => "RS512",
        Algorithm::PS256 => "PS256",
        Algorithm::PS384 => "PS384",
        Algorithm::PS512 => "PS512",
        Algorithm::ES256 => "ES256",
        Algorithm::ES384 => "ES384",
        Algorithm::EdDSA => "EdDSA",
        _ => return None,
    })
}
fn access_hash(
    access: &str,
    algorithm: Algorithm,
) -> std::result::Result<String, OidcResourceOwnerError> {
    let bytes = match algorithm {
        Algorithm::RS256 | Algorithm::PS256 | Algorithm::ES256 => {
            Sha256::digest(access.as_bytes()).to_vec()
        }
        Algorithm::RS384 | Algorithm::PS384 | Algorithm::ES384 => {
            Sha384::digest(access.as_bytes()).to_vec()
        }
        Algorithm::RS512 | Algorithm::PS512 => Sha512::digest(access.as_bytes()).to_vec(),
        _ => return Err(OidcResourceOwnerError::InvalidToken),
    };
    Ok(URL_SAFE_NO_PAD.encode(&bytes[..bytes.len() / 2]))
}

fn strict_jwt_json(
    token: &str,
    index: usize,
) -> std::result::Result<serde_json::Value, OidcResourceOwnerError> {
    let segment = token
        .split('.')
        .nth(index)
        .filter(|value| !value.is_empty())
        .ok_or(OidcResourceOwnerError::InvalidToken)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| OidcResourceOwnerError::InvalidToken)?;
    super::super::discovery::parse_unique_json::<serde_json::Value>(&bytes)
        .map_err(|_| OidcResourceOwnerError::InvalidToken)
}

/// Sanitized OIDC configuration, protocol, or provider failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OidcResourceOwnerError {
    /// Invalid local configuration.
    Configuration(&'static str),
    /// Discovery metadata is invalid or unsafe.
    Discovery,
    /// Browser input is malformed.
    InvalidRequest,
    /// Provider response is invalid.
    Provider,
    /// ID token validation failed.
    InvalidToken,
    /// Required infrastructure is unavailable.
    Unavailable,
}

impl fmt::Display for OidcResourceOwnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Configuration(v) => v,
            Self::Discovery => "OIDC discovery failed",
            Self::InvalidRequest => "OIDC request is invalid",
            Self::Provider => "OIDC provider response is invalid",
            Self::InvalidToken => "OIDC token is invalid",
            Self::Unavailable => "OIDC authentication is unavailable",
        })
    }
}
impl std::error::Error for OidcResourceOwnerError {}
