use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Durable credential stored outside shared config.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum StoredCredential {
    /// OAuth access and refresh tokens.
    #[serde(rename = "o_auth", alias = "oauth")]
    OAuth(OAuthCredential),
    /// Standards-based OAuth state for an MCP server.
    #[serde(rename = "mcp_oauth")]
    McpOAuth(McpOAuthState),
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OAuth(_) => formatter.write_str("OAuth(<redacted>)"),
            Self::McpOAuth(state) => formatter.debug_tuple("McpOAuth").field(state).finish(),
        }
    }
}

/// OAuth token material and non-secret metadata.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// OAuth access token.
    pub access: String,
    /// OAuth refresh token.
    pub refresh: String,
    /// Expiration timestamp in Unix milliseconds.
    pub expires_at_ms: i64,
    /// Non-secret provider-specific metadata.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl std::fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let metadata_keys = self.metadata.keys().collect::<Vec<_>>();
        formatter
            .debug_struct("OAuthCredential")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("metadata_keys", &metadata_keys)
            .finish()
    }
}

/// Durable, version-tolerant OAuth state for an MCP server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthState {
    /// Stored representation version.
    #[serde(default = "mcp_oauth_state_version")]
    pub version: u32,
    /// Exact protected resource identifier.
    #[serde(default)]
    pub resource: String,
    /// Selected authorization server issuer.
    #[serde(default)]
    pub issuer: String,
    /// Client registration used for this resource and issuer.
    #[serde(default)]
    pub registration: McpOAuthRegistration,
    /// Discovered authorization server endpoints.
    #[serde(default)]
    pub endpoints: McpOAuthEndpoints,
    /// Current token family state.
    #[serde(default)]
    pub token: McpOAuthToken,
}

impl std::fmt::Debug for McpOAuthState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthState")
            .field("version", &self.version)
            .field("resource", &self.resource)
            .field("issuer", &self.issuer)
            .field("registration", &self.registration)
            .field("endpoints", &self.endpoints)
            .field("token", &self.token)
            .finish()
    }
}

const fn mcp_oauth_state_version() -> u32 {
    1
}

/// How an MCP OAuth client registration was obtained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOAuthRegistrationMethod {
    /// Registration supplied by configuration.
    #[default]
    Preregistered,
    /// Client ID Metadata Document registration.
    ClientMetadata,
    /// Dynamic Client Registration registration.
    Dynamic,
    /// Registration mechanism introduced by a newer credential version.
    #[serde(other)]
    Unknown,
}

/// Durable MCP OAuth client registration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthRegistration {
    /// Registration mechanism.
    #[serde(default)]
    pub method: McpOAuthRegistrationMethod,
    /// OAuth client identifier.
    #[serde(default)]
    pub client_id: String,
    /// Exact redirect URI used by the client.
    #[serde(default)]
    pub redirect_uri: String,
    /// Token endpoint authentication method.
    #[serde(default)]
    pub token_endpoint_auth_method: String,
    /// Registration creation timestamp in Unix milliseconds.
    #[serde(default)]
    pub created_at_ms: Option<i64>,
    /// Registration expiration timestamp in Unix milliseconds.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

/// Durable discovered endpoints for MCP OAuth.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthEndpoints {
    /// Authorization endpoint.
    #[serde(default)]
    pub authorization: String,
    /// Token endpoint.
    #[serde(default)]
    pub token: String,
    /// Optional token revocation endpoint.
    #[serde(default)]
    pub revocation: Option<String>,
}

/// Durable MCP OAuth token family state.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthToken {
    /// Current access token.
    #[serde(default)]
    pub access_token: String,
    /// Current refresh token, when issued.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// OAuth token type.
    #[serde(default)]
    pub token_type: String,
    /// Scopes granted to this token.
    #[serde(default)]
    pub granted_scopes: Vec<String>,
    /// Actual access-token expiration timestamp in Unix milliseconds.
    ///
    /// Zero means expired or unspecified. A negative value means the
    /// authorization server omitted expiry information for the current token.
    #[serde(default)]
    pub expires_at_ms: i64,
    /// Monotonic token-family generation used for refresh compare-and-swap.
    #[serde(default)]
    pub refresh_generation: u64,
}

impl std::fmt::Debug for McpOAuthToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthToken")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("token_type", &self.token_type)
            .field("granted_scopes", &self.granted_scopes)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("refresh_generation", &self.refresh_generation)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpOAuthTokenExpiry {
    Unknown,
    ExpiresAt(i64),
}

impl McpOAuthToken {
    const fn expiry(&self) -> McpOAuthTokenExpiry {
        match self.expires_at_ms {
            ..0 => McpOAuthTokenExpiry::Unknown,
            expires_at_ms => McpOAuthTokenExpiry::ExpiresAt(expires_at_ms),
        }
    }

    pub(crate) const fn set_expiry_unknown(&mut self) {
        self.expires_at_ms = -1;
    }

    pub(crate) const fn expires_by(&self, deadline_ms: i64) -> bool {
        matches!(
            self.expiry(),
            McpOAuthTokenExpiry::ExpiresAt(expires_at_ms) if expires_at_ms <= deadline_ms
        )
    }
}

/// Identity and deadline for one refresh-generation lease.
#[derive(Clone, PartialEq, Eq)]
pub struct McpRefreshClaim {
    /// Monotonic generation being refreshed.
    pub generation: u64,
    /// Opaque identity required for compare-and-swap completion.
    pub claim_id: String,
    /// Lease deadline in Unix milliseconds.
    pub lease_expires_at_ms: i64,
}

impl std::fmt::Debug for McpRefreshClaim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRefreshClaim")
            .field("generation", &self.generation)
            .field("claim_id", &"<redacted>")
            .field("lease_expires_at_ms", &self.lease_expires_at_ms)
            .finish()
    }
}

/// File-backed credential store.
/// Result of trying to claim one MCP refresh generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRefreshClaimResult {
    /// This caller owns the bounded refresh lease.
    Claimed(McpRefreshClaim),
    /// Another caller committed a newer generation.
    Current(Box<McpOAuthState>),
    /// The token family may have been consumed and must not be retried.
    ReauthorizationRequired,
}

/// Boxed asynchronous result used by host-owned MCP OAuth stores.
pub type McpOAuthStoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Durable target-scoped MCP OAuth storage supplied by a host.
pub trait McpOAuthCredentialStore: Send + Sync + 'static {
    /// Read current state.
    fn read(&self, target_key: String) -> McpOAuthStoreFuture<'_, Option<McpOAuthState>>;
    /// Replace current state.
    fn write(&self, target_key: String, state: McpOAuthState) -> McpOAuthStoreFuture<'_, ()>;
    /// Delete current state unconditionally.
    fn delete(&self, target_key: String) -> McpOAuthStoreFuture<'_, ()>;
    /// Claim an exact refresh generation until `lease_expires_at_ms`.
    fn claim_refresh(
        &self,
        target_key: String,
        claim: McpRefreshClaim,
    ) -> McpOAuthStoreFuture<'_, McpRefreshClaimResult>;
    /// Commit refreshed state if this claim and generation still own the target.
    fn commit_refresh(
        &self,
        target_key: String,
        generation: u64,
        claim_id: String,
        state: McpOAuthState,
    ) -> McpOAuthStoreFuture<'_, bool>;
    /// Release a claim, optionally marking an indeterminate token family unsafe.
    /// Implementations must retain the credential material when marking it unsafe.
    fn abort_refresh(
        &self,
        target_key: String,
        generation: u64,
        claim_id: String,
        indeterminate: bool,
    ) -> McpOAuthStoreFuture<'_, ()>;
}

/// Credential store with an optional host-owned durable MCP backend.
#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
    refresh_lock: Arc<Mutex<()>>,
    refresh_claims: Arc<Mutex<HashMap<String, McpRefreshClaim>>>,
    mcp_store: Option<Arc<dyn McpOAuthCredentialStore>>,
}

impl std::fmt::Debug for FileCredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .field("host_mcp_store", &self.mcp_store.is_some())
            .finish_non_exhaustive()
    }
}

impl FileCredentialStore {
    /// Create a store at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Arc::new(Mutex::new(())),
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_claims: Arc::new(Mutex::new(HashMap::new())),
            mcp_store: None,
        }
    }

    /// Route MCP OAuth state through a host-owned durable backend.
    #[must_use]
    pub fn with_mcp_store(mut self, store: Arc<dyn McpOAuthCredentialStore>) -> Self {
        self.mcp_store = Some(store);
        self
    }

    /// Create a store from `WALTR_AUTH_PATH` or the user data default.
    #[must_use]
    pub fn from_env() -> Self {
        if let Some(path) = std::env::var_os("WALTR_AUTH_PATH") {
            return Self::new(path);
        }
        let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
        Self::new(PathBuf::from(home).join(".local/share/waltr/auth.json"))
    }

    /// Return the configured path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Read a credential by namespaced target key.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured backend cannot read the credential.
    pub async fn read(&self, target_key: &str) -> Result<Option<StoredCredential>, std::io::Error> {
        if target_key.starts_with("mcp:")
            && let Some(store) = &self.mcp_store
        {
            return store
                .read(target_key.to_owned())
                .await
                .map(|state| state.map(StoredCredential::McpOAuth))
                .map_err(std::io::Error::other);
        }
        let _guard = self.lock.lock().await;
        let credentials = self.read_unlocked().await?;
        Ok(credentials.get(target_key).cloned())
    }

    /// Serialize a read-modify-write for a credential target.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured backend cannot read or write the credential.
    pub async fn modify<F>(
        &self,
        target_key: &str,
        f: F,
    ) -> Result<Option<StoredCredential>, std::io::Error>
    where
        F: FnOnce(Option<StoredCredential>) -> Option<StoredCredential>,
    {
        if target_key.starts_with("mcp:")
            && let Some(store) = &self.mcp_store
        {
            let current = store
                .read(target_key.to_owned())
                .await
                .map_err(std::io::Error::other)?
                .map(StoredCredential::McpOAuth);
            let next = f(current);
            match &next {
                Some(StoredCredential::McpOAuth(state)) => store
                    .write(target_key.to_owned(), state.clone())
                    .await
                    .map_err(std::io::Error::other)?,
                Some(StoredCredential::OAuth(_)) => {
                    return Err(std::io::Error::other(
                        "MCP credential backend requires typed MCP OAuth state",
                    ));
                }
                None => store
                    .delete(target_key.to_owned())
                    .await
                    .map_err(std::io::Error::other)?,
            }
            return Ok(next);
        }
        let _guard = self.lock.lock().await;
        let mut credentials = self.read_unlocked().await?;
        let next = f(credentials.get(target_key).cloned());
        match &next {
            Some(credential) => {
                credentials.insert(target_key.to_owned(), credential.clone());
            }
            None => {
                credentials.remove(target_key);
            }
        }
        self.write_unlocked(&credentials).await?;
        Ok(next)
    }

    /// Delete a credential by namespaced target key.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured backend cannot delete the credential.
    pub async fn delete(&self, target_key: &str) -> Result<(), std::io::Error> {
        self.modify(target_key, |_| None).await.map(|_| ())
    }

    pub(crate) async fn claim_mcp_refresh(
        &self,
        target_key: &str,
        claim: McpRefreshClaim,
        now_ms: i64,
    ) -> Result<McpRefreshClaimResult, std::io::Error> {
        if let Some(store) = &self.mcp_store {
            return store
                .claim_refresh(target_key.to_owned(), claim)
                .await
                .map_err(std::io::Error::other);
        }
        let _guard = self.refresh_lock.lock().await;
        let current = self.read(target_key).await?;
        let Some(StoredCredential::McpOAuth(current)) = current else {
            return Ok(McpRefreshClaimResult::ReauthorizationRequired);
        };
        if current.token.refresh_generation != claim.generation {
            return Ok(McpRefreshClaimResult::Current(Box::new(current)));
        }
        let mut claims = self.refresh_claims.lock().await;
        if claims
            .get(target_key)
            .is_some_and(|current| current.lease_expires_at_ms == i64::MAX)
        {
            return Ok(McpRefreshClaimResult::ReauthorizationRequired);
        }
        let active_claim = claims
            .get(target_key)
            .is_some_and(|current| current.lease_expires_at_ms > now_ms);
        if active_claim {
            drop(claims);
            for _ in 0..1_200 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                let current = self.read(target_key).await?;
                if let Some(StoredCredential::McpOAuth(current)) = current
                    && current.token.refresh_generation != claim.generation
                {
                    return Ok(McpRefreshClaimResult::Current(Box::new(current)));
                }
                let claims = self.refresh_claims.lock().await;
                if claims
                    .get(target_key)
                    .is_none_or(|current| current.lease_expires_at_ms <= credential_now_ms())
                {
                    drop(claims);
                    return Ok(McpRefreshClaimResult::ReauthorizationRequired);
                }
                drop(claims);
            }
            return Ok(McpRefreshClaimResult::ReauthorizationRequired);
        }
        if claims.contains_key(target_key) {
            drop(claims);
            return Ok(McpRefreshClaimResult::ReauthorizationRequired);
        }
        claims.insert(target_key.to_owned(), claim.clone());
        drop(claims);
        Ok(McpRefreshClaimResult::Claimed(claim))
    }

    pub(crate) async fn commit_mcp_refresh(
        &self,
        target_key: &str,
        generation: u64,
        claim_id: &str,
        state: McpOAuthState,
    ) -> Result<bool, std::io::Error> {
        if let Some(store) = &self.mcp_store {
            return store
                .commit_refresh(
                    target_key.to_owned(),
                    generation,
                    claim_id.to_owned(),
                    state,
                )
                .await
                .map_err(std::io::Error::other);
        }
        let claim_matches = self
            .refresh_claims
            .lock()
            .await
            .get(target_key)
            .is_some_and(|claim| claim.generation == generation && claim.claim_id == claim_id);
        if !claim_matches {
            return Ok(false);
        }
        let committed = std::sync::atomic::AtomicBool::new(false);
        self.modify(target_key, |stored| {
            let matches = matches!(
                &stored,
                Some(StoredCredential::McpOAuth(current))
                    if current.token.refresh_generation == generation
            );
            committed.store(matches, std::sync::atomic::Ordering::Relaxed);
            matches
                .then_some(StoredCredential::McpOAuth(state))
                .or(stored)
        })
        .await?;
        self.refresh_claims.lock().await.remove(target_key);
        Ok(committed.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub(crate) async fn abort_mcp_refresh(
        &self,
        target_key: &str,
        generation: u64,
        claim_id: &str,
        indeterminate: bool,
    ) -> Result<(), std::io::Error> {
        if let Some(store) = &self.mcp_store {
            return store
                .abort_refresh(
                    target_key.to_owned(),
                    generation,
                    claim_id.to_owned(),
                    indeterminate,
                )
                .await
                .map_err(std::io::Error::other);
        }
        let mut claims = self.refresh_claims.lock().await;
        let claim_matches = claims
            .get(target_key)
            .is_some_and(|claim| claim.generation == generation && claim.claim_id == claim_id);
        if claim_matches {
            if indeterminate {
                // Retain the claim as an in-process replay barrier without deleting
                // credentials that may still contain a usable access token.
                if let Some(claim) = claims.get_mut(target_key) {
                    claim.lease_expires_at_ms = i64::MAX;
                }
            } else {
                claims.remove(target_key);
            }
        }
        drop(claims);
        Ok(())
    }

    async fn read_unlocked(&self) -> Result<BTreeMap<String, StoredCredential>, std::io::Error> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error),
        }
    }

    async fn write_unlocked(
        &self,
        credentials: &BTreeMap<String, StoredCredential>,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
        }
        let bytes = serde_json::to_vec_pretty(credentials).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, bytes).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(tmp, &self.path).await
    }
}

fn credential_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn legacy_oauth_credential_still_deserializes() {
        let credential: StoredCredential = serde_json::from_value(json!({
            "type": "o_auth",
            "access": "access",
            "refresh": "refresh",
            "expires_at_ms": 123,
            "metadata": {"token_url": "https://issuer.example/token"}
        }))
        .expect("legacy credential");

        assert!(matches!(credential, StoredCredential::OAuth(_)));
    }

    #[test]
    fn mcp_oauth_state_tolerates_missing_and_unknown_fields() {
        let credential: StoredCredential = serde_json::from_value(json!({
            "type": "mcp_oauth",
            "resource": "https://mcp.example",
            "future_field": true,
            "registration": {"client_id": "client", "future_field": 1},
            "token": {"access_token": "access", "token_type": "Bearer"}
        }))
        .expect("version-tolerant credential");

        let StoredCredential::McpOAuth(state) = credential else {
            panic!("expected MCP OAuth state");
        };
        assert_eq!(state.version, 1);
        assert_eq!(state.registration.client_id, "client");
        assert_eq!(state.token.refresh_generation, 0);
        assert_eq!(state.token.expiry(), McpOAuthTokenExpiry::ExpiresAt(0));
    }

    #[test]
    fn mcp_oauth_state_tolerates_future_registration_method() {
        let credential: StoredCredential = serde_json::from_value(json!({
            "type": "mcp_oauth",
            "registration": {"method": "future_registration"}
        }))
        .expect("future registration method");

        let StoredCredential::McpOAuth(state) = credential else {
            panic!("expected MCP OAuth state");
        };
        assert_eq!(
            state.registration.method,
            McpOAuthRegistrationMethod::Unknown
        );
    }

    #[test]
    fn oauth_debug_redacts_token_and_claim_secrets() {
        let provider = OAuthCredential {
            access: "provider-access-secret".to_owned(),
            refresh: "provider-refresh-secret".to_owned(),
            expires_at_ms: 10,
            metadata: BTreeMap::from([("future_secret".to_owned(), "metadata-secret".to_owned())]),
        };
        let provider_debug = format!("{provider:?}");
        assert!(!provider_debug.contains("provider-access-secret"));
        assert!(!provider_debug.contains("provider-refresh-secret"));
        assert!(!provider_debug.contains("metadata-secret"));

        let mut state = McpOAuthState {
            version: 1,
            resource: String::new(),
            issuer: String::new(),
            registration: McpOAuthRegistration::default(),
            endpoints: McpOAuthEndpoints::default(),
            token: McpOAuthToken::default(),
        };
        state.token.access_token = "access-secret".to_owned();
        state.token.refresh_token = Some("refresh-secret".to_owned());
        let state_debug = format!("{state:?}");
        assert!(!state_debug.contains("access-secret"));
        assert!(!state_debug.contains("refresh-secret"));

        let claim = McpRefreshClaim {
            generation: 1,
            claim_id: "claim-secret".to_owned(),
            lease_expires_at_ms: 10,
        };
        assert!(!format!("{claim:?}").contains("claim-secret"));
    }

    fn state(generation: u64) -> McpOAuthState {
        McpOAuthState {
            version: 1,
            resource: "https://mcp.example".to_owned(),
            issuer: "https://issuer.example".to_owned(),
            registration: McpOAuthRegistration::default(),
            endpoints: McpOAuthEndpoints::default(),
            token: McpOAuthToken {
                refresh_generation: generation,
                ..McpOAuthToken::default()
            },
        }
    }

    #[tokio::test]
    async fn stale_refresh_claim_cannot_replace_newer_generation() {
        let directory = tempfile::tempdir().expect("temp directory");
        let store = FileCredentialStore::new(directory.path().join("auth.json"));
        let key = "mcp:remote";
        store
            .modify(key, |_| Some(StoredCredential::McpOAuth(state(7))))
            .await
            .expect("store state");
        let claim = McpRefreshClaim {
            generation: 7,
            claim_id: "claim".to_owned(),
            lease_expires_at_ms: credential_now_ms() + 5_000,
        };
        assert!(matches!(
            store
                .claim_mcp_refresh(key, claim.clone(), credential_now_ms())
                .await
                .expect("claim refresh"),
            McpRefreshClaimResult::Claimed(_)
        ));
        store
            .modify(key, |_| Some(StoredCredential::McpOAuth(state(8))))
            .await
            .expect("advance state");

        assert!(
            !store
                .commit_mcp_refresh(key, 7, &claim.claim_id, state(8))
                .await
                .expect("reject stale commit")
        );
        let Some(StoredCredential::McpOAuth(current)) =
            store.read(key).await.expect("read current state")
        else {
            panic!("expected MCP OAuth state");
        };
        assert_eq!(current.token.refresh_generation, 8);
    }
}
