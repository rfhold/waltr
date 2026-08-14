//! Waltr composition for the hosted MCP OAuth authorization server.

use std::{
    collections::{BTreeSet, HashMap},
    env,
    error::Error,
    fmt::{self, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie::{Cookie, SameSite, time};
use mcp::{
    McpOAuthSecret,
    oauth::{
        HardenedOAuthClientMetadataFetcher, McpOAuthEntropy, McpSystemOAuthClock,
        OAuthAuthorizationServer, OAuthAuthorizationServerConfig, OAuthAuthorizationStore,
        OAuthAuthorizationStoreConfig, OAuthClientRegistrationOptions,
        OAuthConsentDecisionEvidence, OAuthConsentHandler, OAuthConsentModel,
        OAuthConsentPresentation, OAuthKeyEncryption, OAuthResource,
        OAuthResourceOwnerAuthentication, OAuthResourceOwnerAuthenticationRequest,
        OAuthResourceOwnerAuthenticator, OAuthSigningKeyState, PostgresOAuthAuthorizationStore,
        PrivateNetworkOAuthCimdDestinationPolicy, SqliteOAuthAuthorizationStore,
        TrustedPrivateOAuthCimdDestinationPolicy, VersionedOAuthWrappingKeyring,
    },
    server::BoxFuture,
};
use p256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint as _, pkcs8::DecodePrivateKey as _};
use rand::RngCore as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{
    access_auth::{
        AccessAuthConfig, AccessAuthMode, AccessAuthStore, AccessAuthenticator,
        LoginCorrelationDigest, McpOAuthLoginContinuation, RequestAuthFailure,
    },
    persistence::Persistence,
};

const DEFAULT_ACCESS_TOKEN_TTL: Duration = Duration::from_mins(5);
const DEFAULT_REFRESH_TOKEN_TTL: Duration = Duration::from_hours(24);
const DEFAULT_REFRESH_FAMILY_TTL: Duration = Duration::from_hours(24 * 30);
const DEFAULT_CODE_TTL: Duration = Duration::from_mins(5);
const AUTHORIZATION_TRANSACTION_TTL: Duration = Duration::from_mins(10);
const OAUTH_CLEANUP_INTERVAL: Duration = Duration::from_mins(5);
const SIGNING_ROTATION_INTERVAL: Duration = Duration::from_hours(24);
const MAINTENANCE_BATCH_SIZE: usize = 256;
const MAX_REVOCATION_TERMINATION_BOUND: Duration = Duration::from_secs(5);

/// Validated local MCP authorization-server configuration.
#[allow(
    clippy::large_enum_variant,
    reason = "configuration is constructed once at startup and the direct variant keeps composition simple"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpOAuthConfig {
    /// Access authentication is disabled, so hosted OAuth is not exposed.
    Disabled,
    /// Hosted OAuth settings for an OIDC-authenticated deployment.
    Enabled(McpOAuthEnabledConfig),
}

/// Validated enabled configuration.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent deployment policy switches are validated together at startup"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthEnabledConfig {
    issuer: String,
    resource: String,
    public_url: String,
    required_scopes: Vec<String>,
    wrapping_keys_file: PathBuf,
    access_token_ttl: Duration,
    refresh_token_ttl: Duration,
    refresh_family_ttl: Duration,
    code_ttl: Duration,
    allow_dcr: bool,
    allow_loopback_redirects: bool,
    allow_cimd: bool,
    cimd_trusted_private_origins: Vec<Url>,
    allow_private_test_destinations: bool,
}

impl McpOAuthConfig {
    /// Read and validate local MCP OAuth settings independently from upstream OIDC domains.
    ///
    /// # Errors
    ///
    /// Returns an error for partial disabled-mode settings or invalid enabled settings.
    pub fn from_env(access: &AccessAuthConfig) -> Result<Self, McpOAuthConfigError> {
        let names = [
            "WALTR_MCP_OAUTH_ISSUER",
            "WALTR_MCP_REQUIRED_SCOPES",
            "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE",
            "WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL",
            "WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL",
            "WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL",
            "WALTR_MCP_OAUTH_CODE_TTL",
            "WALTR_MCP_OAUTH_ALLOW_DCR",
            "WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS",
            "WALTR_MCP_OAUTH_ALLOW_CIMD",
            "WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS",
            "WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS",
        ];
        let mut values = HashMap::new();
        for name in names {
            match env::var(name) {
                Ok(value) => {
                    values.insert(name, value);
                }
                Err(env::VarError::NotPresent) => {}
                Err(env::VarError::NotUnicode(_)) => {
                    return Err(McpOAuthConfigError::Invalid(format!(
                        "{name} must contain valid UTF-8"
                    )));
                }
            }
        }
        Self::from_values(access, &values)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "hosted OAuth validation keeps interdependent deployment settings together"
    )]
    fn from_values(
        access: &AccessAuthConfig,
        values: &HashMap<&str, String>,
    ) -> Result<Self, McpOAuthConfigError> {
        if access.mode() == AccessAuthMode::Disabled {
            if values.values().any(|value| !value.is_empty()) {
                return Err(McpOAuthConfigError::Invalid(
                    "MCP OAuth settings are present while WALTR_AUTH_MODE is disabled".to_owned(),
                ));
            }
            return Ok(Self::Disabled);
        }
        let oidc = access.oidc_config().ok_or_else(|| {
            McpOAuthConfigError::Invalid(
                "OIDC access authentication must be initialized before MCP OAuth".to_owned(),
            )
        })?;
        let public_url = oidc.public_url().to_owned();
        let issuer = value(values, "WALTR_MCP_OAUTH_ISSUER")
            .map_or_else(|| append_path(&public_url, "oauth"), ToOwned::to_owned);
        validate_local_url(&issuer, &public_url, "/oauth", "WALTR_MCP_OAUTH_ISSUER")?;
        let resource = append_path(&public_url, "mcp");
        validate_local_url(&resource, &public_url, "/mcp", "MCP resource")?;
        let required_scopes = split_scopes(required(values, "WALTR_MCP_REQUIRED_SCOPES")?)?;
        if required_scopes.is_empty() {
            return Err(McpOAuthConfigError::Invalid(
                "WALTR_MCP_REQUIRED_SCOPES must contain at least one scope".to_owned(),
            ));
        }
        let wrapping_keys_file =
            PathBuf::from(required(values, "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE")?);
        let access_token_ttl = duration(
            values,
            "WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL",
            DEFAULT_ACCESS_TOKEN_TTL,
            Duration::from_secs(1),
            Duration::from_hours(1),
        )?;
        let refresh_token_ttl = duration(
            values,
            "WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL",
            DEFAULT_REFRESH_TOKEN_TTL,
            Duration::from_mins(1),
            Duration::from_hours(24 * 30),
        )?;
        let refresh_family_ttl = duration(
            values,
            "WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL",
            DEFAULT_REFRESH_FAMILY_TTL,
            Duration::from_mins(1),
            Duration::from_hours(24 * 365),
        )?;
        if refresh_token_ttl > refresh_family_ttl {
            return Err(McpOAuthConfigError::Invalid(
                "MCP OAuth refresh token TTL must not exceed the refresh family TTL".to_owned(),
            ));
        }
        let code_ttl = duration(
            values,
            "WALTR_MCP_OAUTH_CODE_TTL",
            DEFAULT_CODE_TTL,
            Duration::from_secs(1),
            Duration::from_mins(10),
        )?;
        let allow_dcr = boolean(values, "WALTR_MCP_OAUTH_ALLOW_DCR")?;
        let allow_loopback_redirects = boolean(values, "WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS")?;
        let allow_cimd = boolean(values, "WALTR_MCP_OAUTH_ALLOW_CIMD")?;
        let cimd_trusted_private_origins =
            trusted_origins(values, "WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS")?;
        let allow_private_test_destinations =
            boolean(values, "WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS")?;
        Ok(Self::Enabled(McpOAuthEnabledConfig {
            issuer,
            resource,
            public_url,
            required_scopes,
            wrapping_keys_file,
            access_token_ttl,
            refresh_token_ttl,
            refresh_family_ttl,
            code_ttl,
            allow_dcr,
            allow_loopback_redirects,
            allow_cimd,
            cimd_trusted_private_origins,
            allow_private_test_destinations,
        }))
    }
}

impl McpOAuthEnabledConfig {
    /// Construct explicit hosted OAuth settings with secure default lifetimes.
    ///
    /// # Errors
    ///
    /// Returns an error unless issuer and resource are exact deployment-relative HTTPS URLs.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the constructor accepts and normalizes an owned deployment scope list"
    )]
    pub fn new(
        public_url: impl Into<String>,
        issuer: impl Into<String>,
        required_scopes: Vec<String>,
        wrapping_keys_file: impl Into<PathBuf>,
    ) -> Result<Self, McpOAuthConfigError> {
        let public_url = public_url.into();
        let issuer = issuer.into();
        validate_local_url(&issuer, &public_url, "/oauth", "MCP OAuth issuer")?;
        let resource = append_path(&public_url, "mcp");
        validate_local_url(&resource, &public_url, "/mcp", "MCP resource")?;
        let required_scopes = split_scopes(&required_scopes.join(" "))?;
        if required_scopes.is_empty() {
            return Err(McpOAuthConfigError::Invalid(
                "MCP OAuth requires at least one scope".to_owned(),
            ));
        }
        Ok(Self {
            issuer,
            resource,
            public_url,
            required_scopes,
            wrapping_keys_file: wrapping_keys_file.into(),
            access_token_ttl: DEFAULT_ACCESS_TOKEN_TTL,
            refresh_token_ttl: DEFAULT_REFRESH_TOKEN_TTL,
            refresh_family_ttl: DEFAULT_REFRESH_FAMILY_TTL,
            code_ttl: DEFAULT_CODE_TTL,
            allow_dcr: false,
            allow_loopback_redirects: false,
            allow_cimd: false,
            cimd_trusted_private_origins: Vec::new(),
            allow_private_test_destinations: false,
        })
    }

    /// Exact hosted issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Exact public MCP resource URL.
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// Scopes supported and required by the MCP endpoint.
    #[must_use]
    pub fn required_scopes(&self) -> &[String] {
        &self.required_scopes
    }

    /// Configure generic public-client discovery and registration policy.
    #[must_use]
    pub const fn with_client_registration(mut self, allow_dcr: bool, allow_cimd: bool) -> Self {
        self.allow_dcr = allow_dcr;
        self.allow_cimd = allow_cimd;
        self
    }

    /// Configure whether dynamically registered clients may use HTTP loopback redirects.
    #[must_use]
    pub const fn with_loopback_redirects(mut self, allow: bool) -> Self {
        self.allow_loopback_redirects = allow;
        self
    }

    #[must_use]
    #[cfg(any(debug_assertions, feature = "test"))]
    pub(crate) const fn with_access_token_ttl(mut self, lifetime: Duration) -> Self {
        self.access_token_ttl = lifetime;
        self
    }
}

/// Initialized hosted OAuth server and its Waltr adapters.
#[derive(Clone)]
pub struct McpOAuthServer {
    authorization: OAuthAuthorizationServer,
    config: McpOAuthEnabledConfig,
    store: Arc<dyn OAuthAuthorizationStore>,
    encryption: Arc<dyn OAuthKeyEncryption>,
    login: Arc<WaltrResourceOwnerAuthenticator>,
    maintenance_healthy: Arc<AtomicBool>,
}

impl fmt::Debug for McpOAuthServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthServer")
            .field("issuer", &self.config.issuer)
            .field("resource", &self.config.resource)
            .finish_non_exhaustive()
    }
}

impl McpOAuthServer {
    /// Initialize SQL state, wrapping keys, adapters, and the active signing key.
    ///
    /// # Errors
    ///
    /// Returns a sanitized startup error when persistence or signing state is unusable.
    #[allow(
        clippy::too_many_lines,
        reason = "startup ordering must remain linear and fail closed"
    )]
    pub async fn initialize(
        config: McpOAuthEnabledConfig,
        persistence: &Persistence,
        access: AccessAuthenticator,
    ) -> Result<Self, McpOAuthError> {
        if !access.enabled() {
            return Err(McpOAuthError::Configuration);
        }
        let keyring = Arc::new(load_keyring(&config.wrapping_keys_file).await?);
        let store_config = OAuthAuthorizationStoreConfig {
            allow_public_client_registration: config.allow_dcr,
            ..OAuthAuthorizationStoreConfig::default()
        };
        let store = oauth_authorization_store(persistence, store_config).await?;
        let login = Arc::new(WaltrResourceOwnerAuthenticator::new(
            access.clone(),
            AccessAuthStore::new(persistence),
            config.public_url.clone(),
        ));
        let consent = Arc::new(WaltrConsentHandler::new(
            config.public_url.clone(),
            access.allow_insecure_cookie(),
        ));
        let mut server_config = OAuthAuthorizationServerConfig::new(
            config.issuer.clone(),
            vec![OAuthResource {
                resource: config.resource.clone(),
                scopes: config.required_scopes.clone(),
            }],
        );
        server_config.authorization_transaction_lifetime = AUTHORIZATION_TRANSACTION_TTL;
        server_config.authorization_code_lifetime = config.code_ttl;
        server_config.access_token_lifetime = config.access_token_ttl;
        server_config.signing_verification_overlap = config
            .access_token_ttl
            .checked_add(server_config.clock_skew)
            .ok_or(McpOAuthError::Configuration)?;
        server_config.refresh_token_lifetime = config.refresh_token_ttl;
        server_config.refresh_family_lifetime = config.refresh_family_ttl;
        if config.allow_dcr {
            server_config.registration_endpoint = Some(format!("{}/register", config.issuer));
        }
        let encryption: Arc<dyn OAuthKeyEncryption> = keyring;
        let mut authorization = OAuthAuthorizationServer::new(
            server_config,
            Arc::clone(&store),
            login.clone(),
            consent,
            Arc::new(McpSystemOAuthClock),
            Arc::new(SystemEntropy),
            Arc::clone(&encryption),
        )
        .map_err(|_| McpOAuthError::Configuration)?;
        if config.allow_dcr || config.allow_cimd {
            let metadata_fetcher = if config.allow_cimd {
                let fetcher = HardenedOAuthClientMetadataFetcher::production();
                let fetcher = if config.allow_private_test_destinations {
                    fetcher
                        .with_destination_policy(Arc::new(PrivateNetworkOAuthCimdDestinationPolicy))
                } else {
                    let origins = cimd_trusted_origins(&config);
                    let policy = TrustedPrivateOAuthCimdDestinationPolicy::new(origins)
                        .map_err(|_| McpOAuthError::Configuration)?;
                    fetcher.with_destination_policy(Arc::new(policy))
                };
                Some(Arc::new(fetcher) as _)
            } else {
                None
            };
            let registration = OAuthClientRegistrationOptions {
                dynamic_registration: config.allow_dcr,
                allow_loopback_redirects: config.allow_loopback_redirects,
                metadata_fetcher,
                ..OAuthClientRegistrationOptions::default()
            };
            authorization = authorization
                .with_client_registration(registration)
                .map_err(|_| McpOAuthError::Configuration)?;
        }
        let server = Self {
            authorization,
            config,
            store,
            encryption,
            login,
            maintenance_healthy: Arc::new(AtomicBool::new(false)),
        };
        server.ensure_signing_ready().await?;
        server.maintain(false).await?;
        Ok(server)
    }

    /// Hosted generic authorization service.
    #[must_use]
    pub const fn authorization(&self) -> &OAuthAuthorizationServer {
        &self.authorization
    }

    /// Exact protected resource.
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.config.resource
    }

    /// Exact hosted issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.config.issuer
    }

    /// Required MCP scopes.
    #[must_use]
    pub fn required_scopes(&self) -> &[String] {
        &self.config.required_scopes
    }

    /// Compose generic OAuth routes and Waltr's browser-login bridge.
    pub fn router(&self) -> Router {
        self.authorization.router().merge(
            Router::new()
                .route("/oauth/login", get(oauth_login))
                .with_state(self.login.clone()),
        )
    }

    /// Provision a controlled public client through the generic store API.
    ///
    /// # Errors
    ///
    /// Returns an error when the client is invalid, duplicate, or cannot be persisted.
    pub async fn register_client(
        &self,
        client: mcp::oauth::OAuthClient,
    ) -> Result<(), McpOAuthError> {
        match self.store.register_client(client.clone()).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                let existing = self
                    .store
                    .client(
                        client.client_id.clone(),
                        unix_ms(SystemTime::now()).unwrap_or(i64::MAX),
                    )
                    .await
                    .map_err(|_| McpOAuthError::Persistence)?;
                if existing.is_some_and(|existing| {
                    existing.client_id == client.client_id
                        && existing.registration_method == client.registration_method
                        && existing.display_name == client.display_name
                        && existing.client_secret_digest == client.client_secret_digest
                        && existing.redirect_uris == client.redirect_uris
                        && existing.expires_at_ms == client.expires_at_ms
                        && existing.disabled_at_ms == client.disabled_at_ms
                }) {
                    Ok(())
                } else {
                    Err(McpOAuthError::Configuration)
                }
            }
            Err(_) => Err(McpOAuthError::Persistence),
        }
    }

    pub(crate) async fn maintain(&self, rotate: bool) -> Result<(), McpOAuthError> {
        let result = self.maintain_inner(rotate).await;
        self.maintenance_healthy
            .store(result.is_ok(), Ordering::Release);
        result
    }

    async fn maintain_inner(&self, rotate: bool) -> Result<(), McpOAuthError> {
        self.authorization
            .cleanup(MAINTENANCE_BATCH_SIZE)
            .await
            .map_err(|_| McpOAuthError::Persistence)?;
        if rotate {
            let candidate = self
                .authorization
                .generate_signing_key_candidate(false)
                .await
                .map_err(|_| McpOAuthError::Signing)?;
            self.authorization
                .activate_signing_key(candidate.key_id)
                .await
                .map_err(|_| McpOAuthError::Signing)?;
        }
        self.rewrap_signing_keys().await
    }

    /// Check hosted OAuth persistence, signer integrity, maintenance, and revocation bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when maintenance is unhealthy or the active signing key is invalid.
    pub async fn ready(&self) -> Result<(), McpOAuthError> {
        if !self.maintenance_healthy.load(Ordering::Acquire)
            || self
                .authorization
                .revocation_health()
                .authoritative_poll_interval
                > MAX_REVOCATION_TERMINATION_BOUND
        {
            return Err(McpOAuthError::Maintenance);
        }
        self.validate_active_signing_key().await
    }

    pub(crate) fn mark_maintenance_unhealthy(&self) {
        self.maintenance_healthy.store(false, Ordering::Release);
    }

    async fn ensure_signing_ready(&self) -> Result<(), McpOAuthError> {
        let mut active = self
            .store
            .active_signing_key(self.config.issuer.clone())
            .await
            .map_err(|_| McpOAuthError::Persistence)?;
        if active.is_none() {
            self.authorization
                .generate_signing_key_candidate(true)
                .await
                .map_err(|_| McpOAuthError::Signing)?;
            active = self
                .store
                .active_signing_key(self.config.issuer.clone())
                .await
                .map_err(|_| McpOAuthError::Persistence)?;
        }
        self.validate_active_signing_key_record(&active.ok_or(McpOAuthError::Signing)?)
    }

    async fn validate_active_signing_key(&self) -> Result<(), McpOAuthError> {
        let active = self
            .store
            .active_signing_key(self.config.issuer.clone())
            .await
            .map_err(|_| McpOAuthError::Persistence)?
            .ok_or(McpOAuthError::Signing)?;
        self.validate_active_signing_key_record(&active)
    }

    fn validate_active_signing_key_record(
        &self,
        active: &mcp::oauth::OAuthSigningKey,
    ) -> Result<(), McpOAuthError> {
        let encrypted = active
            .encrypted_private_key
            .as_ref()
            .ok_or(McpOAuthError::Signing)?;
        let private = self
            .encryption
            .decrypt(&active.algorithm, &active.key_id, encrypted)
            .map_err(|_| McpOAuthError::Signing)?;
        validate_signing_key_pair(active, &private)
    }

    async fn rewrap_signing_keys(&self) -> Result<(), McpOAuthError> {
        let now = unix_ms(SystemTime::now()).ok_or(McpOAuthError::Signing)?;
        let keys = self
            .store
            .verification_signing_keys(self.config.issuer.clone(), now)
            .await
            .map_err(|_| McpOAuthError::Persistence)?;
        for key in keys {
            if key.encrypted_private_key.is_some()
                && matches!(
                    key.state,
                    OAuthSigningKeyState::Active | OAuthSigningKeyState::Retired
                )
            {
                self.authorization
                    .rewrap_signing_key(key.key_id)
                    .await
                    .map_err(|_| McpOAuthError::Signing)?;
            }
        }
        Ok(())
    }
}

fn cimd_trusted_origins(config: &McpOAuthEnabledConfig) -> Vec<Url> {
    let mut origins = config.cimd_trusted_private_origins.clone();
    let mut self_origin = Url::parse(&config.public_url)
        .expect("validated WALTR_PUBLIC_URL should remain an absolute URL");
    self_origin.set_path("/");
    self_origin.set_query(None);
    self_origin.set_fragment(None);
    if !origins
        .iter()
        .any(|origin| origin.origin() == self_origin.origin())
    {
        origins.push(self_origin);
    }
    origins
}

async fn oauth_authorization_store(
    persistence: &Persistence,
    config: OAuthAuthorizationStoreConfig,
) -> Result<Arc<dyn OAuthAuthorizationStore>, McpOAuthError> {
    if let Some(pool) = persistence.mcp_sqlite_pool() {
        return Ok(Arc::new(
            SqliteOAuthAuthorizationStore::from_pool_with_config(pool.clone(), config)
                .await
                .map_err(|_| McpOAuthError::Persistence)?,
        ));
    }
    if let Some(pool) = persistence.mcp_postgres_pool() {
        return Ok(Arc::new(
            PostgresOAuthAuthorizationStore::from_pool_with_config(pool.clone(), config)
                .await
                .map_err(|_| McpOAuthError::Persistence)?,
        ));
    }
    Err(McpOAuthError::Persistence)
}

#[derive(Debug)]
struct WaltrResourceOwnerAuthenticator {
    access: AccessAuthenticator,
    store: AccessAuthStore,
    public_url: String,
}

impl WaltrResourceOwnerAuthenticator {
    const fn new(access: AccessAuthenticator, store: AccessAuthStore, public_url: String) -> Self {
        Self {
            access,
            store,
            public_url,
        }
    }

    fn correlation_cookie_name(&self) -> &'static str {
        if self.access.allow_insecure_cookie() {
            "waltr-mcp-oauth-local"
        } else {
            "__Host-waltr-mcp-oauth"
        }
    }
}

impl OAuthResourceOwnerAuthenticator for WaltrResourceOwnerAuthenticator {
    fn authenticate(
        &self,
        request: OAuthResourceOwnerAuthenticationRequest,
    ) -> BoxFuture<OAuthResourceOwnerAuthentication> {
        let access = self.access.clone();
        let initial = !request.browser_correlation.expose_secret().is_empty();
        let store = self.store.clone();
        let cookie_name = self.correlation_cookie_name();
        let public_url = self.public_url.clone();
        Box::pin(async move {
            match access
                .authenticate_browser_session_headers(&request.headers)
                .await
            {
                Ok(principal) => {
                    let correlation = if initial {
                        None
                    } else {
                        raw_cookie(&request.headers, cookie_name).map(McpOAuthSecret::new)
                    };
                    OAuthResourceOwnerAuthentication::Authenticated {
                        principal_id: principal.mcp_principal_id(),
                        browser_correlation: correlation,
                    }
                }
                Err(RequestAuthFailure::Unauthenticated) if initial => {
                    let now = SystemTime::now();
                    let correlation = request.browser_correlation.expose_secret();
                    if store
                        .insert_mcp_oauth_login_continuation(&McpOAuthLoginContinuation {
                            transaction_id: request.transaction_id.clone(),
                            correlation_digest: LoginCorrelationDigest::from_correlation(
                                correlation,
                            ),
                            created_at: now,
                            expires_at: now + AUTHORIZATION_TRANSACTION_TTL,
                        })
                        .await
                        .is_err()
                    {
                        return OAuthResourceOwnerAuthentication::Unavailable;
                    }
                    OAuthResourceOwnerAuthentication::LoginRequired {
                        login_url: format!(
                            "{}/oauth/login?{}",
                            public_url.trim_end_matches('/'),
                            url::form_urlencoded::Serializer::new(String::new())
                                .append_pair("transaction_id", &request.transaction_id)
                                .append_pair("correlation", correlation)
                                .finish()
                        ),
                    }
                }
                Err(
                    RequestAuthFailure::Unauthenticated | RequestAuthFailure::InsufficientScope,
                ) => OAuthResourceOwnerAuthentication::Denied,
                Err(RequestAuthFailure::Internal) => OAuthResourceOwnerAuthentication::Unavailable,
            }
        })
    }
}

#[derive(Deserialize)]
struct OAuthLoginQuery {
    transaction_id: String,
    correlation: String,
}

async fn oauth_login(
    State(state): State<Arc<WaltrResourceOwnerAuthenticator>>,
    Query(query): Query<OAuthLoginQuery>,
) -> Response {
    let digest = LoginCorrelationDigest::from_correlation(&query.correlation);
    match state
        .store
        .consume_mcp_oauth_login_continuation(&query.transaction_id, &digest, SystemTime::now())
        .await
    {
        Ok(true) => {}
        Ok(false) => return no_store(StatusCode::BAD_REQUEST.into_response()),
        Err(_) => return no_store(StatusCode::SERVICE_UNAVAILABLE.into_response()),
    }
    let continuation = format!(
        "/oauth/authorize?transaction_id={}",
        url::form_urlencoded::byte_serialize(query.transaction_id.as_bytes()).collect::<String>()
    );
    let return_to = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &continuation)
        .finish();
    let location = format!("/oidc/login?{return_to}");
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&location).expect("local login continuation is a valid header"),
    );
    let cookie = build_cookie(
        state.correlation_cookie_name().to_owned(),
        &query.correlation,
        !state.access.allow_insecure_cookie(),
        AUTHORIZATION_TRANSACTION_TTL,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie.to_string()).expect("generated cookie is valid"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    no_store(response)
}

#[derive(Debug)]
struct WaltrConsentHandler {
    deployment: String,
    secure_cookie: bool,
}

impl WaltrConsentHandler {
    const fn new(deployment: String, allow_insecure_cookie: bool) -> Self {
        Self {
            deployment,
            secure_cookie: !allow_insecure_cookie,
        }
    }
}

impl OAuthConsentHandler for WaltrConsentHandler {
    fn present(&self, model: OAuthConsentModel) -> BoxFuture<OAuthConsentPresentation> {
        let deployment = self.deployment.clone();
        let secure_cookie = self.secure_cookie;
        Box::pin(async move {
            let csrf = random_secret();
            let cookie_name = consent_cookie_name(&model.transaction_id, secure_cookie);
            let mut scopes = String::new();
            for scope in &model.requested_scopes {
                write!(
                    scopes,
                    "<li><code>{}</code><span>Access Waltr MCP with this permission.</span></li>",
                    escape_html(scope)
                )
                .expect("writing to a String cannot fail");
            }
            let client_name = model
                .client_display_name
                .as_deref()
                .unwrap_or(&model.client_id);
            let body = format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Authorize Waltr MCP</title><style>body{{font:16px system-ui,sans-serif;background:#f5f1e8;color:#20201d;margin:0}}main{{max-width:42rem;margin:8vh auto;padding:2.5rem;background:#fff;border:1px solid #d8d0c1}}h1{{font:700 2rem Georgia,serif}}code{{font-weight:700}}li{{display:grid;gap:.25rem;margin:1rem 0}}.detail{{color:#625e56;font-size:.9rem;word-break:break-all}}.actions{{display:flex;gap:.75rem;margin-top:2rem}}button{{padding:.7rem 1.1rem;border:1px solid #20201d;background:#20201d;color:#fff}}button[value=deny]{{background:#fff;color:#20201d}}</style></head><body><main><p class=\"detail\">{}</p><h1>Authorize {}</h1><p>This client is requesting access to <strong>{}</strong>.</p><p class=\"detail\">Client ID: {}</p><p class=\"detail\">Resource: {}</p><ul>{}</ul><form method=\"post\" action=\"/oauth/authorize/decision\"><input type=\"hidden\" name=\"transaction_id\" value=\"{}\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><div class=\"actions\"><button name=\"decision\" value=\"approve\">Approve</button><button name=\"decision\" value=\"deny\">Deny</button></div></form></main></body></html>",
                escape_html(&deployment),
                escape_html(client_name),
                escape_html(&deployment),
                escape_html(&model.client_id),
                escape_html(&model.resource),
                scopes,
                escape_html(&model.transaction_id),
                escape_html(&csrf),
            );
            let mut response = Html(body).into_response();
            let cookie = build_cookie(
                cookie_name,
                &csrf,
                secure_cookie,
                AUTHORIZATION_TRANSACTION_TTL,
            );
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::from_str(&cookie.to_string()).expect("generated cookie is valid"),
            );
            OAuthConsentPresentation::Response(response)
        })
    }

    fn validate_decision(&self, evidence: OAuthConsentDecisionEvidence) -> BoxFuture<bool> {
        let cookie_name = consent_cookie_name(&evidence.transaction_id, self.secure_cookie);
        Box::pin(async move {
            raw_cookie(&evidence.headers, &cookie_name)
                .is_some_and(|cookie| constant_time_eq(cookie, evidence.csrf.expose_secret()))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrappingKeyFile {
    schema_version: u32,
    active: String,
    keys: Vec<WrappingKeyEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WrappingKeyEntry {
    id: String,
    key: String,
}

async fn load_keyring(path: &Path) -> Result<VersionedOAuthWrappingKeyring, McpOAuthError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| McpOAuthError::WrappingKeys)?;
    if !metadata.is_file() {
        return Err(McpOAuthError::WrappingKeys);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o222 != 0 {
            return Err(McpOAuthError::WrappingKeys);
        }
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| McpOAuthError::WrappingKeys)?;
    if bytes.len() > 64 * 1024 {
        return Err(McpOAuthError::WrappingKeys);
    }
    let parsed: WrappingKeyFile =
        serde_json::from_slice(&bytes).map_err(|_| McpOAuthError::WrappingKeys)?;
    if parsed.schema_version != 1 {
        return Err(McpOAuthError::WrappingKeys);
    }
    let mut keys = Vec::with_capacity(parsed.keys.len());
    let mut ids = BTreeSet::new();
    for entry in parsed.keys {
        if !ids.insert(entry.id.clone()) {
            return Err(McpOAuthError::WrappingKeys);
        }
        let key = URL_SAFE_NO_PAD
            .decode(entry.key)
            .map_err(|_| McpOAuthError::WrappingKeys)?;
        keys.push((entry.id, key));
    }
    VersionedOAuthWrappingKeyring::new(parsed.active, keys).map_err(|_| McpOAuthError::WrappingKeys)
}

fn validate_signing_key_pair(
    key: &mcp::oauth::OAuthSigningKey,
    private: &[u8],
) -> Result<(), McpOAuthError> {
    if key.algorithm != "ES256"
        || key
            .public_jwk
            .get("kty")
            .and_then(serde_json::Value::as_str)
            != Some("EC")
        || key
            .public_jwk
            .get("crv")
            .and_then(serde_json::Value::as_str)
            != Some("P-256")
        || key
            .public_jwk
            .get("alg")
            .and_then(serde_json::Value::as_str)
            != Some("ES256")
        || key
            .public_jwk
            .get("kid")
            .and_then(serde_json::Value::as_str)
            != Some(key.key_id.as_str())
    {
        return Err(McpOAuthError::Signing);
    }
    let secret = SecretKey::from_pkcs8_der(private).map_err(|_| McpOAuthError::Signing)?;
    let point = secret.public_key().to_encoded_point(false);
    let expected_x = point.x().ok_or(McpOAuthError::Signing)?;
    let expected_y = point.y().ok_or(McpOAuthError::Signing)?;
    let x = key
        .public_jwk
        .get("x")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .ok_or(McpOAuthError::Signing)?;
    let y = key
        .public_jwk
        .get("y")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .ok_or(McpOAuthError::Signing)?;
    if x.as_slice() != expected_x.as_slice() || y.as_slice() != expected_y.as_slice() {
        return Err(McpOAuthError::Signing);
    }
    Ok(())
}

#[derive(Debug)]
struct SystemEntropy;

impl McpOAuthEntropy for SystemEntropy {
    fn fill_bytes(&self, output: &mut [u8]) -> mcp::Result<()> {
        rand::rng().fill_bytes(output);
        Ok(())
    }
}

fn value<'a>(values: &'a HashMap<&str, String>, name: &str) -> Option<&'a str> {
    values
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn required<'a>(
    values: &'a HashMap<&str, String>,
    name: &str,
) -> Result<&'a str, McpOAuthConfigError> {
    value(values, name).ok_or_else(|| McpOAuthConfigError::Missing(name.to_owned()))
}

fn boolean(values: &HashMap<&str, String>, name: &str) -> Result<bool, McpOAuthConfigError> {
    match value(values, name).unwrap_or("false") {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(McpOAuthConfigError::Invalid(format!(
            "{name} must be exactly `true` or `false`"
        ))),
    }
}

fn trusted_origins(
    values: &HashMap<&str, String>,
    name: &str,
) -> Result<Vec<Url>, McpOAuthConfigError> {
    let Some(raw) = value(values, name) else {
        return Ok(Vec::new());
    };
    let mut origins = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        let origin = Url::parse(entry).map_err(|_| {
            McpOAuthConfigError::Invalid(format!(
                "{name} must contain comma-separated HTTPS origins"
            ))
        })?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(McpOAuthConfigError::Invalid(format!(
                "{name} must contain only HTTPS origins without paths, credentials, queries, or fragments"
            )));
        }
        TrustedPrivateOAuthCimdDestinationPolicy::new([origin.clone()]).map_err(|_| {
            McpOAuthConfigError::Invalid(format!("{name} contains a prohibited origin"))
        })?;
        if origins
            .iter()
            .any(|existing: &Url| existing.origin() == origin.origin())
        {
            return Err(McpOAuthConfigError::Invalid(format!(
                "{name} contains a duplicate origin"
            )));
        }
        origins.push(origin);
    }
    Ok(origins)
}

fn duration(
    values: &HashMap<&str, String>,
    name: &str,
    default: Duration,
    minimum: Duration,
    maximum: Duration,
) -> Result<Duration, McpOAuthConfigError> {
    let Some(raw) = value(values, name) else {
        return Ok(default);
    };
    let seconds = raw.parse::<u64>().map_err(|_| {
        McpOAuthConfigError::Invalid(format!("{name} must be an integer number of seconds"))
    })?;
    let value = Duration::from_secs(seconds);
    if value < minimum || value > maximum {
        return Err(McpOAuthConfigError::Invalid(format!(
            "{name} is outside the supported lifetime bounds"
        )));
    }
    Ok(value)
}

fn split_scopes(raw: &str) -> Result<Vec<String>, McpOAuthConfigError> {
    let mut scopes = Vec::new();
    for scope in raw.split_ascii_whitespace() {
        if scope.chars().any(char::is_control) {
            return Err(McpOAuthConfigError::Invalid(
                "WALTR_MCP_REQUIRED_SCOPES contains an invalid scope".to_owned(),
            ));
        }
        if !scopes.iter().any(|existing| existing == scope) {
            scopes.push(scope.to_owned());
        }
    }
    Ok(scopes)
}

fn append_path(base: &str, segment: &str) -> String {
    format!("{}/{segment}", base.trim_end_matches('/'))
}

fn validate_local_url(
    value: &str,
    public_url: &str,
    suffix: &str,
    name: &str,
) -> Result<(), McpOAuthConfigError> {
    let url = Url::parse(value)
        .map_err(|_| McpOAuthConfigError::Invalid(format!("{name} must be an absolute URL")))?;
    let public = Url::parse(public_url).map_err(|_| {
        McpOAuthConfigError::Invalid("WALTR_PUBLIC_URL must be an absolute URL".to_owned())
    })?;
    let expected_path = format!("{}{}", public.path().trim_end_matches('/'), suffix);
    if url.scheme() != "https"
        || url.origin() != public.origin()
        || url.path() != expected_path
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(McpOAuthConfigError::Invalid(format!(
            "{name} must be the exact HTTPS deployment URL ending in {suffix}"
        )));
    }
    Ok(())
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn consent_cookie_name(transaction_id: &str, secure: bool) -> String {
    let digest = Sha256::digest(transaction_id.as_bytes());
    let mut suffix = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(suffix, "{byte:02x}").expect("writing to a String cannot fail");
    }
    if secure {
        format!("__Host-waltr-mcp-consent-{suffix}")
    } else {
        format!("waltr-mcp-consent-local-{suffix}")
    }
}

fn raw_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().ok()?;
        for pair in value.split(';') {
            let (candidate, value) = pair.trim().split_once('=')?;
            if candidate == name && (value.is_empty() || found.replace(value).is_some()) {
                return None;
            }
        }
    }
    found
}

fn build_cookie(name: String, value: &str, secure: bool, ttl: Duration) -> Cookie<'static> {
    Cookie::build((name, value.to_owned()))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::seconds(
            i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX),
        ))
        .build()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (left, right)| difference | left ^ right)
        == 0
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn unix_ms(time: SystemTime) -> Option<i64> {
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

pub(crate) const fn oauth_cleanup_interval() -> Duration {
    OAUTH_CLEANUP_INTERVAL
}

pub(crate) const fn signing_rotation_interval() -> Duration {
    SIGNING_ROTATION_INTERVAL
}

/// MCP OAuth environment configuration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpOAuthConfigError {
    /// A required setting is absent.
    Missing(String),
    /// Settings are malformed or inconsistent.
    Invalid(String),
}

impl fmt::Display for McpOAuthConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => {
                write!(formatter, "required MCP OAuth setting {name} is missing")
            }
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl Error for McpOAuthConfigError {}

/// Sanitized hosted OAuth startup or maintenance failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOAuthError {
    /// Local composition is invalid.
    Configuration,
    /// The wrapping-key file is unavailable or invalid.
    WrappingKeys,
    /// OAuth persistence is unavailable.
    Persistence,
    /// Signing state is unavailable.
    Signing,
    /// OAuth maintenance or bounded revocation propagation is unhealthy.
    Maintenance,
}

impl fmt::Display for McpOAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Configuration => "MCP OAuth configuration is invalid",
            Self::WrappingKeys => "MCP OAuth wrapping keys are unavailable",
            Self::Persistence => "MCP OAuth persistence is unavailable",
            Self::Signing => "MCP OAuth signing state is unavailable",
            Self::Maintenance => "MCP OAuth maintenance is unavailable",
        })
    }
}

impl Error for McpOAuthError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_auth::OidcConfig;

    fn access_config() -> AccessAuthConfig {
        AccessAuthConfig::oidc(
            OidcConfig::new(
                "https://identity.example",
                "browser",
                None,
                "https://waltr.example/oidc/callback",
                "waltr-api",
                vec!["openid".to_owned()],
                Duration::from_hours(1),
                false,
                false,
            )
            .unwrap()
            .with_access_domains(
                "https://waltr.example/base",
                "https://identity.example",
                "waltr-api",
                vec!["api:use".to_owned()],
            )
            .unwrap(),
        )
    }

    #[test]
    fn disabled_access_rejects_partial_oauth_configuration() {
        let values = HashMap::from([("WALTR_MCP_REQUIRED_SCOPES", "mcp:use".to_owned())]);
        assert!(matches!(
            McpOAuthConfig::from_values(&AccessAuthConfig::disabled(), &values),
            Err(McpOAuthConfigError::Invalid(_))
        ));
        assert_eq!(
            McpOAuthConfig::from_values(&AccessAuthConfig::disabled(), &HashMap::new()).unwrap(),
            McpOAuthConfig::Disabled
        );
    }

    #[test]
    fn enabled_defaults_are_deployment_relative_and_domains_are_separate() {
        let values = HashMap::from([
            ("WALTR_MCP_REQUIRED_SCOPES", "mcp:use tools:read".to_owned()),
            (
                "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE",
                "/run/secrets/mcp-keys".to_owned(),
            ),
        ]);
        let McpOAuthConfig::Enabled(config) =
            McpOAuthConfig::from_values(&access_config(), &values).unwrap()
        else {
            panic!("OIDC mode should enable hosted OAuth");
        };
        assert_eq!(config.issuer(), "https://waltr.example/base/oauth");
        assert_eq!(config.resource(), "https://waltr.example/base/mcp");
        assert_eq!(config.required_scopes(), ["mcp:use", "tools:read"]);
        assert!(!config.allow_loopback_redirects);
        assert_eq!(
            cimd_trusted_origins(&config),
            [Url::parse("https://waltr.example/").expect("self origin")]
        );
    }

    #[test]
    fn issuer_lifetimes_and_policy_flags_are_strict() {
        let base = HashMap::from([
            ("WALTR_MCP_REQUIRED_SCOPES", "mcp:use".to_owned()),
            (
                "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE",
                "/run/secrets/mcp-keys".to_owned(),
            ),
        ]);
        for (name, value) in [
            ("WALTR_MCP_OAUTH_ISSUER", "https://other.example/oauth"),
            ("WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL", "0"),
            ("WALTR_MCP_OAUTH_CODE_TTL", "601"),
            ("WALTR_MCP_OAUTH_ALLOW_DCR", "yes"),
            ("WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS", "yes"),
        ] {
            let mut values = base.clone();
            values.insert(name, value.to_owned());
            assert!(
                McpOAuthConfig::from_values(&access_config(), &values).is_err(),
                "{name}={value}"
            );
        }
    }

    #[test]
    fn cimd_private_origins_are_exact_https_origins() {
        let mut values = HashMap::from([
            ("WALTR_MCP_REQUIRED_SCOPES", "mcp:use".to_owned()),
            (
                "WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE",
                "/run/secrets/mcp-keys".to_owned(),
            ),
            (
                "WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS",
                "https://private.example, https://other.example:444".to_owned(),
            ),
        ]);
        let McpOAuthConfig::Enabled(config) =
            McpOAuthConfig::from_values(&access_config(), &values).expect("valid config")
        else {
            panic!("OIDC mode should enable hosted OAuth");
        };
        assert_eq!(config.cimd_trusted_private_origins.len(), 2);

        for invalid in [
            "http://private.example",
            "https://private.example/path",
            "https://user@private.example",
            "https://private.example?query",
            "https://private.example#fragment",
            "https://private.example,",
            "https://PRIVATE.example:443,https://private.example",
            "https://localhost",
            "https://127.0.0.1",
            "https://169.254.169.254",
            "https://[::1]",
        ] {
            values.insert(
                "WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS",
                invalid.to_owned(),
            );
            assert!(
                McpOAuthConfig::from_values(&access_config(), &values).is_err(),
                "{invalid}"
            );
        }
    }

    #[tokio::test]
    async fn wrapping_key_file_rejects_writable_and_duplicate_key_ids() {
        let root = std::env::temp_dir().join(format!("waltr-mcp-key-test-{}", random_secret()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("keys.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"active":"one","keys":[{"id":"one","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"},{"id":"one","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_keyring(&path).await,
            Err(McpOAuthError::WrappingKeys)
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            assert!(matches!(
                load_keyring(&path).await,
                Err(McpOAuthError::WrappingKeys)
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn wrapping_key_file_requires_known_schema_version() {
        let root = std::env::temp_dir().join(format!("waltr-mcp-key-schema-{}", random_secret()));
        std::fs::create_dir_all(&root).unwrap();
        for (name, contents) in [
            (
                "missing.json",
                r#"{"active":"one","keys":[{"id":"one","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}]}"#,
            ),
            (
                "unknown.json",
                r#"{"schema_version":2,"active":"one","keys":[{"id":"one","key":"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}]}"#,
            ),
        ] {
            let path = root.join(name);
            std::fs::write(&path, contents).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            }
            assert!(matches!(
                load_keyring(&path).await,
                Err(McpOAuthError::WrappingKeys)
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
