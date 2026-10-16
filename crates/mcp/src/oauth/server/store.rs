use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::OAuthEncryptedPrivateKey;
use crate::McpPrincipalId;
use crate::server::BoxFuture;

#[cfg(feature = "oauth-authorization-server-postgres")]
mod postgres;
#[cfg(feature = "oauth-authorization-server-sqlite")]
mod sqlite;

#[cfg(feature = "oauth-authorization-server-postgres")]
pub use postgres::PostgresOAuthAuthorizationStore;
#[cfg(feature = "oauth-authorization-server-sqlite")]
pub use sqlite::SqliteOAuthAuthorizationStore;

/// Run the complete MCP `SQLite` migration history on an existing pool.
///
/// # Errors
///
/// Returns an error when migration history is invalid or a database operation fails.
#[cfg(feature = "oauth-authorization-server-sqlite")]
pub async fn migrate_mcp_sqlite(pool: &sqlx::SqlitePool) -> OAuthAuthorizationStoreResult<()> {
    sqlite::run_migrations(pool).await
}

/// Run the complete MCP `PostgreSQL` migration history on an existing pool.
///
/// # Errors
///
/// Returns an error when migration history is invalid or a database operation fails.
#[cfg(feature = "oauth-authorization-server-postgres")]
pub async fn migrate_mcp_postgres(pool: &sqlx::PgPool) -> OAuthAuthorizationStoreResult<()> {
    postgres::run_migrations(pool).await
}

/// Quotas enforced atomically by an OAuth authorization store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthAuthorizationStoreConfig {
    /// Maximum enabled or disabled registered clients retained by the store.
    pub max_clients: usize,
    /// Maximum redirect URIs for one client.
    pub max_redirect_uris_per_client: usize,
    /// Maximum unexpired authorization transactions.
    pub max_authorization_transactions: usize,
    /// Maximum retained grants.
    pub max_grants: usize,
    /// Maximum retained refresh families.
    pub max_refresh_families: usize,
    /// Maximum retained access-token authorization records.
    pub max_access_tokens: usize,
    /// Whether dynamically registered public clients are accepted.
    pub allow_public_client_registration: bool,
}

impl Default for OAuthAuthorizationStoreConfig {
    fn default() -> Self {
        Self {
            max_clients: 10_000,
            max_redirect_uris_per_client: 16,
            max_authorization_transactions: 10_000,
            max_grants: 100_000,
            max_refresh_families: 100_000,
            max_access_tokens: 1_000_000,
            allow_public_client_registration: true,
        }
    }
}

/// Authorization-store failure category. Values and backing-store diagnostics are not exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthAuthorizationStoreError {
    /// Backing persistence could not complete an operation.
    Unavailable,
    /// Supplied or persisted data violates the contract.
    InvalidData,
    /// Private key material could not be decrypted or wrapped.
    KeyUnavailable,
}

impl fmt::Display for OAuthAuthorizationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("OAuth authorization store unavailable"),
            Self::InvalidData => {
                formatter.write_str("OAuth authorization store contains invalid data")
            }
            Self::KeyUnavailable => formatter.write_str("OAuth signing key unavailable"),
        }
    }
}

impl std::error::Error for OAuthAuthorizationStoreError {}

/// Result returned by authorization-store operations.
pub type OAuthAuthorizationStoreResult<T> = Result<T, OAuthAuthorizationStoreError>;

/// How a public client became known to the authorization server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthClientRegistrationMethod {
    /// Statically provisioned client.
    Preconfigured,
    /// RFC 7591 dynamic registration.
    Dynamic,
    /// Client ID Metadata Document.
    ClientMetadataDocument,
}

/// Persisted public OAuth client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClient {
    /// Exact client identifier.
    pub client_id: String,
    /// Registration source.
    pub registration_method: OAuthClientRegistrationMethod,
    /// Untrusted display name.
    pub display_name: Option<String>,
    /// Digest of a confidential client secret, when one is provisioned.
    pub client_secret_digest: Option<Vec<u8>>,
    /// Admitted redirect URI set; HTTP loopback ports may vary at authorization.
    pub redirect_uris: Vec<String>,
    /// Creation time as Unix milliseconds.
    pub created_at_ms: i64,
    /// Optional client expiry.
    pub expires_at_ms: Option<i64>,
    /// Disablement time, when disabled.
    pub disabled_at_ms: Option<i64>,
}

impl OAuthClient {
    pub(crate) fn matches_redirect(&self, requested: &str) -> bool {
        self.redirect_uris.iter().any(|registered| {
            registered == requested
                || loopback_redirect_parts(registered)
                    .is_some_and(|parts| loopback_redirect_parts(requested) == Some(parts))
        })
    }
}

// Admission of an HTTP loopback URI is the policy opt-in. Compare the original
// bytes, excluding only the port, rather than normalizing unrelated components.
fn loopback_redirect_parts(value: &str) -> Option<(&str, &str)> {
    crate::validate_public_redirect_uri(value, true).ok()?;
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "http" {
        return None;
    }
    let authority_start = value.find("://")? + 3;
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |offset| authority_start + offset);
    let authority = &value[authority_start..authority_end];
    let port_start = authority.rsplit_once(':').and_then(|(host, port)| {
        (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
            .then_some(authority_start + host.len())
    });
    Some((
        &value[..port_start.unwrap_or(authority_end)],
        &value[authority_end..],
    ))
}

/// Validated cached Client ID Metadata Document snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCachedClientMetadata {
    /// Immutable snapshot identifier.
    pub snapshot_id: String,
    /// Exact client metadata URL.
    pub client_id: String,
    /// Normalized metadata document.
    pub document: serde_json::Value,
    /// Digest of the fetched document.
    pub document_digest: Vec<u8>,
    /// Retrieval time.
    pub retrieved_at_ms: i64,
    /// Freshness expiry.
    pub expires_at_ms: i64,
    /// Deadline after which the snapshot may be deleted.
    pub retain_until_ms: i64,
}

/// Atomic provisional dynamic registration request and abuse-control policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthDynamicClientRegistrationRequest {
    /// Normalized dynamic public client.
    pub client: OAuthClient,
    /// Provisional expiry.
    pub provisional_expires_at_ms: i64,
    /// Inactivity lifetime applied after promotion.
    pub inactivity_lifetime_ms: i64,
    /// Maximum provisional registrations.
    pub max_provisional_clients: usize,
    /// Maximum promoted dynamic registrations.
    pub max_active_dynamic_clients: usize,
    /// Global fixed-window registration limit.
    pub global_rate_limit: usize,
    /// Trusted-source fixed-window registration limit.
    pub source_rate_limit: usize,
    /// Fixed-window duration.
    pub rate_limit_window_ms: i64,
    /// Optional host-validated source identity.
    pub trusted_source: Option<String>,
}

/// Dynamic registration admission result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthDynamicClientRegistrationResult {
    /// Provisional client committed.
    Registered,
    /// Client identifier collision.
    Duplicate,
    /// Registration rate was exceeded.
    RateLimited,
    /// Dynamic registration capacity was exhausted.
    CapacityExceeded,
    /// Dynamic registration is disabled by store policy.
    Disabled,
}

/// Atomic first-authorization promotion request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthDynamicClientPromotionRequest {
    /// Authorization transaction receiving the principal.
    pub transaction_id: String,
    /// Authenticated principal.
    pub principal_id: McpPrincipalId,
    /// Operation time.
    pub now_ms: i64,
    /// Per-principal promoted dynamic-client limit.
    pub max_clients_per_principal: usize,
    /// Global promoted dynamic-client limit.
    pub max_active_dynamic_clients: usize,
}

/// Atomic resumed authentication and browser-correlation consumption request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthResumeAuthenticationRequest {
    /// Authorization transaction receiving the principal.
    pub transaction_id: String,
    /// Authenticated principal.
    pub principal_id: McpPrincipalId,
    /// Digest of the transaction-specific browser correlation cookie.
    pub browser_correlation_digest: Vec<u8>,
    /// Operation time.
    pub now_ms: i64,
}

/// Atomic resumed dynamic-client promotion and correlation consumption request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthResumeDynamicClientPromotionRequest {
    /// Existing dynamic-client promotion request and quota policy.
    pub promotion: OAuthDynamicClientPromotionRequest,
    /// Digest of the transaction-specific browser correlation cookie.
    pub browser_correlation_digest: Vec<u8>,
}

/// Authentication and dynamic promotion result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthClientPromotionResult {
    /// Principal attached and any provisional client promoted.
    Authenticated,
    /// Transaction was stale or already authenticated.
    InvalidTransaction,
    /// Principal or dynamic active-client quota was exhausted.
    QuotaExceeded,
}

/// Consent decision recorded for an authorization transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthAuthorizationDecision {
    /// Resource owner approved the exact scope set.
    Approved,
    /// Resource owner denied the request.
    Denied,
}

/// Persisted authorization endpoint transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAuthorizationTransaction {
    /// Opaque transaction identifier.
    pub transaction_id: String,
    /// Registered client identifier.
    pub client_id: String,
    /// Optional CIMD snapshot binding.
    pub client_metadata_snapshot_id: Option<String>,
    /// Exact redirect URI.
    pub redirect_uri: String,
    /// Exact resource identifier.
    pub resource: String,
    /// Requested scopes; canonicalized by the store.
    pub scopes: Vec<String>,
    /// S256 PKCE challenge.
    pub pkce_challenge: String,
    /// Digest of browser correlation secret.
    pub browser_correlation_digest: Vec<u8>,
    /// Opaque OAuth state reference.
    pub state_reference: String,
    /// Stable authenticated principal, when attached.
    pub principal_id: Option<String>,
    /// Resource-owner decision, when made.
    pub decision: Option<OAuthAuthorizationDecision>,
    /// Creation time.
    pub created_at_ms: i64,
    /// Absolute expiry.
    pub expires_at_ms: i64,
}

/// Validated authorization transaction and client view used by HTTP handlers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthAuthorizationContext {
    /// Persisted transaction.
    pub transaction: OAuthAuthorizationTransaction,
    /// Registered client.
    pub client: OAuthClient,
    /// Whether the exact principal/client/resource/scope consent already exists.
    pub consent_granted: bool,
}

/// Validated code view used to prepare a signed token before atomic redemption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthAuthorizationCodeContext {
    /// Persisted transaction bound to the code.
    pub transaction: OAuthAuthorizationTransaction,
}

/// Validated refresh view used to prepare a signed replacement before rotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthRefreshTokenContext {
    /// Persisted grant.
    pub grant: OAuthGrant,
    /// Refresh family absolute expiry.
    pub family_expires_at_ms: i64,
}

/// Digest-only authorization code record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAuthorizationCode {
    /// Code digest.
    pub code_digest: Vec<u8>,
    /// Approved transaction identifier.
    pub transaction_id: String,
    /// Creation time.
    pub created_at_ms: i64,
    /// Absolute expiry.
    pub expires_at_ms: i64,
    /// Consumption time.
    pub consumed_at_ms: Option<i64>,
}

/// Metadata for a caller-prepared signed access token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenMetadata {
    /// JWT identifier.
    pub jti: String,
    /// ES256 signing-key identifier.
    pub signing_key_id: String,
    /// Issuance time.
    pub issued_at_ms: i64,
    /// Expiration time.
    pub expires_at_ms: i64,
}

/// Atomic authorization-code redemption and first-token issuance request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthIssueGrantRequest {
    /// Presented code digest.
    pub code_digest: Vec<u8>,
    /// Exact request bindings.
    pub client_id: String,
    /// Exact request bindings.
    pub redirect_uri: String,
    /// Exact request bindings.
    pub resource: String,
    /// PKCE challenge derived from the presented verifier.
    pub pkce_challenge: String,
    /// Caller-generated grant identifier.
    pub grant_id: String,
    /// Caller-generated refresh-family identifier.
    pub refresh_family_id: String,
    /// Digest of the first refresh token.
    pub refresh_token_digest: Vec<u8>,
    /// First refresh-token expiry.
    pub refresh_token_expires_at_ms: i64,
    /// Refresh-family absolute expiry.
    pub refresh_family_expires_at_ms: i64,
    /// Prepared signed access-token metadata.
    pub access_token: OAuthTokenMetadata,
    /// Operation time.
    pub now_ms: i64,
}

/// Grant binding returned after a committed code exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthIssueGrantResult {
    /// Code was consumed and every grant/token record committed.
    Issued(Box<OAuthGrant>),
    /// Code was absent, consumed, expired, or mismatched.
    InvalidGrant,
}

/// Persisted authorization grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthGrant {
    /// Grant identifier.
    pub grant_id: String,
    /// Stable principal.
    pub principal_id: String,
    /// Client identifier.
    pub client_id: String,
    /// Resource identifier.
    pub resource: String,
    /// Canonical granted scopes.
    pub scopes: Vec<String>,
    /// Creation time.
    pub created_at_ms: i64,
    /// Revocation time.
    pub revoked_at_ms: Option<i64>,
}

/// Persisted refresh-token family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthRefreshFamily {
    /// Family identifier.
    pub family_id: String,
    /// Grant identifier.
    pub grant_id: String,
    /// Creation time.
    pub created_at_ms: i64,
    /// Absolute expiry.
    pub expires_at_ms: i64,
    /// Family revocation time.
    pub revoked_at_ms: Option<i64>,
    /// Sanitized revocation reason.
    pub revocation_reason: Option<String>,
}

/// Persisted access-token authorization record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAccessTokenRecord {
    /// JWT identifier.
    pub jti: String,
    /// Signing-key identifier.
    pub signing_key_id: String,
    /// Grant identifier.
    pub grant_id: String,
    /// Refresh family, when refreshable.
    pub refresh_family_id: Option<String>,
    /// Exact token bindings.
    pub client_id: String,
    /// Exact token bindings.
    pub principal_id: String,
    /// Exact token bindings.
    pub resource: String,
    /// Canonical scopes.
    pub scopes: Vec<String>,
    /// Issuance time.
    pub issued_at_ms: i64,
    /// Expiry time.
    pub expires_at_ms: i64,
    /// Revocation time.
    pub revoked_at_ms: Option<i64>,
}

/// Atomic refresh rotation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthRefreshRotationRequest {
    /// Presented refresh-token digest.
    pub refresh_token_digest: Vec<u8>,
    /// Exact expected binding.
    pub client_id: String,
    /// Exact expected binding.
    pub resource: String,
    /// Equal or narrower canonical scopes.
    pub scopes: Vec<String>,
    /// Replacement refresh-token digest.
    pub replacement_refresh_token_digest: Vec<u8>,
    /// Replacement refresh-token expiry.
    pub replacement_refresh_token_expires_at_ms: i64,
    /// Prepared signed access-token metadata.
    pub access_token: OAuthTokenMetadata,
    /// Operation time.
    pub now_ms: i64,
}

/// Refresh rotation outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthRefreshRotationResult {
    /// Presented generation was consumed and replacement records committed.
    Rotated {
        /// Replacement generation number.
        generation: u64,
    },
    /// Token was absent, expired, revoked, or mismatched.
    InvalidGrant,
    /// A consumed generation was replayed and the family was revoked.
    ReplayDetected,
}

/// Administrative or token-based idempotent revocation target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthRevocationTarget {
    /// JWT identifier and presenting client.
    AccessToken {
        /// JWT identifier.
        jti: String,
        /// Presenting client identifier.
        client_id: String,
    },
    /// Refresh digest and presenting client.
    RefreshToken {
        /// Opaque token digest.
        digest: Vec<u8>,
        /// Presenting client identifier.
        client_id: String,
    },
    /// Refresh family identifier.
    RefreshFamily(String),
    /// Grant identifier.
    Grant(String),
    /// Client identifier.
    Client(String),
}

/// Exact access-token validation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthValidateAccessTokenRequest {
    /// JWT identifier.
    pub jti: String,
    /// Exact signed claims.
    pub client_id: String,
    /// Exact signed claims.
    pub grant_id: String,
    /// Exact signed claims.
    pub principal_id: McpPrincipalId,
    /// Exact signed claims.
    pub resource: String,
    /// Exact signed claims.
    pub scopes: Vec<String>,
    /// Operation time.
    pub now_ms: i64,
}

/// Explicit signing-key lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthSigningKeyState {
    /// Generated candidate not yet used for issuance.
    Pending,
    /// Sole issuer key used for new tokens.
    Active,
    /// Verification-only key retained for overlap.
    Retired,
}

/// Candidate generated and encrypted outside the store transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthSigningKeyCandidate {
    /// Exact issuer.
    pub issuer: String,
    /// Stable JWT key identifier.
    pub key_id: String,
    /// Must be ES256.
    pub algorithm: String,
    /// Public JWK.
    pub public_jwk: serde_json::Value,
    /// Wrapped PKCS#8 private key.
    pub encrypted_private_key: OAuthEncryptedPrivateKey,
    /// Creation time.
    pub created_at_ms: i64,
}

/// Persisted signing key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthSigningKey {
    /// Exact issuer.
    pub issuer: String,
    /// Stable JWT key identifier.
    pub key_id: String,
    /// ES256 algorithm.
    pub algorithm: String,
    /// Public JWK.
    pub public_jwk: serde_json::Value,
    /// Wrapped private key, removed after the rollback window.
    pub encrypted_private_key: Option<OAuthEncryptedPrivateKey>,
    /// Lifecycle state.
    pub state: OAuthSigningKeyState,
    /// Creation time.
    pub created_at_ms: i64,
    /// Activation time.
    pub activated_at_ms: Option<i64>,
    /// Retirement time.
    pub retired_at_ms: Option<i64>,
    /// Public verification retention deadline.
    pub verify_until_ms: Option<i64>,
    /// Private-material retention deadline.
    pub private_until_ms: Option<i64>,
}

/// Bounded cleanup counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OAuthCleanupResult {
    /// Deleted transient or retained records.
    pub records_deleted: usize,
    /// Private key ciphertexts removed.
    pub private_keys_removed: usize,
    /// Public signing keys deleted.
    pub signing_keys_deleted: usize,
}

/// Atomic provider-neutral OAuth authorization persistence contract.
pub trait OAuthAuthorizationStore: Send + Sync + 'static {
    /// Store policy.
    fn config(&self) -> OAuthAuthorizationStoreConfig;
    /// Register one public client atomically with quota and redirect enforcement.
    fn register_client(
        &self,
        client: OAuthClient,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Atomically admit one provisional dynamic public client.
    fn register_dynamic_client(
        &self,
        _request: OAuthDynamicClientRegistrationRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthDynamicClientRegistrationResult>> {
        Box::pin(async { Ok(OAuthDynamicClientRegistrationResult::Disabled) })
    }
    /// Cache a validated CIMD snapshot.
    fn cache_client_metadata(
        &self,
        metadata: OAuthCachedClientMetadata,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<()>>;
    /// Resolve the current unexpired validated CIMD snapshot for a client ID.
    fn client_metadata(
        &self,
        _client_id: String,
        _now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Option<OAuthCachedClientMetadata>>> {
        Box::pin(async { Ok(None) })
    }
    /// Create a fully bound authorization transaction.
    fn create_authorization_transaction(
        &self,
        transaction: OAuthAuthorizationTransaction,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Resolve an enabled registered client at the operation time.
    fn client(
        &self,
        client_id: String,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Option<OAuthClient>>>;
    /// Load a live transaction, its client, and exact remembered-consent state.
    fn authorization_context(
        &self,
        transaction_id: String,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Option<OAuthAuthorizationContext>>>;
    /// Consume the browser correlation value once after matching its digest.
    fn consume_browser_correlation(
        &self,
        transaction_id: String,
        correlation_digest: Vec<u8>,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Attach one stable principal exactly once.
    fn authenticate_authorization_transaction(
        &self,
        transaction_id: String,
        principal_id: McpPrincipalId,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Atomically consume browser correlation and attach one stable principal.
    fn resume_authorization_transaction(
        &self,
        _request: OAuthResumeAuthenticationRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    /// Attach a principal and atomically promote a provisional dynamic client.
    fn authenticate_and_promote_client(
        &self,
        _request: OAuthDynamicClientPromotionRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthClientPromotionResult>> {
        Box::pin(async { Ok(OAuthClientPromotionResult::InvalidTransaction) })
    }
    /// Atomically promote a dynamic client, attach its principal, and consume correlation.
    fn resume_and_promote_client(
        &self,
        _request: OAuthResumeDynamicClientPromotionRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthClientPromotionResult>> {
        Box::pin(async { Ok(OAuthClientPromotionResult::InvalidTransaction) })
    }
    /// Record one approval or denial decision.
    fn decide_authorization_transaction(
        &self,
        transaction_id: String,
        decision: OAuthAuthorizationDecision,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Record consent for the exact approved principal/client/resource/scope set.
    fn remember_authorization_consent(
        &self,
        transaction_id: String,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Create a digest-only code for an approved transaction.
    fn issue_authorization_code(
        &self,
        code: OAuthAuthorizationCode,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Inspect an unconsumed live code before preparing its signed access token.
    fn authorization_code_context(
        &self,
        code_digest: Vec<u8>,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Option<OAuthAuthorizationCodeContext>>>;
    /// Consume one code and create all grant and token records atomically.
    fn redeem_authorization_code(
        &self,
        request: OAuthIssueGrantRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthIssueGrantResult>>;
    /// Consume one refresh generation and create its replacement atomically.
    fn rotate_refresh_token(
        &self,
        request: OAuthRefreshRotationRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthRefreshRotationResult>>;
    /// Inspect a live refresh generation before preparing replacement tokens.
    fn refresh_token_context(
        &self,
        refresh_token_digest: Vec<u8>,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Option<OAuthRefreshTokenContext>>>;
    /// Idempotently revoke a token, family, grant, or client.
    fn revoke(
        &self,
        target: OAuthRevocationTarget,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<()>>;
    /// Authoritatively validate every persisted access-token binding.
    fn validate_access_token(
        &self,
        request: OAuthValidateAccessTokenRequest,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Insert a pending candidate, or atomically bootstrap it as active when issuer is empty.
    fn put_signing_key_candidate(
        &self,
        candidate: OAuthSigningKeyCandidate,
        bootstrap: bool,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthSigningKey>>;
    /// Atomically activate a pending key and retire the previous active key.
    #[allow(clippy::too_many_arguments)]
    fn activate_signing_key(
        &self,
        issuer: String,
        key_id: String,
        now_ms: i64,
        verify_until_ms: i64,
        private_until_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Load the sole active signer.
    fn active_signing_key(
        &self,
        issuer: String,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Option<OAuthSigningKey>>>;
    /// Publish active and retained retired verification keys.
    fn verification_signing_keys(
        &self,
        issuer: String,
        now_ms: i64,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<Vec<OAuthSigningKey>>>;
    /// Replace ciphertext without changing the signing-key ID.
    fn rewrap_signing_key(
        &self,
        issuer: String,
        key_id: String,
        encrypted: OAuthEncryptedPrivateKey,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<bool>>;
    /// Bounded cleanup of expired security state and signing keys.
    fn cleanup(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> BoxFuture<OAuthAuthorizationStoreResult<OAuthCleanupResult>>;
}

/// Complete process-local behavioral reference implementation.
#[derive(Clone, Debug)]
pub struct InMemoryOAuthAuthorizationStore {
    state: Arc<Mutex<OAuthState>>,
    config: OAuthAuthorizationStoreConfig,
}

impl Default for InMemoryOAuthAuthorizationStore {
    fn default() -> Self {
        Self::new(OAuthAuthorizationStoreConfig::default())
    }
}

impl InMemoryOAuthAuthorizationStore {
    /// Create an empty store with explicit quotas.
    #[must_use]
    pub fn new(config: OAuthAuthorizationStoreConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(OAuthState::default())),
            config,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct OAuthState {
    clients: BTreeMap<String, OAuthClient>,
    #[serde(default)]
    dynamic_clients: BTreeMap<String, DynamicClientLifecycle>,
    metadata: BTreeMap<String, OAuthCachedClientMetadata>,
    transactions: BTreeMap<String, OAuthAuthorizationTransaction>,
    codes: BTreeMap<String, OAuthAuthorizationCode>,
    grants: BTreeMap<String, OAuthGrant>,
    families: BTreeMap<String, OAuthRefreshFamily>,
    refresh_tokens: BTreeMap<String, RefreshTokenRecord>,
    access_tokens: BTreeMap<String, OAuthAccessTokenRecord>,
    signing_keys: BTreeMap<String, OAuthSigningKey>,
    #[serde(default)]
    consents: BTreeSet<String>,
    #[serde(default)]
    consumed_browser_correlations: BTreeSet<String>,
    #[serde(default)]
    registration_rates: BTreeMap<String, RegistrationRate>,
    #[serde(default)]
    revoked_clients: BTreeMap<String, i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DynamicClientLifecycle {
    provisional_expires_at_ms: i64,
    inactivity_lifetime_ms: i64,
    promoted_principal_id: Option<String>,
    promoted_at_ms: Option<i64>,
    last_used_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RegistrationRate {
    window_started_at_ms: i64,
    window_expires_at_ms: i64,
    count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RefreshTokenRecord {
    digest: Vec<u8>,
    family_id: String,
    generation: u64,
    issued_at_ms: i64,
    expires_at_ms: i64,
    consumed_at_ms: Option<i64>,
    replacement_generation: Option<u64>,
}

impl OAuthState {
    fn client(&self, id: &str, now: i64) -> Option<OAuthClient> {
        if self.revoked_clients.contains_key(id) {
            return None;
        }
        self.clients
            .get(id)
            .filter(|client| {
                client.created_at_ms <= now
                    && client.disabled_at_ms.is_none()
                    && client.expires_at_ms.is_none_or(|expiry| expiry > now)
                    && self.dynamic_clients.get(id).is_none_or(|lifecycle| {
                        lifecycle.promoted_principal_id.is_some()
                            || lifecycle.provisional_expires_at_ms > now
                    })
            })
            .cloned()
    }

    fn authorization_context(&self, id: &str, now: i64) -> Option<OAuthAuthorizationContext> {
        let transaction = self
            .transactions
            .get(id)
            .filter(|value| value.created_at_ms <= now && value.expires_at_ms > now)?
            .clone();
        let client = self.client(&transaction.client_id, now).or_else(|| {
            transaction
                .client_metadata_snapshot_id
                .as_ref()
                .and_then(|id| self.metadata.get(id))
                .and_then(metadata_client)
        })?;
        let consent_granted = transaction.principal_id.as_ref().is_some_and(|principal| {
            self.consents.contains(&consent_key(
                principal,
                &transaction.client_id,
                &transaction.resource,
                &transaction.scopes,
            ))
        });
        Some(OAuthAuthorizationContext {
            transaction,
            client,
            consent_granted,
        })
    }

    fn consume_correlation(
        &mut self,
        id: &str,
        digest: &[u8],
        now: i64,
    ) -> OAuthAuthorizationStoreResult<bool> {
        validate_time(now)?;
        drop(digest_key(digest)?);
        let Some(transaction) = self
            .transactions
            .get(id)
            .filter(|value| value.created_at_ms <= now && value.expires_at_ms > now)
        else {
            return Ok(false);
        };
        if transaction.browser_correlation_digest != digest
            || self.consumed_browser_correlations.contains(id)
        {
            return Ok(false);
        }
        self.consumed_browser_correlations.insert(id.to_owned());
        Ok(true)
    }

    fn resume_authenticate(
        &mut self,
        request: &OAuthResumeAuthenticationRequest,
    ) -> OAuthAuthorizationStoreResult<bool> {
        validate_time(request.now_ms)?;
        drop(digest_key(&request.browser_correlation_digest)?);
        let valid = self
            .authorization_context(&request.transaction_id, request.now_ms)
            .is_some_and(|context| {
                context.transaction.principal_id.is_none()
                    && constant_time_equal(
                        &context.transaction.browser_correlation_digest,
                        &request.browser_correlation_digest,
                    )
                    && !self
                        .consumed_browser_correlations
                        .contains(&request.transaction_id)
            });
        if !valid {
            return Ok(false);
        }
        self.transactions
            .get_mut(&request.transaction_id)
            .ok_or(OAuthAuthorizationStoreError::InvalidData)?
            .principal_id = Some(request.principal_id.as_str().to_owned());
        self.consumed_browser_correlations
            .insert(request.transaction_id.clone());
        Ok(true)
    }

    fn resume_and_promote(
        &mut self,
        request: &OAuthResumeDynamicClientPromotionRequest,
    ) -> OAuthAuthorizationStoreResult<OAuthClientPromotionResult> {
        validate_time(request.promotion.now_ms)?;
        drop(digest_key(&request.browser_correlation_digest)?);
        let correlation_valid = self
            .transactions
            .get(&request.promotion.transaction_id)
            .is_some_and(|transaction| {
                transaction.created_at_ms <= request.promotion.now_ms
                    && transaction.expires_at_ms > request.promotion.now_ms
                    && constant_time_equal(
                        &transaction.browser_correlation_digest,
                        &request.browser_correlation_digest,
                    )
                    && !self
                        .consumed_browser_correlations
                        .contains(&request.promotion.transaction_id)
            });
        if !correlation_valid {
            return Ok(OAuthClientPromotionResult::InvalidTransaction);
        }
        let result = self.authenticate_and_promote(&request.promotion);
        if result == OAuthClientPromotionResult::Authenticated {
            self.consumed_browser_correlations
                .insert(request.promotion.transaction_id.clone());
        }
        Ok(result)
    }

    fn remember_consent(&mut self, id: &str, now: i64) -> bool {
        let Some(transaction) = self.transactions.get(id).filter(|value| {
            value.created_at_ms <= now
                && value.expires_at_ms > now
                && value.decision == Some(OAuthAuthorizationDecision::Approved)
        }) else {
            return false;
        };
        let Some(principal) = transaction.principal_id.as_ref() else {
            return false;
        };
        self.consents.insert(consent_key(
            principal,
            &transaction.client_id,
            &transaction.resource,
            &transaction.scopes,
        ));
        true
    }

    fn code_context(
        &self,
        digest: &[u8],
        now: i64,
    ) -> OAuthAuthorizationStoreResult<Option<OAuthAuthorizationCodeContext>> {
        validate_time(now)?;
        let Some(code) = self.codes.get(&digest_key(digest)?).filter(|value| {
            value.created_at_ms <= now
                && value.consumed_at_ms.is_none()
                && value.expires_at_ms > now
        }) else {
            return Ok(None);
        };
        let Some(transaction) = self.transactions.get(&code.transaction_id).filter(|value| {
            value.expires_at_ms > now
                && value.decision == Some(OAuthAuthorizationDecision::Approved)
                && value.principal_id.is_some()
        }) else {
            return Ok(None);
        };
        Ok(Some(OAuthAuthorizationCodeContext {
            transaction: transaction.clone(),
        }))
    }

    fn refresh_context(
        &self,
        digest: &[u8],
        now: i64,
    ) -> OAuthAuthorizationStoreResult<Option<OAuthRefreshTokenContext>> {
        validate_time(now)?;
        let Some(token) = self
            .refresh_tokens
            .get(&digest_key(digest)?)
            .filter(|value| value.issued_at_ms <= now && value.expires_at_ms > now)
        else {
            return Ok(None);
        };
        let Some(family) = self
            .families
            .get(&token.family_id)
            .filter(|value| value.revoked_at_ms.is_none() && value.expires_at_ms > now)
        else {
            return Ok(None);
        };
        let Some(grant) = self
            .grants
            .get(&family.grant_id)
            .filter(|value| value.revoked_at_ms.is_none())
        else {
            return Ok(None);
        };
        Ok(Some(OAuthRefreshTokenContext {
            grant: grant.clone(),
            family_expires_at_ms: family.expires_at_ms,
        }))
    }

    fn register_client(
        &mut self,
        config: &OAuthAuthorizationStoreConfig,
        mut client: OAuthClient,
    ) -> OAuthAuthorizationStoreResult<bool> {
        canonical_strings(&mut client.redirect_uris)?;
        if client.client_id.is_empty()
            || client.created_at_ms < 0
            || client
                .client_secret_digest
                .as_ref()
                .is_some_and(|digest| digest.len() < 16)
            || client.redirect_uris.is_empty()
            || client.redirect_uris.len() > config.max_redirect_uris_per_client
            || client
                .redirect_uris
                .iter()
                .any(|uri| crate::validate_public_redirect_uri(uri, true).is_err())
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        if client.registration_method == OAuthClientRegistrationMethod::Dynamic
            && !config.allow_public_client_registration
        {
            return Ok(false);
        }
        let reserved_clients = self
            .clients
            .values()
            .filter(|client| client.registration_method != OAuthClientRegistrationMethod::Dynamic)
            .count();
        if self.clients.contains_key(&client.client_id) || reserved_clients >= config.max_clients {
            return Ok(false);
        }
        self.clients.insert(client.client_id.clone(), client);
        Ok(true)
    }

    fn register_dynamic(
        &mut self,
        config: &OAuthAuthorizationStoreConfig,
        request: OAuthDynamicClientRegistrationRequest,
    ) -> OAuthAuthorizationStoreResult<OAuthDynamicClientRegistrationResult> {
        let mut client = request.client;
        canonical_strings(&mut client.redirect_uris)?;
        if client.registration_method != OAuthClientRegistrationMethod::Dynamic
            || client.client_secret_digest.is_some()
            || client.created_at_ms < 0
            || request.provisional_expires_at_ms <= client.created_at_ms
            || request.inactivity_lifetime_ms <= 0
            || request.rate_limit_window_ms <= 0
            || client.redirect_uris.is_empty()
            || client.redirect_uris.len() > config.max_redirect_uris_per_client
            || client
                .redirect_uris
                .iter()
                .any(|uri| crate::validate_public_redirect_uri(uri, true).is_err())
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        if !config.allow_public_client_registration {
            return Ok(OAuthDynamicClientRegistrationResult::Disabled);
        }
        self.reclaim_dynamic_clients(client.created_at_ms, usize::MAX);
        if self.clients.contains_key(&client.client_id) {
            return Ok(OAuthDynamicClientRegistrationResult::Duplicate);
        }
        if !self.consume_registration_rate(
            "global",
            client.created_at_ms,
            request.rate_limit_window_ms,
            request.global_rate_limit,
        ) || request.trusted_source.as_ref().is_some_and(|source| {
            !self.consume_registration_rate(
                &format!("source:{source}"),
                client.created_at_ms,
                request.rate_limit_window_ms,
                request.source_rate_limit,
            )
        }) {
            return Ok(OAuthDynamicClientRegistrationResult::RateLimited);
        }
        let provisional = self
            .dynamic_clients
            .values()
            .filter(|value| value.promoted_principal_id.is_none())
            .count();
        let active = self
            .dynamic_clients
            .values()
            .filter(|value| value.promoted_principal_id.is_some())
            .count();
        if provisional >= request.max_provisional_clients
            || active >= request.max_active_dynamic_clients
        {
            return Ok(OAuthDynamicClientRegistrationResult::CapacityExceeded);
        }
        let client_id = client.client_id.clone();
        self.dynamic_clients.insert(
            client_id.clone(),
            DynamicClientLifecycle {
                provisional_expires_at_ms: request.provisional_expires_at_ms,
                inactivity_lifetime_ms: request.inactivity_lifetime_ms,
                promoted_principal_id: None,
                promoted_at_ms: None,
                last_used_at_ms: client.created_at_ms,
            },
        );
        self.clients.insert(client_id, client);
        Ok(OAuthDynamicClientRegistrationResult::Registered)
    }

    fn consume_registration_rate(
        &mut self,
        key: &str,
        now: i64,
        window_ms: i64,
        limit: usize,
    ) -> bool {
        if limit == 0 {
            return false;
        }
        let rate = self
            .registration_rates
            .entry(key.to_owned())
            .or_insert_with(|| RegistrationRate {
                window_started_at_ms: now,
                window_expires_at_ms: now.saturating_add(window_ms),
                count: 0,
            });
        if now.saturating_sub(rate.window_started_at_ms) >= window_ms {
            rate.window_started_at_ms = now;
            rate.window_expires_at_ms = now.saturating_add(window_ms);
            rate.count = 0;
        }
        if rate.count >= limit {
            return false;
        }
        rate.count += 1;
        true
    }

    fn cache_metadata(
        &mut self,
        config: &OAuthAuthorizationStoreConfig,
        metadata: OAuthCachedClientMetadata,
    ) -> OAuthAuthorizationStoreResult<()> {
        if metadata.snapshot_id.is_empty()
            || metadata.client_id.is_empty()
            || metadata.document_digest.is_empty()
            || metadata.retrieved_at_ms < 0
            || metadata.retrieved_at_ms > metadata.expires_at_ms
            || metadata.expires_at_ms > metadata.retain_until_ms
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        let admitted_clients = self
            .metadata
            .values()
            .map(|entry| &entry.client_id)
            .collect::<BTreeSet<_>>();
        if !admitted_clients.contains(&metadata.client_id)
            && admitted_clients.len() >= config.max_clients
        {
            return Err(OAuthAuthorizationStoreError::Unavailable);
        }
        if self
            .metadata
            .get(&metadata.snapshot_id)
            .is_some_and(|existing| {
                existing.client_id != metadata.client_id
                    || existing.document != metadata.document
                    || existing.document_digest != metadata.document_digest
            })
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        self.metadata.insert(metadata.snapshot_id.clone(), metadata);
        Ok(())
    }

    fn current_metadata(&self, client_id: &str, now: i64) -> Option<OAuthCachedClientMetadata> {
        if self.revoked_clients.contains_key(client_id) {
            return None;
        }
        self.metadata
            .values()
            .filter(|entry| entry.client_id == client_id && entry.expires_at_ms > now)
            .max_by_key(|entry| (entry.retrieved_at_ms, entry.snapshot_id.as_str()))
            .cloned()
    }

    fn create_transaction(
        &mut self,
        config: &OAuthAuthorizationStoreConfig,
        mut transaction: OAuthAuthorizationTransaction,
    ) -> OAuthAuthorizationStoreResult<bool> {
        canonical_strings(&mut transaction.scopes)?;
        let client = self.clients.get(&transaction.client_id).filter(|client| {
            client.disabled_at_ms.is_none()
                && client
                    .expires_at_ms
                    .is_none_or(|expiry| expiry > transaction.created_at_ms)
        });
        let transaction_created_at = transaction.created_at_ms;
        let metadata_exists = transaction
            .client_metadata_snapshot_id
            .as_ref()
            .is_some_and(|id| {
                self.metadata.get(id).is_some_and(|entry| {
                    entry.client_id == transaction.client_id
                        && entry.expires_at_ms > transaction_created_at
                })
            });
        if transaction.transaction_id.is_empty()
            || transaction.redirect_uri.is_empty()
            || transaction.resource.is_empty()
            || transaction.pkce_challenge.is_empty()
            || transaction.browser_correlation_digest.is_empty()
            || transaction.state_reference.is_empty()
            || transaction.created_at_ms < 0
            || transaction.expires_at_ms <= transaction.created_at_ms
            || (client.is_none() && !metadata_exists)
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        if let Some(client) = client
            && !client.matches_redirect(&transaction.redirect_uri)
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        if let Some(snapshot_id) = &transaction.client_metadata_snapshot_id {
            let metadata = self
                .metadata
                .get(snapshot_id)
                .and_then(metadata_client)
                .ok_or(OAuthAuthorizationStoreError::InvalidData)?;
            if !metadata.matches_redirect(&transaction.redirect_uri) {
                return Err(OAuthAuthorizationStoreError::InvalidData);
            }
        }
        let active = self
            .transactions
            .values()
            .filter(|entry| entry.expires_at_ms > transaction.created_at_ms)
            .count();
        if self.transactions.contains_key(&transaction.transaction_id)
            || active >= config.max_authorization_transactions
        {
            return Ok(false);
        }
        transaction.principal_id = None;
        transaction.decision = None;
        let snapshot_id = transaction.client_metadata_snapshot_id.clone();
        let transaction_expiry = transaction.expires_at_ms;
        self.transactions
            .insert(transaction.transaction_id.clone(), transaction);
        if let Some(metadata) = snapshot_id.and_then(|id| self.metadata.get_mut(&id)) {
            metadata.retain_until_ms = metadata.retain_until_ms.max(transaction_expiry);
        }
        Ok(true)
    }

    fn authenticate(&mut self, id: &str, principal: &McpPrincipalId, now: i64) -> bool {
        let Some(transaction) = self.transactions.get_mut(id).filter(|entry| {
            entry.created_at_ms <= now && entry.expires_at_ms > now && entry.principal_id.is_none()
        }) else {
            return false;
        };
        transaction.principal_id = Some(principal.as_str().to_owned());
        true
    }

    fn authenticate_and_promote(
        &mut self,
        request: &OAuthDynamicClientPromotionRequest,
    ) -> OAuthClientPromotionResult {
        let Some(transaction) = self
            .transactions
            .get(&request.transaction_id)
            .filter(|entry| {
                entry.created_at_ms <= request.now_ms
                    && entry.expires_at_ms > request.now_ms
                    && entry.principal_id.is_none()
            })
        else {
            return OAuthClientPromotionResult::InvalidTransaction;
        };
        let client_id = transaction.client_id.clone();
        if self
            .dynamic_clients
            .get(&client_id)
            .is_some_and(|lifecycle| lifecycle.promoted_principal_id.is_none())
        {
            if self.dynamic_clients[&client_id].provisional_expires_at_ms <= request.now_ms {
                return OAuthClientPromotionResult::InvalidTransaction;
            }
            let active = self
                .dynamic_clients
                .values()
                .filter(|value| value.promoted_principal_id.is_some())
                .count();
            let principal_clients = self
                .dynamic_clients
                .values()
                .filter(|value| {
                    value.promoted_principal_id.as_deref() == Some(request.principal_id.as_str())
                })
                .count();
            if active >= request.max_active_dynamic_clients
                || principal_clients >= request.max_clients_per_principal
            {
                return OAuthClientPromotionResult::QuotaExceeded;
            }
        }
        self.transactions
            .get_mut(&request.transaction_id)
            .expect("validated transaction")
            .principal_id = Some(request.principal_id.as_str().to_owned());
        if let Some(lifecycle) = self.dynamic_clients.get_mut(&client_id) {
            lifecycle
                .promoted_principal_id
                .get_or_insert_with(|| request.principal_id.as_str().to_owned());
            lifecycle.promoted_at_ms.get_or_insert(request.now_ms);
            lifecycle.last_used_at_ms = request.now_ms;
        }
        OAuthClientPromotionResult::Authenticated
    }

    fn decide(&mut self, id: &str, decision: OAuthAuthorizationDecision, now: i64) -> bool {
        let Some(transaction) = self.transactions.get_mut(id).filter(|entry| {
            entry.created_at_ms <= now
                && entry.expires_at_ms > now
                && entry.principal_id.is_some()
                && entry.decision.is_none()
        }) else {
            return false;
        };
        transaction.decision = Some(decision);
        true
    }

    fn issue_code(&mut self, code: OAuthAuthorizationCode) -> OAuthAuthorizationStoreResult<bool> {
        let key = digest_key(&code.code_digest)?;
        let approved = self
            .transactions
            .get(&code.transaction_id)
            .is_some_and(|entry| {
                entry.decision == Some(OAuthAuthorizationDecision::Approved)
                    && entry.principal_id.is_some()
                    && entry.expires_at_ms > code.created_at_ms
            });
        if !approved
            || code.created_at_ms < 0
            || code.expires_at_ms <= code.created_at_ms
            || self
                .transactions
                .get(&code.transaction_id)
                .is_none_or(|transaction| {
                    code.created_at_ms < transaction.created_at_ms
                        || code.expires_at_ms > transaction.expires_at_ms
                })
            || code.consumed_at_ms.is_some()
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        if self.codes.contains_key(&key) {
            return Ok(false);
        }
        if let Some(snapshot_id) = self
            .transactions
            .get(&code.transaction_id)
            .and_then(|transaction| transaction.client_metadata_snapshot_id.as_ref())
            && let Some(metadata) = self.metadata.get_mut(snapshot_id)
        {
            metadata.retain_until_ms = metadata.retain_until_ms.max(code.expires_at_ms);
        }
        self.codes.insert(key, code);
        Ok(true)
    }

    #[allow(clippy::too_many_lines)]
    fn redeem(
        &mut self,
        config: &OAuthAuthorizationStoreConfig,
        request: OAuthIssueGrantRequest,
    ) -> OAuthAuthorizationStoreResult<OAuthIssueGrantResult> {
        validate_token_metadata(&request.access_token, request.now_ms)?;
        let key = digest_key(&request.code_digest)?;
        let Some(code) = self.codes.get(&key) else {
            return Ok(OAuthIssueGrantResult::InvalidGrant);
        };
        let Some(transaction) = self.transactions.get(&code.transaction_id) else {
            return Ok(OAuthIssueGrantResult::InvalidGrant);
        };
        let snapshot_valid = transaction
            .client_metadata_snapshot_id
            .as_ref()
            .is_none_or(|id| {
                self.metadata.get(id).is_some_and(|metadata| {
                    metadata.client_id == transaction.client_id
                        && metadata_client(metadata).is_some_and(|client| {
                            client.matches_redirect(&transaction.redirect_uri)
                        })
                })
            });
        let valid = code.consumed_at_ms.is_none()
            && code.created_at_ms <= request.now_ms
            && code.expires_at_ms > request.now_ms
            && transaction.created_at_ms <= request.now_ms
            && transaction.expires_at_ms > request.now_ms
            && transaction.decision == Some(OAuthAuthorizationDecision::Approved)
            && transaction.client_id == request.client_id
            && transaction.redirect_uri == request.redirect_uri
            && transaction.resource == request.resource
            && constant_time_equal(
                transaction.pkce_challenge.as_bytes(),
                request.pkce_challenge.as_bytes(),
            )
            && transaction.principal_id.is_some()
            && snapshot_valid
            && self.clients.get(&request.client_id).is_none_or(|client| {
                client.disabled_at_ms.is_none()
                    && client
                        .expires_at_ms
                        .is_none_or(|expiry| expiry > request.now_ms)
            });
        if !valid {
            return Ok(OAuthIssueGrantResult::InvalidGrant);
        }
        if request.grant_id.is_empty()
            || request.refresh_family_id.is_empty()
            || request.refresh_token_expires_at_ms <= request.now_ms
            || request.refresh_family_expires_at_ms < request.refresh_token_expires_at_ms
            || self.grants.contains_key(&request.grant_id)
            || self.families.contains_key(&request.refresh_family_id)
            || self.access_tokens.contains_key(&request.access_token.jti)
            || self.grants.len() >= config.max_grants
            || self.families.len() >= config.max_refresh_families
            || self.access_tokens.len() >= config.max_access_tokens
            || self
                .refresh_tokens
                .contains_key(&digest_key(&request.refresh_token_digest)?)
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        let principal = transaction
            .principal_id
            .clone()
            .expect("validated principal");
        let grant = OAuthGrant {
            grant_id: request.grant_id.clone(),
            principal_id: principal.clone(),
            client_id: request.client_id.clone(),
            resource: request.resource.clone(),
            scopes: transaction.scopes.clone(),
            created_at_ms: request.now_ms,
            revoked_at_ms: None,
        };
        let family = OAuthRefreshFamily {
            family_id: request.refresh_family_id.clone(),
            grant_id: request.grant_id.clone(),
            created_at_ms: request.now_ms,
            expires_at_ms: request.refresh_family_expires_at_ms,
            revoked_at_ms: None,
            revocation_reason: None,
        };
        let refresh = RefreshTokenRecord {
            digest: request.refresh_token_digest.clone(),
            family_id: request.refresh_family_id.clone(),
            generation: 0,
            issued_at_ms: request.now_ms,
            expires_at_ms: request.refresh_token_expires_at_ms,
            consumed_at_ms: None,
            replacement_generation: None,
        };
        let access = OAuthAccessTokenRecord {
            jti: request.access_token.jti.clone(),
            signing_key_id: request.access_token.signing_key_id,
            grant_id: request.grant_id.clone(),
            refresh_family_id: Some(request.refresh_family_id.clone()),
            client_id: request.client_id,
            principal_id: principal,
            resource: request.resource,
            scopes: transaction.scopes.clone(),
            issued_at_ms: request.access_token.issued_at_ms,
            expires_at_ms: request.access_token.expires_at_ms,
            revoked_at_ms: None,
        };
        self.codes
            .get_mut(&key)
            .expect("code exists")
            .consumed_at_ms = Some(request.now_ms);
        self.grants.insert(grant.grant_id.clone(), grant.clone());
        self.families.insert(family.family_id.clone(), family);
        self.refresh_tokens
            .insert(digest_key(&refresh.digest)?, refresh);
        self.access_tokens.insert(access.jti.clone(), access);
        if let Some(lifecycle) = self.dynamic_clients.get_mut(&grant.client_id) {
            lifecycle.last_used_at_ms = request.now_ms;
        }
        Ok(OAuthIssueGrantResult::Issued(Box::new(grant)))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "refresh rotation is one atomic validation and state transition"
    )]
    fn rotate(
        &mut self,
        config: &OAuthAuthorizationStoreConfig,
        mut request: OAuthRefreshRotationRequest,
    ) -> OAuthAuthorizationStoreResult<OAuthRefreshRotationResult> {
        canonical_strings(&mut request.scopes)?;
        validate_token_metadata(&request.access_token, request.now_ms)?;
        let key = digest_key(&request.refresh_token_digest)?;
        let Some(current) = self.refresh_tokens.get(&key).cloned() else {
            return Ok(OAuthRefreshRotationResult::InvalidGrant);
        };
        if current.consumed_at_ms.is_some() {
            self.revoke_family(&current.family_id, request.now_ms, "refresh_replay");
            return Ok(OAuthRefreshRotationResult::ReplayDetected);
        }
        let Some(family) = self.families.get(&current.family_id) else {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        };
        let Some(grant) = self.grants.get(&family.grant_id) else {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        };
        let valid = current.expires_at_ms > request.now_ms
            && current.issued_at_ms <= request.now_ms
            && family.created_at_ms <= request.now_ms
            && family.expires_at_ms > request.now_ms
            && family.revoked_at_ms.is_none()
            && grant.revoked_at_ms.is_none()
            && grant.created_at_ms <= request.now_ms
            && grant.client_id == request.client_id
            && grant.resource == request.resource
            && request
                .scopes
                .iter()
                .all(|scope| grant.scopes.contains(scope))
            && self.clients.get(&grant.client_id).is_none_or(|client| {
                client.disabled_at_ms.is_none()
                    && client
                        .expires_at_ms
                        .is_none_or(|expiry| expiry > request.now_ms)
            });
        if !valid {
            return Ok(OAuthRefreshRotationResult::InvalidGrant);
        }
        let replacement_key = digest_key(&request.replacement_refresh_token_digest)?;
        if self.refresh_tokens.contains_key(&replacement_key)
            || self.access_tokens.contains_key(&request.access_token.jti)
            || request.replacement_refresh_token_expires_at_ms <= request.now_ms
            || request.replacement_refresh_token_expires_at_ms > family.expires_at_ms
            || self.access_tokens.len() >= config.max_access_tokens
        {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(OAuthAuthorizationStoreError::InvalidData)?;
        let replacement = RefreshTokenRecord {
            digest: request.replacement_refresh_token_digest,
            family_id: family.family_id.clone(),
            generation,
            issued_at_ms: request.now_ms,
            expires_at_ms: request.replacement_refresh_token_expires_at_ms,
            consumed_at_ms: None,
            replacement_generation: None,
        };
        let dynamic_client_id = grant.client_id.clone();
        let access = OAuthAccessTokenRecord {
            jti: request.access_token.jti.clone(),
            signing_key_id: request.access_token.signing_key_id,
            grant_id: grant.grant_id.clone(),
            refresh_family_id: Some(family.family_id.clone()),
            client_id: grant.client_id.clone(),
            principal_id: grant.principal_id.clone(),
            resource: grant.resource.clone(),
            scopes: request.scopes,
            issued_at_ms: request.access_token.issued_at_ms,
            expires_at_ms: request.access_token.expires_at_ms,
            revoked_at_ms: None,
        };
        let consumed = self.refresh_tokens.get_mut(&key).expect("refresh exists");
        consumed.consumed_at_ms = Some(request.now_ms);
        consumed.replacement_generation = Some(generation);
        self.refresh_tokens.insert(replacement_key, replacement);
        self.access_tokens.insert(access.jti.clone(), access);
        if let Some(lifecycle) = self.dynamic_clients.get_mut(&dynamic_client_id) {
            lifecycle.last_used_at_ms = request.now_ms;
        }
        Ok(OAuthRefreshRotationResult::Rotated { generation })
    }

    fn revoke(
        &mut self,
        target: OAuthRevocationTarget,
        now: i64,
    ) -> OAuthAuthorizationStoreResult<()> {
        validate_time(now)?;
        match target {
            OAuthRevocationTarget::AccessToken { jti, client_id } => {
                if let Some(token) = self
                    .access_tokens
                    .get_mut(&jti)
                    .filter(|token| token.client_id == client_id)
                {
                    token.revoked_at_ms.get_or_insert(now);
                }
            }
            OAuthRevocationTarget::RefreshToken { digest, client_id } => {
                if let Some(token) = self.refresh_tokens.get(&digest_key(&digest)?).cloned()
                    && self
                        .families
                        .get(&token.family_id)
                        .and_then(|family| self.grants.get(&family.grant_id))
                        .is_some_and(|grant| grant.client_id == client_id)
                {
                    self.revoke_family(&token.family_id, now, "token_revocation");
                }
            }
            OAuthRevocationTarget::RefreshFamily(id) => {
                self.revoke_family(&id, now, "administrative");
            }
            OAuthRevocationTarget::Grant(id) => {
                if let Some(grant) = self.grants.get_mut(&id) {
                    grant.revoked_at_ms.get_or_insert(now);
                }
                let families = self
                    .families
                    .values()
                    .filter(|family| family.grant_id == id)
                    .map(|family| family.family_id.clone())
                    .collect::<Vec<_>>();
                for family in families {
                    self.revoke_family(&family, now, "grant_revocation");
                }
                for token in self
                    .access_tokens
                    .values_mut()
                    .filter(|token| token.grant_id == id)
                {
                    token.revoked_at_ms.get_or_insert(now);
                }
            }
            OAuthRevocationTarget::Client(id) => {
                self.revoked_clients.entry(id.clone()).or_insert(now);
                if let Some(client) = self.clients.get_mut(&id) {
                    client.disabled_at_ms.get_or_insert(now);
                }
                let grants = self
                    .grants
                    .values()
                    .filter(|grant| grant.client_id == id)
                    .map(|grant| grant.grant_id.clone())
                    .collect::<Vec<_>>();
                for grant in grants {
                    self.revoke(OAuthRevocationTarget::Grant(grant), now)?;
                }
            }
        }
        Ok(())
    }

    fn revoke_family(&mut self, id: &str, now: i64, reason: &str) {
        if let Some(family) = self.families.get_mut(id) {
            family.revoked_at_ms.get_or_insert(now);
            family
                .revocation_reason
                .get_or_insert_with(|| reason.to_owned());
        }
        for token in self
            .access_tokens
            .values_mut()
            .filter(|token| token.refresh_family_id.as_deref() == Some(id))
        {
            token.revoked_at_ms.get_or_insert(now);
        }
    }

    fn validate_access(
        &self,
        mut request: OAuthValidateAccessTokenRequest,
    ) -> OAuthAuthorizationStoreResult<bool> {
        validate_time(request.now_ms)?;
        canonical_strings(&mut request.scopes)?;
        let Some(token) = self.access_tokens.get(&request.jti) else {
            return Ok(false);
        };
        let Some(grant) = self.grants.get(&request.grant_id) else {
            return Ok(false);
        };
        let family_valid = token.refresh_family_id.as_ref().is_none_or(|id| {
            self.families.get(id).is_some_and(|family| {
                family.revoked_at_ms.is_none() && family.expires_at_ms > request.now_ms
            })
        });
        Ok(token.revoked_at_ms.is_none()
            && token.issued_at_ms <= request.now_ms
            && token.expires_at_ms > request.now_ms
            && grant.created_at_ms <= request.now_ms
            && grant.revoked_at_ms.is_none()
            && self.clients.get(&token.client_id).is_none_or(|client| {
                client.disabled_at_ms.is_none()
                    && client
                        .expires_at_ms
                        .is_none_or(|expiry| expiry > request.now_ms)
            })
            && family_valid
            && token.client_id == request.client_id
            && token.grant_id == request.grant_id
            && token.principal_id == request.principal_id.as_str()
            && token.resource == request.resource
            && token.scopes == request.scopes)
    }

    fn put_key(
        &mut self,
        candidate: OAuthSigningKeyCandidate,
        bootstrap: bool,
    ) -> OAuthAuthorizationStoreResult<OAuthSigningKey> {
        validate_candidate(&candidate)?;
        let map_key = signing_key_map_key(&candidate.issuer, &candidate.key_id);
        if let Some(existing) = self.signing_keys.get(&map_key) {
            if existing.algorithm != candidate.algorithm
                || existing.public_jwk != candidate.public_jwk
                || existing.encrypted_private_key.as_ref() != Some(&candidate.encrypted_private_key)
                || existing.created_at_ms != candidate.created_at_ms
            {
                return Err(OAuthAuthorizationStoreError::InvalidData);
            }
            return Ok(existing.clone());
        }
        let has_any = self
            .signing_keys
            .values()
            .any(|key| key.issuer == candidate.issuer);
        let active = bootstrap && !has_any;
        let key = OAuthSigningKey {
            issuer: candidate.issuer,
            key_id: candidate.key_id,
            algorithm: candidate.algorithm,
            public_jwk: candidate.public_jwk,
            encrypted_private_key: Some(candidate.encrypted_private_key),
            state: if active {
                OAuthSigningKeyState::Active
            } else {
                OAuthSigningKeyState::Pending
            },
            created_at_ms: candidate.created_at_ms,
            activated_at_ms: active.then_some(candidate.created_at_ms),
            retired_at_ms: None,
            verify_until_ms: None,
            private_until_ms: None,
        };
        validate_signing_key_record(&key)?;
        self.signing_keys.insert(map_key, key.clone());
        Ok(key)
    }

    #[allow(clippy::too_many_arguments)]
    fn activate_key(
        &mut self,
        issuer: &str,
        key_id: &str,
        now: i64,
        verify_until: i64,
        private_until: i64,
    ) -> OAuthAuthorizationStoreResult<bool> {
        if now < 0 || private_until < now || verify_until < private_until {
            return Err(OAuthAuthorizationStoreError::InvalidData);
        }
        let target_key = signing_key_map_key(issuer, key_id);
        if !self.signing_keys.get(&target_key).is_some_and(|key| {
            key.state == OAuthSigningKeyState::Pending
                && key.encrypted_private_key.is_some()
                && key.created_at_ms <= now
        }) {
            return Ok(false);
        }
        for key in self
            .signing_keys
            .values_mut()
            .filter(|key| key.issuer == issuer && key.state == OAuthSigningKeyState::Active)
        {
            key.state = OAuthSigningKeyState::Retired;
            key.retired_at_ms = Some(now);
            key.verify_until_ms = Some(verify_until);
            key.private_until_ms = Some(private_until);
        }
        let target = self
            .signing_keys
            .get_mut(&target_key)
            .expect("candidate exists");
        target.state = OAuthSigningKeyState::Active;
        target.activated_at_ms = Some(now);
        Ok(true)
    }

    #[allow(clippy::too_many_lines)]
    fn cleanup(&mut self, now: i64, limit: usize) -> OAuthCleanupResult {
        let mut result = OAuthCleanupResult {
            records_deleted: self.reclaim_dynamic_clients(now, limit),
            ..OAuthCleanupResult::default()
        };
        let expired_rates = self
            .registration_rates
            .iter()
            .filter(|(_, rate)| rate.window_expires_at_ms <= now)
            .map(|(key, _)| key.clone())
            .take(limit.saturating_sub(result.records_deleted))
            .collect::<Vec<_>>();
        for key in expired_rates {
            self.registration_rates.remove(&key);
            result.records_deleted += 1;
        }
        let mut transient = self
            .transactions
            .iter()
            .filter(|(_, value)| value.expires_at_ms <= now)
            .map(|(id, value)| (value.expires_at_ms, id.clone(), 0_u8))
            .chain(
                self.codes
                    .iter()
                    .filter(|(_, value)| value.expires_at_ms <= now)
                    .map(|(id, value)| (value.expires_at_ms, id.clone(), 1_u8)),
            )
            .chain(
                self.metadata
                    .iter()
                    .filter(|(_, value)| value.retain_until_ms <= now)
                    .map(|(id, value)| (value.retain_until_ms, id.clone(), 2_u8)),
            )
            .collect::<Vec<_>>();
        transient.sort();
        for (_, id, kind) in transient
            .into_iter()
            .take(limit.saturating_sub(result.records_deleted))
        {
            match kind {
                0 => {
                    self.transactions.remove(&id);
                }
                1 => {
                    self.codes.remove(&id);
                }
                _ => {
                    self.metadata.remove(&id);
                }
            }
            result.records_deleted += 1;
        }
        let remaining = limit.saturating_sub(result.records_deleted);
        let mut tokens = self
            .access_tokens
            .iter()
            .filter(|(_, value)| value.expires_at_ms <= now || value.revoked_at_ms.is_some())
            .map(|(id, value)| (value.expires_at_ms, id.clone(), 0_u8))
            .chain(
                self.refresh_tokens
                    .iter()
                    .filter(|(_, value)| value.expires_at_ms <= now)
                    .map(|(id, value)| (value.expires_at_ms, id.clone(), 1_u8)),
            )
            .chain(
                self.families
                    .iter()
                    .filter(|(_, value)| {
                        value.expires_at_ms <= now || value.revoked_at_ms.is_some()
                    })
                    .map(|(id, value)| (value.expires_at_ms, id.clone(), 2_u8)),
            )
            .collect::<Vec<_>>();
        tokens.sort();
        for (_, id, kind) in tokens.into_iter().take(remaining) {
            match kind {
                0 => {
                    self.access_tokens.remove(&id);
                }
                1 => {
                    self.refresh_tokens.remove(&id);
                }
                _ => {
                    self.families.remove(&id);
                }
            }
            result.records_deleted += 1;
        }
        let mut keys = self
            .signing_keys
            .iter()
            .filter(|(_, key)| key.state == OAuthSigningKeyState::Retired)
            .map(|(id, key)| (id.clone(), key.private_until_ms, key.verify_until_ms))
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| left.0.cmp(&right.0));
        for (id, private_until, verify_until) in keys
            .into_iter()
            .take(limit.saturating_sub(result.records_deleted))
        {
            if let Some(key) = self.signing_keys.get_mut(&id)
                && key.encrypted_private_key.is_some()
                && private_until.is_some_and(|deadline| deadline <= now)
            {
                key.encrypted_private_key = None;
                result.private_keys_removed += 1;
            }
            if verify_until.is_some_and(|deadline| deadline <= now) {
                self.signing_keys.remove(&id);
                result.signing_keys_deleted += 1;
            }
        }
        result
    }

    fn reclaim_dynamic_clients(&mut self, now: i64, limit: usize) -> usize {
        let reclaimable = self
            .dynamic_clients
            .iter()
            .filter(|(client_id, lifecycle)| {
                let client_id = client_id.as_str();
                let expired_provisional = lifecycle.promoted_principal_id.is_none()
                    && lifecycle.provisional_expires_at_ms <= now;
                let inactive = lifecycle.promoted_principal_id.is_some()
                    && lifecycle
                        .last_used_at_ms
                        .saturating_add(lifecycle.inactivity_lifetime_ms)
                        <= now
                    && !self.grants.values().any(|grant| {
                        grant.client_id == client_id
                            && grant.revoked_at_ms.is_none()
                            && (self.access_tokens.values().any(|token| {
                                token.grant_id == grant.grant_id
                                    && token.revoked_at_ms.is_none()
                                    && token.expires_at_ms > now
                            }) || self.families.values().any(|family| {
                                family.grant_id == grant.grant_id
                                    && family.revoked_at_ms.is_none()
                                    && family.expires_at_ms > now
                            }))
                    })
                    && !self.families.values().any(|family| {
                        family.revoked_at_ms.is_none()
                            && family.expires_at_ms > now
                            && self
                                .grants
                                .get(&family.grant_id)
                                .is_some_and(|grant| grant.client_id == client_id)
                    });
                expired_provisional || inactive
            })
            .map(|(client_id, _)| client_id.clone())
            .take(limit)
            .collect::<Vec<_>>();
        for client_id in &reclaimable {
            self.dynamic_clients.remove(client_id);
            self.clients.remove(client_id);
        }
        reclaimable.len()
    }
}

macro_rules! memory_method {
    ($name:ident($($argument:ident: $type:ty),*) -> $output:ty, $body:expr) => {
        fn $name(&self, $($argument: $type),*) -> BoxFuture<OAuthAuthorizationStoreResult<$output>> {
            let state = Arc::clone(&self.state); let config = self.config.clone();
            Box::pin(async move { let mut state = state.lock().await; ($body)(&mut state, &config, $($argument),*) })
        }
    };
}

impl OAuthAuthorizationStore for InMemoryOAuthAuthorizationStore {
    fn config(&self) -> OAuthAuthorizationStoreConfig {
        self.config.clone()
    }
    memory_method!(register_client(client: OAuthClient) -> bool, |state: &mut OAuthState, config: &OAuthAuthorizationStoreConfig, client| state.register_client(config, client));
    memory_method!(register_dynamic_client(request: OAuthDynamicClientRegistrationRequest) -> OAuthDynamicClientRegistrationResult, |state: &mut OAuthState, config: &OAuthAuthorizationStoreConfig, request| state.register_dynamic(config, request));
    memory_method!(cache_client_metadata(metadata: OAuthCachedClientMetadata) -> (), |state: &mut OAuthState, config: &OAuthAuthorizationStoreConfig, metadata| state.cache_metadata(config, metadata));
    memory_method!(client_metadata(client_id: String, now_ms: i64) -> Option<OAuthCachedClientMetadata>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, client_id: String, now_ms| { validate_time(now_ms)?; Ok(state.current_metadata(&client_id, now_ms)) });
    memory_method!(create_authorization_transaction(transaction: OAuthAuthorizationTransaction) -> bool, |state: &mut OAuthState, config: &OAuthAuthorizationStoreConfig, transaction| state.create_transaction(config, transaction));
    memory_method!(client(client_id: String, now_ms: i64) -> Option<OAuthClient>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, client_id: String, now_ms| { validate_time(now_ms)?; Ok(state.client(&client_id, now_ms)) });
    memory_method!(authorization_context(transaction_id: String, now_ms: i64) -> Option<OAuthAuthorizationContext>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, transaction_id: String, now_ms| { validate_time(now_ms)?; Ok(state.authorization_context(&transaction_id, now_ms)) });
    memory_method!(consume_browser_correlation(transaction_id: String, correlation_digest: Vec<u8>, now_ms: i64) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, transaction_id: String, correlation_digest: Vec<u8>, now_ms| state.consume_correlation(&transaction_id, &correlation_digest, now_ms));
    memory_method!(authenticate_authorization_transaction(transaction_id: String, principal_id: McpPrincipalId, now_ms: i64) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, transaction_id: String, principal_id, now_ms| { validate_time(now_ms)?; Ok(state.authenticate(&transaction_id, &principal_id, now_ms)) });
    memory_method!(resume_authorization_transaction(request: OAuthResumeAuthenticationRequest) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, request: OAuthResumeAuthenticationRequest| state.resume_authenticate(&request));
    memory_method!(authenticate_and_promote_client(request: OAuthDynamicClientPromotionRequest) -> OAuthClientPromotionResult, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, request: OAuthDynamicClientPromotionRequest| { validate_time(request.now_ms)?; Ok(state.authenticate_and_promote(&request)) });
    memory_method!(resume_and_promote_client(request: OAuthResumeDynamicClientPromotionRequest) -> OAuthClientPromotionResult, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, request: OAuthResumeDynamicClientPromotionRequest| state.resume_and_promote(&request));
    memory_method!(decide_authorization_transaction(transaction_id: String, decision: OAuthAuthorizationDecision, now_ms: i64) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, transaction_id: String, decision, now_ms| { validate_time(now_ms)?; Ok(state.decide(&transaction_id, decision, now_ms)) });
    memory_method!(remember_authorization_consent(transaction_id: String, now_ms: i64) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, transaction_id: String, now_ms| { validate_time(now_ms)?; Ok(state.remember_consent(&transaction_id, now_ms)) });
    memory_method!(issue_authorization_code(code: OAuthAuthorizationCode) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, code| state.issue_code(code));
    memory_method!(authorization_code_context(code_digest: Vec<u8>, now_ms: i64) -> Option<OAuthAuthorizationCodeContext>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, code_digest: Vec<u8>, now_ms| state.code_context(&code_digest, now_ms));
    memory_method!(redeem_authorization_code(request: OAuthIssueGrantRequest) -> OAuthIssueGrantResult, |state: &mut OAuthState, config: &OAuthAuthorizationStoreConfig, request| state.redeem(config, request));
    memory_method!(rotate_refresh_token(request: OAuthRefreshRotationRequest) -> OAuthRefreshRotationResult, |state: &mut OAuthState, config: &OAuthAuthorizationStoreConfig, request| state.rotate(config, request));
    memory_method!(refresh_token_context(refresh_token_digest: Vec<u8>, now_ms: i64) -> Option<OAuthRefreshTokenContext>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, refresh_token_digest: Vec<u8>, now_ms| state.refresh_context(&refresh_token_digest, now_ms));
    memory_method!(revoke(target: OAuthRevocationTarget, now_ms: i64) -> (), |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, target, now_ms| state.revoke(target, now_ms));
    memory_method!(validate_access_token(request: OAuthValidateAccessTokenRequest) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, request| state.validate_access(request));
    memory_method!(put_signing_key_candidate(candidate: OAuthSigningKeyCandidate, bootstrap: bool) -> OAuthSigningKey, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, candidate, bootstrap| state.put_key(candidate, bootstrap));
    memory_method!(activate_signing_key(issuer: String, key_id: String, now_ms: i64, verify_until_ms: i64, private_until_ms: i64) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, issuer: String, key_id: String, now_ms, verify_until_ms, private_until_ms| state.activate_key(&issuer, &key_id, now_ms, verify_until_ms, private_until_ms));
    memory_method!(active_signing_key(issuer: String) -> Option<OAuthSigningKey>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, issuer| Ok(state.signing_keys.values().find(|key| key.issuer == issuer && key.state == OAuthSigningKeyState::Active).cloned()));
    memory_method!(verification_signing_keys(issuer: String, now_ms: i64) -> Vec<OAuthSigningKey>, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, issuer, now_ms| { validate_time(now_ms)?; Ok(state.signing_keys.values().filter(|key| key.issuer == issuer && (key.state == OAuthSigningKeyState::Active || key.state == OAuthSigningKeyState::Retired && key.verify_until_ms.is_some_and(|deadline| deadline > now_ms))).cloned().collect()) });
    memory_method!(rewrap_signing_key(issuer: String, key_id: String, encrypted: OAuthEncryptedPrivateKey) -> bool, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, issuer: String, key_id: String, encrypted| { validate_encrypted_private_key(&encrypted)?; let Some(key) = state.signing_keys.get_mut(&signing_key_map_key(&issuer, &key_id)).filter(|key| key.encrypted_private_key.is_some()) else { return Ok(false); }; key.encrypted_private_key = Some(encrypted); Ok(true) });
    memory_method!(cleanup(now_ms: i64, limit: usize) -> OAuthCleanupResult, |state: &mut OAuthState, _config: &OAuthAuthorizationStoreConfig, now_ms, limit| { validate_time(now_ms)?; Ok(state.cleanup(now_ms, limit)) });
}

fn canonical_strings(values: &mut Vec<String>) -> OAuthAuthorizationStoreResult<()> {
    if values.iter().any(String::is_empty) {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    values.sort();
    values.dedup();
    Ok(())
}
fn metadata_client(metadata: &OAuthCachedClientMetadata) -> Option<OAuthClient> {
    let document: crate::McpClientMetadataDocument =
        serde_json::from_value(metadata.document.clone()).ok()?;
    document
        .validate_public_client(&metadata.client_id, true)
        .ok()?;
    Some(OAuthClient {
        client_id: metadata.client_id.clone(),
        registration_method: OAuthClientRegistrationMethod::ClientMetadataDocument,
        display_name: document.client_name,
        client_secret_digest: None,
        redirect_uris: document.redirect_uris,
        created_at_ms: metadata.retrieved_at_ms,
        expires_at_ms: None,
        disabled_at_ms: None,
    })
}
fn digest_key(digest: &[u8]) -> OAuthAuthorizationStoreResult<String> {
    if digest.len() < 16 {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    Ok(URL_SAFE_NO_PAD.encode(digest))
}
const fn validate_time(value: i64) -> OAuthAuthorizationStoreResult<()> {
    if value < 0 {
        Err(OAuthAuthorizationStoreError::InvalidData)
    } else {
        Ok(())
    }
}
fn signing_key_map_key(issuer: &str, key_id: &str) -> String {
    format!("{}:{issuer}{key_id}", issuer.len())
}
fn consent_key(principal: &str, client: &str, resource: &str, scopes: &[String]) -> String {
    serde_json::to_string(&(principal, client, resource, scopes)).expect("consent key serializes")
}
fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}
const fn validate_token_metadata(
    token: &OAuthTokenMetadata,
    now: i64,
) -> OAuthAuthorizationStoreResult<()> {
    if token.jti.is_empty()
        || token.signing_key_id.is_empty()
        || token.issued_at_ms < 0
        || token.issued_at_ms > now
        || token.expires_at_ms <= now
    {
        Err(OAuthAuthorizationStoreError::InvalidData)
    } else {
        Ok(())
    }
}
fn validate_candidate(candidate: &OAuthSigningKeyCandidate) -> OAuthAuthorizationStoreResult<()> {
    if candidate.issuer.is_empty()
        || candidate.key_id.is_empty()
        || candidate.algorithm != "ES256"
        || candidate.created_at_ms < 0
    {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    validate_public_jwk(&candidate.public_jwk, &candidate.key_id)?;
    validate_encrypted_private_key(&candidate.encrypted_private_key)
}
fn validate_signing_key_record(key: &OAuthSigningKey) -> OAuthAuthorizationStoreResult<()> {
    if key.issuer.is_empty() || key.key_id.is_empty() || key.algorithm != "ES256" {
        return Err(OAuthAuthorizationStoreError::InvalidData);
    }
    validate_time(key.created_at_ms)?;
    validate_public_jwk(&key.public_jwk, &key.key_id)?;
    if let Some(encrypted) = &key.encrypted_private_key {
        validate_encrypted_private_key(encrypted)?;
    }
    let valid_state = match key.state {
        OAuthSigningKeyState::Pending => {
            key.encrypted_private_key.is_some()
                && key.activated_at_ms.is_none()
                && key.retired_at_ms.is_none()
                && key.verify_until_ms.is_none()
                && key.private_until_ms.is_none()
        }
        OAuthSigningKeyState::Active => {
            key.encrypted_private_key.is_some()
                && key
                    .activated_at_ms
                    .is_some_and(|value| value >= key.created_at_ms)
                && key.retired_at_ms.is_none()
                && key.verify_until_ms.is_none()
                && key.private_until_ms.is_none()
        }
        OAuthSigningKeyState::Retired => key
            .activated_at_ms
            .zip(key.retired_at_ms)
            .zip(key.private_until_ms)
            .zip(key.verify_until_ms)
            .is_some_and(|(((activated, retired), private_until), verify_until)| {
                activated >= key.created_at_ms
                    && retired >= activated
                    && private_until >= retired
                    && verify_until >= private_until
            }),
    };
    if valid_state {
        Ok(())
    } else {
        Err(OAuthAuthorizationStoreError::InvalidData)
    }
}
fn validate_public_jwk(jwk: &serde_json::Value, key_id: &str) -> OAuthAuthorizationStoreResult<()> {
    let coordinate = |name| {
        jwk.get(name)
            .and_then(serde_json::Value::as_str)
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
            .is_some_and(|value| value.len() == 32)
    };
    if jwk.get("kty").and_then(serde_json::Value::as_str) == Some("EC")
        && jwk.get("crv").and_then(serde_json::Value::as_str) == Some("P-256")
        && jwk.get("use").and_then(serde_json::Value::as_str) == Some("sig")
        && jwk.get("alg").and_then(serde_json::Value::as_str) == Some("ES256")
        && jwk.get("kid").and_then(serde_json::Value::as_str) == Some(key_id)
        && coordinate("x")
        && coordinate("y")
    {
        Ok(())
    } else {
        Err(OAuthAuthorizationStoreError::InvalidData)
    }
}
const fn validate_encrypted_private_key(
    encrypted: &OAuthEncryptedPrivateKey,
) -> OAuthAuthorizationStoreResult<()> {
    if encrypted.wrapping_key_id.is_empty()
        || encrypted.nonce.len() != 12
        || encrypted.ciphertext.len() < 16
    {
        Err(OAuthAuthorizationStoreError::InvalidData)
    } else {
        Ok(())
    }
}
