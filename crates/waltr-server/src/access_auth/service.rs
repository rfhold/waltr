use std::{
    collections::HashMap,
    error::Error,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use axum::http::{HeaderMap, header};

use super::{
    AccessAuthConfig, AccessAuthHttpClient, AccessAuthMode, AccessAuthStore, AccessAuthStoreError,
    AuthenticatedPrincipal, AuthenticationMethod, BrowserSessionTokenDigest,
    DiscoveredOidcProvider, OidcConfig, OidcError, OidcVerifier,
    lease::{RevocationRegistry, RevocationSignal},
};
use crate::persistence::Persistence;

const DISABLED_ISSUER: &str = "waltr:disabled";
const DISABLED_SUBJECT: &str = "local";
pub(super) const GLOBAL_LOGIN_RATE_LIMIT: u32 = 240;
pub(super) const SOURCE_LOGIN_RATE_LIMIT: u32 = 20;
pub(super) const LOGIN_RATE_WINDOW: Duration = Duration::from_mins(1);
pub(super) const LOGIN_SOURCE_BUCKET_RETENTION: Duration = Duration::from_mins(2);
pub(super) const MAX_LOGIN_SOURCE_BUCKETS: usize = 256;

/// Cloneable server-owned access-authentication service.
#[derive(Clone)]
pub struct AccessAuthenticator {
    inner: Arc<AccessAuthenticatorInner>,
}

impl fmt::Debug for AccessAuthenticator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessAuthenticator")
            .field("enabled", &self.enabled())
            .finish_non_exhaustive()
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "the disabled and initialized authenticator states share one handle"
)]
enum AccessAuthenticatorInner {
    Disabled,
    Oidc(OidcAccessAuthenticator),
}

pub(super) struct OidcAccessAuthenticator {
    config: OidcConfig,
    store: AccessAuthStore,
    provider: DiscoveredOidcProvider,
    api_verifier: OidcVerifier,
    login_rate_limiter: Mutex<LoginRateLimiter>,
    revocations: RevocationRegistry,
}

impl AccessAuthenticator {
    /// Initialize access authentication, including OIDC discovery and initial JWKS loading.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration, discovery, persistence, or resource setup fails.
    pub async fn initialize(
        config: AccessAuthConfig,
        persistence: &Persistence,
        client: AccessAuthHttpClient,
    ) -> Result<Self, AccessAuthError> {
        if config.mode() == AccessAuthMode::Disabled {
            return Ok(Self::disabled());
        }
        let oidc_config = config
            .oidc_config()
            .cloned()
            .ok_or(AccessAuthError::Configuration(
                "OIDC mode has no OIDC configuration",
            ))?;
        let provider = DiscoveredOidcProvider::discover(client.clone(), oidc_config.clone())
            .await
            .map_err(|error| {
                tracing::error!(%error, "OIDC provider discovery failed");
                AccessAuthError::from_oidc(error)
            })?;
        let browser_verifier = provider.verifier();
        let api_verifier = if oidc_config.api_issuer() == oidc_config.issuer() {
            browser_verifier
                .for_access_audience(oidc_config.api_audience(), oidc_config.api_client_id())
        } else {
            OidcVerifier::discover_access(
                client.clone(),
                oidc_config.api_issuer(),
                oidc_config.api_audience(),
                oidc_config.api_client_id(),
                oidc_config.transport_policy(),
            )
            .await
            .map_err(AccessAuthError::from_oidc)?
        };
        let store = AccessAuthStore::new(persistence);
        store
            .revoke_browser_sessions_outside_configuration(
                oidc_config.issuer(),
                oidc_config.client_id(),
                SystemTime::now(),
            )
            .await
            .map_err(AccessAuthError::persistence)?;
        Ok(Self {
            inner: Arc::new(AccessAuthenticatorInner::Oidc(OidcAccessAuthenticator {
                config: oidc_config,
                store,
                provider,
                api_verifier,
                login_rate_limiter: Mutex::new(LoginRateLimiter::new(Instant::now())),
                revocations: RevocationRegistry::default(),
            })),
        })
    }

    /// Construct an explicitly disabled access authenticator.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(AccessAuthenticatorInner::Disabled),
        }
    }

    /// Return whether OIDC authentication is enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        matches!(&*self.inner, AccessAuthenticatorInner::Oidc(_))
    }

    /// Return the public API protected-resource identifier.
    #[must_use]
    pub fn api_resource_url(&self) -> Option<&str> {
        self.oidc().map(|oidc| oidc.config.public_url())
    }

    /// Return the API authorization server.
    #[must_use]
    pub fn api_authorization_server_url(&self) -> Option<&str> {
        self.oidc().map(|oidc| oidc.config.api_issuer())
    }

    /// Return scopes required for native API bearer access.
    #[must_use]
    pub fn api_required_scopes(&self) -> &[String] {
        self.oidc()
            .map_or(&[], |oidc| oidc.config.api_required_scopes())
    }

    pub(super) fn api_native_client(&self) -> Option<super::handlers::ApiNativeClientMetadata<'_>> {
        let config = self.oidc()?.config();
        let client_id = config.api_client_id()?;
        let mut scopes = vec!["openid".to_owned(), "offline_access".to_owned()];
        for scope in config.api_required_scopes() {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        Some(super::handlers::ApiNativeClientMetadata {
            client_id,
            scopes,
            audience: Some(config.api_audience()),
        })
    }

    /// Validate a plaintext bearer token against the configured issuer and scopes.
    ///
    /// # Errors
    ///
    /// Returns an authentication failure when token validation or key loading fails.
    pub async fn validate_api_access_token(
        &self,
        token: &str,
        required_scopes: &[String],
    ) -> Result<AuthenticatedPrincipal, RequestAuthFailure> {
        match &*self.inner {
            AccessAuthenticatorInner::Disabled => Ok(disabled_principal()),
            AccessAuthenticatorInner::Oidc(oidc) => oidc
                .api_verifier
                .validate_access_token(token, required_scopes)
                .await
                .map_err(RequestAuthFailure::from_oidc),
        }
    }

    /// Authenticate an HTTP bearer header without consulting browser cookies.
    ///
    /// # Errors
    ///
    /// Returns an authentication failure when the header or bearer token is invalid.
    pub async fn authenticate_bearer_headers(
        &self,
        headers: &HeaderMap,
        required_scopes: &[String],
    ) -> Result<AuthenticatedPrincipal, RequestAuthFailure> {
        if !self.enabled() {
            return Ok(disabled_principal());
        }
        let token = bearer_token(headers)?;
        self.validate_api_access_token(token, required_scopes).await
    }

    /// Authenticate browser headers using only a persisted browser session cookie.
    ///
    /// # Errors
    ///
    /// Returns an authentication failure when credentials are absent, invalid, expired, or revoked.
    pub async fn authenticate_browser_headers(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedPrincipal, RequestAuthFailure> {
        if !self.enabled() {
            return Ok(disabled_principal());
        }
        let token = session_cookie(headers, self.session_cookie_name())?;
        self.authenticate_browser_session_token(token).await
    }

    /// Authenticate a persisted browser session cookie without accepting bearer credentials.
    ///
    /// # Errors
    ///
    /// Returns an authentication failure when the session cookie is absent, invalid, expired, or revoked.
    pub async fn authenticate_browser_session_headers(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedPrincipal, RequestAuthFailure> {
        if !self.enabled() {
            return Ok(disabled_principal());
        }
        let token = session_cookie(headers, self.session_cookie_name())?;
        self.authenticate_browser_session_token(token).await
    }

    async fn authenticate_browser_session_token(
        &self,
        token: &str,
    ) -> Result<AuthenticatedPrincipal, RequestAuthFailure> {
        let oidc = self.oidc().expect("enabled authentication is OIDC");
        let token_digest = BrowserSessionTokenDigest::from_token(token);
        let (revocation_signal, session) = register_before_lookup(
            &oidc.revocations,
            &token_digest,
            oidc.store.lookup_browser_session(
                &token_digest,
                oidc.config.issuer(),
                oidc.config.client_id(),
                SystemTime::now(),
            ),
        )
        .await
        .map_err(RequestAuthFailure::store)?;
        let session = session.ok_or(RequestAuthFailure::Unauthenticated)?;
        Ok(AuthenticatedPrincipal::browser(
            session.issuer,
            session.client_id,
            session.subject,
            token_digest,
            session.expires_at,
            oidc.store.clone(),
            revocation_signal,
        ))
    }

    pub(super) async fn revoke_browser_session_token(
        &self,
        token: &str,
        now: SystemTime,
    ) -> Result<(), AccessAuthStoreError> {
        let Some(oidc) = self.oidc() else {
            return Ok(());
        };
        let token_digest = BrowserSessionTokenDigest::from_token(token);
        commit_then_cancel(
            &oidc.revocations,
            &token_digest,
            oidc.store.revoke_browser_session(&token_digest, now),
        )
        .await
    }

    pub(super) fn oidc(&self) -> Option<&OidcAccessAuthenticator> {
        match &*self.inner {
            AccessAuthenticatorInner::Disabled => None,
            AccessAuthenticatorInner::Oidc(oidc) => Some(oidc),
        }
    }

    pub(super) fn session_cookie_name(&self) -> &'static str {
        if self
            .oidc()
            .is_some_and(|oidc| oidc.config.allow_insecure_cookie())
        {
            "waltr-session-local"
        } else {
            "__Host-waltr-session"
        }
    }

    pub(super) fn login_cookie_name(&self) -> &'static str {
        if self
            .oidc()
            .is_some_and(|oidc| oidc.config.allow_insecure_cookie())
        {
            "waltr-login-local"
        } else {
            "__Host-waltr-login"
        }
    }

    /// Return whether this deployment explicitly permits non-Secure local cookies.
    #[must_use]
    pub fn allow_insecure_cookie(&self) -> bool {
        self.oidc()
            .is_some_and(|oidc| oidc.config.allow_insecure_cookie())
    }
}

async fn register_before_lookup<T, E>(
    registry: &RevocationRegistry,
    token_digest: &BrowserSessionTokenDigest,
    lookup: impl Future<Output = Result<T, E>>,
) -> Result<(Arc<RevocationSignal>, T), E> {
    let signal = registry.signal(token_digest);
    lookup.await.map(|result| (signal, result))
}

async fn commit_then_cancel<E>(
    registry: &RevocationRegistry,
    token_digest: &BrowserSessionTokenDigest,
    commit: impl Future<Output = Result<(), E>>,
) -> Result<(), E> {
    commit.await?;
    registry.cancel(token_digest);
    Ok(())
}

impl OidcAccessAuthenticator {
    pub(super) const fn config(&self) -> &OidcConfig {
        &self.config
    }

    pub(super) const fn store(&self) -> &AccessAuthStore {
        &self.store
    }

    pub(super) const fn provider(&self) -> &DiscoveredOidcProvider {
        &self.provider
    }

    pub(super) fn verifier(&self) -> OidcVerifier {
        self.provider.verifier()
    }

    pub(super) fn admit_login(&self, source: Option<IpAddr>) -> bool {
        self.login_rate_limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admit(source, Instant::now())
    }

    pub(super) fn login_source(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Option<IpAddr> {
        derive_login_source(peer, headers, |address| {
            self.config.is_trusted_proxy(address)
        })
    }
}

fn derive_login_source(
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    is_trusted: impl Fn(IpAddr) -> bool,
) -> Option<IpAddr> {
    let peer = peer?;
    if !is_trusted(peer) {
        return Some(peer);
    }
    let mut forwarded = headers.get_all("x-forwarded-for").iter();
    let Some(value) = forwarded.next() else {
        return Some(peer);
    };
    if forwarded.next().is_some() {
        return Some(peer);
    }
    let Ok(value) = value.to_str() else {
        return Some(peer);
    };
    let Some(hops) = value
        .split(',')
        .map(str::trim)
        .map(parse_forwarded_hop)
        .collect::<Option<Vec<_>>>()
        .filter(|hops| !hops.is_empty())
    else {
        return Some(peer);
    };
    for hop in hops.into_iter().rev() {
        if !is_trusted(hop) {
            return Some(hop);
        }
    }
    Some(peer)
}

fn parse_forwarded_hop(value: &str) -> Option<IpAddr> {
    if value.is_empty() {
        return None;
    }
    if let Some(bracketed) = value.strip_prefix('[') {
        let (address, suffix) = bracketed.split_once(']')?;
        let address = address.parse::<Ipv6Addr>().ok()?;
        if !suffix.is_empty() {
            suffix.strip_prefix(':')?.parse::<u16>().ok()?;
        }
        return Some(IpAddr::V6(address));
    }
    if let Ok(address) = value.parse::<Ipv4Addr>() {
        return Some(IpAddr::V4(address));
    }
    if let Ok(address) = value.parse::<Ipv6Addr>() {
        return Some(IpAddr::V6(address));
    }
    match value.parse::<SocketAddr>().ok()? {
        SocketAddr::V4(address) => Some(IpAddr::V4(*address.ip())),
        SocketAddr::V6(_) => None,
    }
}

struct LoginRateLimiter {
    global: LoginRateWindow,
    sources: HashMap<IpAddr, LoginRateWindow>,
    unknown_source: LoginRateWindow,
}

#[derive(Clone, Copy)]
struct LoginRateWindow {
    started: Instant,
    last_seen: Instant,
    admitted: u32,
}

impl LoginRateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            global: LoginRateWindow::new(now),
            sources: HashMap::new(),
            unknown_source: LoginRateWindow::new(now),
        }
    }

    fn admit(&mut self, source: Option<IpAddr>, now: Instant) -> bool {
        self.global.reset_if_elapsed(now);
        if self.global.admitted >= GLOBAL_LOGIN_RATE_LIMIT {
            return false;
        }

        self.sources.retain(|_, window| {
            now.checked_duration_since(window.last_seen)
                .is_none_or(|elapsed| elapsed < LOGIN_SOURCE_BUCKET_RETENTION)
        });
        let source_window = if let Some(source) = source {
            if !self.sources.contains_key(&source) && self.sources.len() >= MAX_LOGIN_SOURCE_BUCKETS
            {
                let oldest = self
                    .sources
                    .iter()
                    .min_by_key(|(_, window)| window.last_seen)
                    .map(|(source, _)| *source)
                    .expect("a full source map is nonempty");
                self.sources.remove(&oldest);
            }
            self.sources
                .entry(source)
                .or_insert_with(|| LoginRateWindow::new(now))
        } else {
            &mut self.unknown_source
        };
        source_window.reset_if_elapsed(now);
        source_window.last_seen = now;
        if source_window.admitted >= SOURCE_LOGIN_RATE_LIMIT {
            return false;
        }
        source_window.admitted += 1;
        self.global.admitted += 1;
        self.global.last_seen = now;
        true
    }
}

impl LoginRateWindow {
    const fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_seen: now,
            admitted: 0,
        }
    }

    fn reset_if_elapsed(&mut self, now: Instant) {
        if now
            .checked_duration_since(self.started)
            .is_some_and(|elapsed| elapsed >= LOGIN_RATE_WINDOW)
        {
            self.started = now;
            self.admitted = 0;
        }
    }
}

fn disabled_principal() -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::new(
        DISABLED_ISSUER,
        DISABLED_SUBJECT,
        [],
        AuthenticationMethod::BrowserSession,
    )
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, RequestAuthFailure> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next().ok_or(RequestAuthFailure::Unauthenticated)?;
    if values.next().is_some() {
        return Err(RequestAuthFailure::Unauthenticated);
    }
    let value = value
        .to_str()
        .map_err(|_| RequestAuthFailure::Unauthenticated)?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or(RequestAuthFailure::Unauthenticated)?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.chars().any(char::is_whitespace)
    {
        return Err(RequestAuthFailure::Unauthenticated);
    }
    Ok(token)
}

fn session_cookie<'a>(
    headers: &'a HeaderMap,
    expected_name: &str,
) -> Result<&'a str, RequestAuthFailure> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value
            .to_str()
            .map_err(|_| RequestAuthFailure::Unauthenticated)?;
        for pair in value.split(';') {
            let (name, token) = pair
                .trim()
                .split_once('=')
                .ok_or(RequestAuthFailure::Unauthenticated)?;
            if name == expected_name && (token.is_empty() || found.replace(token).is_some()) {
                return Err(RequestAuthFailure::Unauthenticated);
            }
        }
    }
    found.ok_or(RequestAuthFailure::Unauthenticated)
}

/// Sanitized service initialization or browser-flow failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessAuthError {
    /// Local authentication configuration is inconsistent.
    Configuration(&'static str),
    /// OIDC discovery or initial key loading failed.
    Discovery(&'static str),
    /// The provider rejected or returned an invalid authorization response.
    Provider(&'static str),
    /// Authentication state could not be persisted.
    Persistence(&'static str),
    /// A request input or lifecycle transition is invalid.
    InvalidRequest(&'static str),
}

impl AccessAuthError {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "boundary errors are consumed and sanitized before exposure"
    )]
    pub(super) const fn from_oidc(error: OidcError) -> Self {
        match error {
            OidcError::Configuration(_) => Self::Configuration("OIDC configuration is invalid"),
            OidcError::Discovery(_) | OidcError::UnknownKeyId => {
                Self::Discovery("OIDC provider discovery or key loading failed")
            }
            OidcError::InvalidRequest(_) => Self::InvalidRequest("OIDC request is invalid"),
            OidcError::Provider(_) => Self::Provider("OIDC provider request failed"),
            OidcError::InvalidToken(_) | OidcError::InsufficientScope => {
                Self::Provider("OIDC token validation failed")
            }
        }
    }

    pub(super) const fn persistence(_: AccessAuthStoreError) -> Self {
        Self::Persistence("access authentication persistence failed")
    }
}

impl fmt::Display for AccessAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message)
            | Self::Discovery(message)
            | Self::Provider(message)
            | Self::Persistence(message)
            | Self::InvalidRequest(message) => formatter.write_str(message),
        }
    }
}

impl Error for AccessAuthError {}

/// Authentication result suitable for HTTP, gRPC, and MCP boundary mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestAuthFailure {
    /// No valid authentication evidence was supplied.
    Unauthenticated,
    /// A valid bearer token lacks a route-required scope.
    InsufficientScope,
    /// Issuer/JWKS or persistence infrastructure failed.
    Internal,
}

impl RequestAuthFailure {
    #[allow(
        clippy::needless_pass_by_value,
        reason = "boundary errors are consumed and classified before exposure"
    )]
    const fn from_oidc(error: OidcError) -> Self {
        match error {
            OidcError::InsufficientScope => Self::InsufficientScope,
            OidcError::Provider(_) | OidcError::Discovery(_) => Self::Internal,
            OidcError::Configuration(_)
            | OidcError::InvalidRequest(_)
            | OidcError::InvalidToken(_)
            | OidcError::UnknownKeyId => Self::Unauthenticated,
        }
    }

    const fn store(_: AccessAuthStoreError) -> Self {
        Self::Internal
    }
}

impl fmt::Display for RequestAuthFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated => formatter.write_str("authentication required"),
            Self::InsufficientScope => formatter.write_str("insufficient scope"),
            Self::Internal => formatter.write_str("authentication service unavailable"),
        }
    }
}

impl Error for RequestAuthFailure {}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    reason = "test network fixtures construct octets from bounded loop indices"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_signal_is_registered_before_lookup_can_complete() {
        let registry = Arc::new(RevocationRegistry::default());
        let digest = BrowserSessionTokenDigest::from_token("registration-race");
        let task_registry = Arc::clone(&registry);
        let lookup_registry = Arc::clone(&registry);
        let task_digest = digest.clone();
        let lookup_digest = digest.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let authentication = tokio::spawn(async move {
            register_before_lookup(task_registry.as_ref(), &task_digest, async move {
                let signal = lookup_registry
                    .live_signal(&lookup_digest)
                    .expect("signal must exist before lookup is polled");
                started_tx.send(signal).unwrap();
                release_rx.await.unwrap();
                Ok::<_, ()>("active session")
            })
            .await
        });

        let registered = started_rx.await.unwrap();
        registry.cancel(&digest);
        release_tx.send(()).unwrap();
        let (returned, session) = authentication.await.unwrap().unwrap();
        assert_eq!(session, "active session");
        assert!(Arc::ptr_eq(&registered, &returned));
        assert!(returned.is_cancelled());
    }

    #[tokio::test]
    async fn failed_durable_logout_does_not_cancel_local_signal() {
        let registry = RevocationRegistry::default();
        let digest = BrowserSessionTokenDigest::from_token("failed-logout");
        let signal = registry.signal(&digest);

        let result =
            commit_then_cancel(&registry, &digest, async { Err::<(), _>("store failed") }).await;
        assert_eq!(result, Err("store failed"));
        assert!(!signal.is_cancelled());

        commit_then_cancel(&registry, &digest, async { Ok::<_, &str>(()) })
            .await
            .unwrap();
        assert!(signal.is_cancelled());
    }

    #[tokio::test]
    async fn disabled_mode_returns_one_effective_principal() {
        let auth = AccessAuthenticator::disabled();
        let first = auth
            .authenticate_browser_headers(&HeaderMap::new())
            .await
            .unwrap();
        let second = auth
            .authenticate_bearer_headers(&HeaderMap::new(), &["scope".to_owned()])
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.subject(), DISABLED_SUBJECT);
        assert!(!auth.enabled());
    }

    #[test]
    fn bearer_parser_is_strict() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            bearer_token(&headers),
            Err(RequestAuthFailure::Unauthenticated)
        );
        headers.insert(header::AUTHORIZATION, "Bearer token.value".parse().unwrap());
        assert_eq!(bearer_token(&headers), Ok("token.value"));
        headers.insert(header::AUTHORIZATION, "Basic value".parse().unwrap());
        assert_eq!(
            bearer_token(&headers),
            Err(RequestAuthFailure::Unauthenticated)
        );
        headers.insert(header::AUTHORIZATION, "Bearer token value".parse().unwrap());
        assert_eq!(
            bearer_token(&headers),
            Err(RequestAuthFailure::Unauthenticated)
        );
    }

    #[test]
    fn browser_session_authentication_does_not_accept_bearer_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer api-token".parse().unwrap());
        headers.insert(header::COOKIE, "waltr-session-local=valid".parse().unwrap());
        assert_eq!(session_cookie(&headers, "waltr-session-local"), Ok("valid"));
    }

    #[test]
    fn one_source_does_not_consume_another_sources_allowance() {
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        let first = Some("192.0.2.1".parse().unwrap());
        let second = Some("192.0.2.2".parse().unwrap());
        for _ in 0..SOURCE_LOGIN_RATE_LIMIT {
            assert!(limiter.admit(first, start));
        }
        assert!(!limiter.admit(first, start));
        assert!(limiter.admit(second, start));
        assert!(limiter.admit(first, start + LOGIN_RATE_WINDOW));
    }

    #[test]
    fn unknown_source_has_its_own_bounded_allowance() {
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        for _ in 0..SOURCE_LOGIN_RATE_LIMIT {
            assert!(limiter.admit(None, start));
        }
        assert!(!limiter.admit(None, start));
        assert!(limiter.admit(Some("192.0.2.1".parse().unwrap()), start));
    }

    #[test]
    fn source_bucket_retention_is_bounded_and_cleans_stale_entries() {
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        for index in 0..GLOBAL_LOGIN_RATE_LIMIT * 2 {
            let source = IpAddr::from([10, (index >> 16) as u8, (index >> 8) as u8, index as u8]);
            let now = if index < GLOBAL_LOGIN_RATE_LIMIT {
                start
            } else {
                start + LOGIN_RATE_WINDOW
            };
            assert!(limiter.admit(Some(source), now));
        }
        assert_eq!(limiter.sources.len(), MAX_LOGIN_SOURCE_BUCKETS);
        assert!(limiter.admit(
            Some("192.0.2.1".parse().unwrap()),
            start + LOGIN_SOURCE_BUCKET_RETENTION + LOGIN_RATE_WINDOW,
        ));
        assert_eq!(limiter.sources.len(), 1);
    }

    #[test]
    fn global_login_protection_bounds_rotating_sources() {
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        for index in 0..GLOBAL_LOGIN_RATE_LIMIT {
            let source = IpAddr::from([10, 0, (index >> 8) as u8, index as u8]);
            assert!(limiter.admit(Some(source), start));
        }
        assert!(!limiter.admit(Some("192.0.2.1".parse().unwrap()), start));
    }

    #[test]
    fn trusted_proxy_chain_uses_nearest_untrusted_hop_and_shared_rate_bucket() {
        let trusted = trusted_test_proxy;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "192.0.2.10:49152, 198.51.100.20, 10.0.0.2:443"
                .parse()
                .unwrap(),
        );
        let source = derive_login_source(Some("10.0.0.1".parse().unwrap()), &headers, trusted);
        assert_eq!(source, Some("198.51.100.20".parse().unwrap()));
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        for _ in 0..SOURCE_LOGIN_RATE_LIMIT {
            assert!(limiter.admit(source, start));
        }
        assert!(!limiter.admit(source, start));

        assert_eq!(
            derive_login_source(Some("203.0.113.1".parse().unwrap()), &headers, trusted),
            Some("203.0.113.1".parse().unwrap()),
            "an untrusted peer cannot spoof X-Forwarded-For"
        );
    }

    #[test]
    fn forwarded_hop_parser_accepts_only_unambiguous_address_forms() {
        for (value, expected) in [
            ("192.0.2.1", "192.0.2.1"),
            ("192.0.2.1:443", "192.0.2.1"),
            ("[2001:db8::1]", "2001:db8::1"),
            ("[2001:db8::1]:443", "2001:db8::1"),
            ("2001:db8::1", "2001:db8::1"),
            ("2001:db8::1:443", "2001:db8::1:443"),
        ] {
            assert_eq!(parse_forwarded_hop(value), Some(expected.parse().unwrap()));
        }
        for value in [
            "",
            "invalid",
            "[2001:db8::1",
            "[2001:db8::1]extra",
            "[192.0.2.1]:443",
            "192.0.2.1:invalid",
        ] {
            assert_eq!(parse_forwarded_hop(value), None, "{value}");
        }
    }

    #[test]
    fn malformed_empty_duplicate_and_all_trusted_chains_share_peer_bucket() {
        let peer = "10.0.0.1".parse().unwrap();
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        let mut variants = Vec::new();
        for value in [
            "",
            "   ",
            "192.0.2.1, invalid",
            "[2001:db8::1]bad",
            "10.0.0.2, 10.0.0.3",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", value.parse().unwrap());
            variants.push(headers);
        }
        let mut duplicate = HeaderMap::new();
        duplicate.append("x-forwarded-for", "192.0.2.1".parse().unwrap());
        duplicate.append("x-forwarded-for", "198.51.100.1".parse().unwrap());
        variants.push(duplicate);

        for index in 0..SOURCE_LOGIN_RATE_LIMIT {
            let source = derive_login_source(
                Some(peer),
                &variants[index as usize % variants.len()],
                trusted_test_proxy,
            );
            assert_eq!(source, Some(peer));
            assert!(limiter.admit(source, start));
        }
        for headers in variants {
            let source = derive_login_source(Some(peer), &headers, trusted_test_proxy);
            assert_eq!(source, Some(peer));
            assert!(!limiter.admit(source, start));
        }
    }

    #[test]
    fn bracketed_ipv6_with_port_selects_one_stable_source_bucket() {
        let peer = "10.0.0.1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "[2001:db8::10]:49152, 10.0.0.2".parse().unwrap(),
        );
        let source = derive_login_source(Some(peer), &headers, trusted_test_proxy);
        assert_eq!(source, Some("2001:db8::10".parse().unwrap()));
        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        for _ in 0..SOURCE_LOGIN_RATE_LIMIT {
            assert!(limiter.admit(source, start));
        }
        assert!(!limiter.admit(source, start));
    }

    #[test]
    fn distinct_bare_ipv6_clients_receive_distinct_source_buckets() {
        let peer = "10.0.0.1".parse().unwrap();
        let mut first_headers = HeaderMap::new();
        first_headers.insert("x-forwarded-for", "2001:db8::10, 10.0.0.2".parse().unwrap());
        let mut second_headers = HeaderMap::new();
        second_headers.insert("x-forwarded-for", "2001:db8::11, 10.0.0.2".parse().unwrap());
        let first = derive_login_source(Some(peer), &first_headers, trusted_test_proxy);
        let second = derive_login_source(Some(peer), &second_headers, trusted_test_proxy);
        assert_eq!(first, Some("2001:db8::10".parse().unwrap()));
        assert_eq!(second, Some("2001:db8::11".parse().unwrap()));

        let start = Instant::now();
        let mut limiter = LoginRateLimiter::new(start);
        for _ in 0..SOURCE_LOGIN_RATE_LIMIT {
            assert!(limiter.admit(first, start));
        }
        assert!(!limiter.admit(first, start));
        assert!(limiter.admit(second, start));
    }

    fn trusted_test_proxy(address: IpAddr) -> bool {
        matches!(address, IpAddr::V4(address) if address.octets()[0..2] == [10, 0])
    }
}
