use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, OnceLock, Weak},
};

use oauth2::{AccessToken, RefreshToken, Scope};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Current durable outbound OAuth credential representation.
pub const OAUTH_CREDENTIAL_VERSION: u32 = 1;

/// One provider or MCP OAuth credential.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// Stored representation version.
    pub version: u32,
    /// Shared token material.
    pub token: OAuthTokenSet,
    /// Target-specific non-secret context.
    pub context: OAuthCredentialContext,
}

/// Durable credential stored outside shared config.
pub type StoredCredential = OAuthCredential;

impl std::fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field("version", &self.version)
            .field("token", &self.token)
            .field("context", &self.context)
            .finish()
    }
}

impl OAuthCredential {
    /// Create a provider credential.
    #[must_use]
    pub const fn provider(token: OAuthTokenSet, metadata: BTreeMap<String, String>) -> Self {
        Self {
            version: OAUTH_CREDENTIAL_VERSION,
            token,
            context: OAuthCredentialContext::Provider { metadata },
        }
    }

    /// Create an MCP credential.
    #[must_use]
    pub fn mcp(token: OAuthTokenSet, context: McpOAuthContext) -> Self {
        Self {
            version: OAUTH_CREDENTIAL_VERSION,
            token,
            context: OAuthCredentialContext::Mcp(Box::new(context)),
        }
    }

    /// Provider metadata when this is a provider credential.
    #[must_use]
    pub const fn provider_metadata(&self) -> Option<&BTreeMap<String, String>> {
        match &self.context {
            OAuthCredentialContext::Provider { metadata } => Some(metadata),
            OAuthCredentialContext::Mcp(_) => None,
        }
    }

    /// Mutable provider metadata when this is a provider credential.
    #[must_use]
    pub const fn provider_metadata_mut(&mut self) -> Option<&mut BTreeMap<String, String>> {
        match &mut self.context {
            OAuthCredentialContext::Provider { metadata } => Some(metadata),
            OAuthCredentialContext::Mcp(_) => None,
        }
    }

    /// MCP context when this is an MCP credential.
    #[must_use]
    pub fn mcp_context(&self) -> Option<&McpOAuthContext> {
        match &self.context {
            OAuthCredentialContext::Provider { .. } => None,
            OAuthCredentialContext::Mcp(context) => Some(context.as_ref()),
        }
    }

    /// Mutable MCP context when this is an MCP credential.
    #[must_use]
    pub fn mcp_context_mut(&mut self) -> Option<&mut McpOAuthContext> {
        match &mut self.context {
            OAuthCredentialContext::Provider { .. } => None,
            OAuthCredentialContext::Mcp(context) => Some(context.as_mut()),
        }
    }
}

/// Shared OAuth token set for provider and MCP credentials.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthTokenSet {
    /// Current access token.
    pub access_token: AccessToken,
    /// Current refresh token, when issued.
    pub refresh_token: Option<RefreshToken>,
    /// OAuth token type, normally `Bearer`.
    pub token_type: String,
    /// Scopes granted to this token.
    #[serde(default)]
    pub scopes: Vec<Scope>,
    /// Absolute expiration timestamp in Unix milliseconds, when supplied.
    pub expires_at_ms: Option<i64>,
}

impl std::fmt::Debug for OAuthTokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthTokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("token_type", &self.token_type)
            .field("scopes", &self.scopes)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl PartialEq for OAuthTokenSet {
    fn eq(&self, other: &Self) -> bool {
        self.access_token.secret() == other.access_token.secret()
            && self.refresh_token.as_ref().map(RefreshToken::secret)
                == other.refresh_token.as_ref().map(RefreshToken::secret)
            && self.token_type == other.token_type
            && self.scopes == other.scopes
            && self.expires_at_ms == other.expires_at_ms
    }
}

impl Eq for OAuthTokenSet {}

impl PartialEq for OAuthCredential {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version && self.token == other.token && self.context == other.context
    }
}

impl Eq for OAuthCredential {}

impl OAuthTokenSet {
    /// Construct a bearer token set.
    #[must_use]
    pub fn bearer(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        expires_at_ms: Option<i64>,
    ) -> Self {
        Self {
            access_token: AccessToken::new(access_token.into()),
            refresh_token: refresh_token.map(RefreshToken::new),
            token_type: "Bearer".to_owned(),
            scopes: Vec::new(),
            expires_at_ms,
        }
    }

    pub(crate) fn expires_by(&self, deadline_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms <= deadline_ms)
    }
}

/// Typed non-secret context for an outbound OAuth credential.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OAuthCredentialContext {
    /// Provider-specific extracted metadata.
    Provider {
        /// Non-secret provider-specific metadata.
        #[serde(default)]
        metadata: BTreeMap<String, String>,
    },
    /// MCP resource, registration, and endpoints.
    Mcp(Box<McpOAuthContext>),
}

impl std::fmt::Debug for OAuthCredentialContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider { metadata } => formatter
                .debug_struct("Provider")
                .field("metadata_keys", &metadata.keys().collect::<Vec<_>>())
                .finish(),
            Self::Mcp(context) => formatter.debug_tuple("Mcp").field(context).finish(),
        }
    }
}

/// Durable MCP-specific OAuth context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthContext {
    /// Exact protected resource identifier.
    pub resource: String,
    /// Selected authorization server issuer.
    pub issuer: String,
    /// Client registration used for this resource and issuer.
    pub registration: McpOAuthRegistration,
    /// Discovered authorization server endpoints.
    pub endpoints: McpOAuthEndpoints,
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
    pub method: McpOAuthRegistrationMethod,
    /// OAuth client identifier.
    pub client_id: String,
    /// Exact redirect URI used by the client.
    pub redirect_uri: String,
    /// Token endpoint authentication method.
    pub token_endpoint_auth_method: String,
    /// Registration creation timestamp in Unix milliseconds.
    pub created_at_ms: Option<i64>,
    /// Registration expiration timestamp in Unix milliseconds.
    pub expires_at_ms: Option<i64>,
}

/// Durable discovered endpoints for MCP OAuth.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthEndpoints {
    /// Authorization endpoint.
    pub authorization: String,
    /// Token endpoint.
    pub token: String,
    /// Optional token revocation endpoint.
    pub revocation: Option<String>,
}

/// Credential plus SQL/file storage envelope fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredentialRecord {
    /// Typed credential payload.
    pub credential: OAuthCredential,
    /// Monotonic storage revision used for refresh compare-and-swap.
    pub revision: u64,
    /// Whether a possibly consumed token family requires authorization again.
    #[serde(default)]
    pub reauthorization_required: bool,
}

/// Identity and deadline for one refresh-revision lease.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthRefreshClaim {
    /// Monotonic revision being refreshed.
    pub revision: u64,
    /// Opaque identity required for compare-and-swap completion.
    pub claim_id: String,
    /// Lease deadline in Unix milliseconds.
    pub lease_expires_at_ms: i64,
}

impl std::fmt::Debug for OAuthRefreshClaim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthRefreshClaim")
            .field("revision", &self.revision)
            .field("claim_id", &"<redacted>")
            .field("lease_expires_at_ms", &self.lease_expires_at_ms)
            .finish()
    }
}

/// Result of trying to claim one credential refresh revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthRefreshClaimResult {
    /// This caller owns the bounded refresh lease.
    Claimed(OAuthRefreshClaim),
    /// Another caller committed a newer revision.
    Current(Box<OAuthCredentialRecord>),
    /// The token family may have been consumed and must not be retried.
    ReauthorizationRequired,
}

/// Boxed asynchronous result used by host-owned OAuth stores.
pub type OAuthCredentialStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Durable target-scoped outbound OAuth storage supplied by a host.
pub trait OAuthCredentialStore: Send + Sync + 'static {
    /// Read the current credential and storage envelope.
    fn read(
        &self,
        target_key: String,
    ) -> OAuthCredentialStoreFuture<'_, Option<OAuthCredentialRecord>>;
    /// Install or replace a credential, clearing refresh failure state.
    fn write(
        &self,
        target_key: String,
        credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, ()>;
    /// Delete a credential unconditionally.
    fn delete(&self, target_key: String) -> OAuthCredentialStoreFuture<'_, ()>;
    /// Claim an exact storage revision until the supplied deadline.
    fn claim_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
    ) -> OAuthCredentialStoreFuture<'_, OAuthRefreshClaimResult>;
    /// Commit a refreshed credential if the claim still owns the revision.
    fn commit_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, bool>;
    /// Release a claim, optionally marking the token family unsafe.
    fn abort_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        indeterminate: bool,
    ) -> OAuthCredentialStoreFuture<'_, ()>;
}

/// Cloneable handle to a host-owned outbound credential store.
#[derive(Clone)]
pub struct CredentialStore(Arc<dyn OAuthCredentialStore>);

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .finish_non_exhaustive()
    }
}

impl CredentialStore {
    /// Wrap a concrete credential store.
    #[must_use]
    pub fn new(store: impl OAuthCredentialStore) -> Self {
        Self(Arc::new(store))
    }

    /// Wrap a shared concrete credential store.
    #[must_use]
    pub fn shared<T>(store: Arc<T>) -> Self
    where
        T: OAuthCredentialStore,
    {
        Self(store)
    }

    /// Read one target record.
    ///
    /// # Errors
    ///
    /// Returns a sanitized storage error when the host store cannot be read.
    pub async fn read(&self, target_key: &str) -> Result<Option<OAuthCredentialRecord>, String> {
        self.0.read(target_key.to_owned()).await
    }

    /// Install one target credential.
    ///
    /// # Errors
    ///
    /// Returns a sanitized storage error when the host store cannot persist the credential.
    pub async fn write(&self, target_key: &str, credential: OAuthCredential) -> Result<(), String> {
        self.0.write(target_key.to_owned(), credential).await
    }

    /// Delete one target credential.
    ///
    /// # Errors
    ///
    /// Returns a sanitized storage error when the host store cannot delete the credential.
    pub async fn delete(&self, target_key: &str) -> Result<(), String> {
        self.0.delete(target_key.to_owned()).await
    }

    pub(crate) async fn claim_refresh(
        &self,
        target_key: &str,
        claim: OAuthRefreshClaim,
    ) -> Result<OAuthRefreshClaimResult, String> {
        self.0.claim_refresh(target_key.to_owned(), claim).await
    }

    pub(crate) async fn commit_refresh(
        &self,
        target_key: &str,
        claim: OAuthRefreshClaim,
        credential: OAuthCredential,
    ) -> Result<bool, String> {
        self.0
            .commit_refresh(target_key.to_owned(), claim, credential)
            .await
    }

    pub(crate) async fn abort_refresh(
        &self,
        target_key: &str,
        claim: OAuthRefreshClaim,
        indeterminate: bool,
    ) -> Result<(), String> {
        self.0
            .abort_refresh(target_key.to_owned(), claim, indeterminate)
            .await
    }
}

struct FileCredentialSynchronization {
    lock: Arc<Mutex<()>>,
    refresh_lock: Arc<Mutex<()>>,
    refresh_claims: Arc<Mutex<HashMap<String, OAuthRefreshClaim>>>,
}

impl FileCredentialSynchronization {
    fn new() -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            refresh_lock: Arc::new(Mutex::new(())),
            refresh_claims: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// File-backed credential store for standalone agent embeddings.
#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
    _synchronization: Arc<FileCredentialSynchronization>,
    lock: Arc<Mutex<()>>,
    refresh_lock: Arc<Mutex<()>>,
    refresh_claims: Arc<Mutex<HashMap<String, OAuthRefreshClaim>>>,
}

impl std::fmt::Debug for FileCredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FileCredentialStore {
    /// Create a store at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = effective_credential_path(path.into());
        let synchronization = credential_synchronization(&path);
        Self {
            path,
            _synchronization: Arc::clone(&synchronization),
            lock: Arc::clone(&synchronization.lock),
            refresh_lock: Arc::clone(&synchronization.refresh_lock),
            refresh_claims: Arc::clone(&synchronization.refresh_claims),
        }
    }

    /// Create a standalone store from `WALTR_AUTH_PATH` or the user data default.
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
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn read_unlocked(
        &self,
    ) -> Result<BTreeMap<String, OAuthCredentialRecord>, std::io::Error> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error),
        }
    }

    async fn write_unlocked(
        &self,
        credentials: &BTreeMap<String, OAuthCredentialRecord>,
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

impl From<FileCredentialStore> for CredentialStore {
    fn from(store: FileCredentialStore) -> Self {
        Self::new(store)
    }
}

impl OAuthCredentialStore for FileCredentialStore {
    fn read(
        &self,
        target_key: String,
    ) -> OAuthCredentialStoreFuture<'_, Option<OAuthCredentialRecord>> {
        Box::pin(async move {
            let _guard = self.lock.lock().await;
            self.read_unlocked()
                .await
                .map(|credentials| credentials.get(&target_key).cloned())
                .map_err(|_| "credential file read failed".to_owned())
        })
    }

    fn write(
        &self,
        target_key: String,
        credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, ()> {
        Box::pin(async move {
            let _refresh_guard = self.refresh_lock.lock().await;
            let _guard = self.lock.lock().await;
            let mut credentials = self
                .read_unlocked()
                .await
                .map_err(|_| "credential file read failed".to_owned())?;
            let revision = credentials
                .get(&target_key)
                .map_or(0, |record| record.revision.saturating_add(1));
            credentials.insert(
                target_key.clone(),
                OAuthCredentialRecord {
                    credential,
                    revision,
                    reauthorization_required: false,
                },
            );
            self.write_unlocked(&credentials)
                .await
                .map_err(|_| "credential file write failed".to_owned())?;
            self.refresh_claims.lock().await.remove(&target_key);
            Ok(())
        })
    }

    fn delete(&self, target_key: String) -> OAuthCredentialStoreFuture<'_, ()> {
        Box::pin(async move {
            let _refresh_guard = self.refresh_lock.lock().await;
            let _guard = self.lock.lock().await;
            let mut credentials = self
                .read_unlocked()
                .await
                .map_err(|_| "credential file read failed".to_owned())?;
            credentials.remove(&target_key);
            self.write_unlocked(&credentials)
                .await
                .map_err(|_| "credential file write failed".to_owned())?;
            self.refresh_claims.lock().await.remove(&target_key);
            Ok(())
        })
    }

    fn claim_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
    ) -> OAuthCredentialStoreFuture<'_, OAuthRefreshClaimResult> {
        Box::pin(async move {
            for _ in 0..310 {
                let refresh_guard = self.refresh_lock.lock().await;
                let Some(current) = OAuthCredentialStore::read(self, target_key.clone()).await?
                else {
                    return Ok(OAuthRefreshClaimResult::ReauthorizationRequired);
                };
                if current.reauthorization_required {
                    return Ok(OAuthRefreshClaimResult::ReauthorizationRequired);
                }
                if current.revision != claim.revision {
                    return Ok(OAuthRefreshClaimResult::Current(Box::new(current)));
                }
                let mut claims = self.refresh_claims.lock().await;
                let existing = claims.get(&target_key).cloned();
                if existing.is_none() {
                    claims.insert(target_key.clone(), claim.clone());
                    return Ok(OAuthRefreshClaimResult::Claimed(claim));
                }
                drop(claims);
                drop(refresh_guard);

                if let Some(existing) = existing
                    && existing.lease_expires_at_ms <= unix_time_ms()
                {
                    let _refresh_guard = self.refresh_lock.lock().await;
                    let mut claims = self.refresh_claims.lock().await;
                    if claims.get(&target_key) != Some(&existing) {
                        continue;
                    }
                    claims.remove(&target_key);
                    drop(claims);
                    let _guard = self.lock.lock().await;
                    let mut credentials = self
                        .read_unlocked()
                        .await
                        .map_err(|_| "credential file read failed".to_owned())?;
                    if let Some(current) = credentials.get_mut(&target_key)
                        && current.revision == claim.revision
                    {
                        current.reauthorization_required = true;
                        self.write_unlocked(&credentials)
                            .await
                            .map_err(|_| "credential file write failed".to_owned())?;
                    }
                    return Ok(OAuthRefreshClaimResult::ReauthorizationRequired);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok(OAuthRefreshClaimResult::ReauthorizationRequired)
        })
    }

    fn commit_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, bool> {
        Box::pin(async move {
            let _refresh_guard = self.refresh_lock.lock().await;
            let claim_matches = self
                .refresh_claims
                .lock()
                .await
                .get(&target_key)
                .is_some_and(|current| current == &claim);
            if !claim_matches {
                return Ok(false);
            }
            let _guard = self.lock.lock().await;
            let mut credentials = self
                .read_unlocked()
                .await
                .map_err(|_| "credential file read failed".to_owned())?;
            let Some(current) = credentials.get(&target_key) else {
                return Ok(false);
            };
            if current.revision != claim.revision || current.reauthorization_required {
                return Ok(false);
            }
            credentials.insert(
                target_key.clone(),
                OAuthCredentialRecord {
                    credential,
                    revision: claim.revision.saturating_add(1),
                    reauthorization_required: false,
                },
            );
            self.write_unlocked(&credentials)
                .await
                .map_err(|_| "credential file write failed".to_owned())?;
            self.refresh_claims.lock().await.remove(&target_key);
            Ok(true)
        })
    }

    fn abort_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        indeterminate: bool,
    ) -> OAuthCredentialStoreFuture<'_, ()> {
        Box::pin(async move {
            let _refresh_guard = self.refresh_lock.lock().await;
            let claim_matches = self
                .refresh_claims
                .lock()
                .await
                .get(&target_key)
                .is_some_and(|current| current == &claim);
            if !claim_matches {
                return Ok(());
            }
            if indeterminate {
                let _guard = self.lock.lock().await;
                let mut credentials = self
                    .read_unlocked()
                    .await
                    .map_err(|_| "credential file read failed".to_owned())?;
                if let Some(current) = credentials.get_mut(&target_key)
                    && current.revision == claim.revision
                {
                    current.reauthorization_required = true;
                    self.write_unlocked(&credentials)
                        .await
                        .map_err(|_| "credential file write failed".to_owned())?;
                }
            }
            self.refresh_claims.lock().await.remove(&target_key);
            Ok(())
        })
    }
}

fn credential_synchronization(path: &Path) -> Arc<FileCredentialSynchronization> {
    static SYNCHRONIZATION: OnceLock<
        StdMutex<HashMap<PathBuf, Weak<FileCredentialSynchronization>>>,
    > = OnceLock::new();
    let registry = SYNCHRONIZATION.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(synchronization) = registry.get(path).and_then(Weak::upgrade) {
        return synchronization;
    }
    registry.retain(|_, synchronization| synchronization.strong_count() > 0);
    let synchronization = Arc::new(FileCredentialSynchronization::new());
    registry.insert(path.to_path_buf(), Arc::downgrade(&synchronization));
    synchronization
}

fn effective_credential_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_or_else(|_| path.clone(), |current| current.join(&path))
    };
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return canonical;
    }

    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return absolute;
        };
        missing.push(name.to_owned());
        let Some(parent) = ancestor.parent() else {
            return absolute;
        };
        ancestor = parent;
    }
    let mut effective = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for component in missing.iter().rev() {
        effective.push(component);
    }
    effective
}

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(access: &str) -> OAuthCredential {
        OAuthCredential::provider(
            OAuthTokenSet::bearer(access, Some("refresh-secret".to_owned()), Some(10)),
            BTreeMap::from([("account".to_owned(), "secret-metadata".to_owned())]),
        )
    }

    #[test]
    fn unified_credential_round_trips_and_redacts_secrets() {
        let credential = provider("access-secret");
        let encoded = serde_json::to_string(&credential).unwrap();
        let decoded: OAuthCredential = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, credential);
        let debug = format!("{credential:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(!debug.contains("secret-metadata"));
    }

    #[tokio::test]
    async fn file_store_uses_revision_claim_and_cas_for_every_target() {
        for target in ["provider:test", "mcp:test"] {
            let directory = tempfile::tempdir().unwrap();
            let store = FileCredentialStore::new(directory.path().join("auth.json"));
            OAuthCredentialStore::write(&store, target.to_owned(), provider("old"))
                .await
                .unwrap();
            let current = OAuthCredentialStore::read(&store, target.to_owned())
                .await
                .unwrap()
                .unwrap();
            let claim = OAuthRefreshClaim {
                revision: current.revision,
                claim_id: "claim-secret".to_owned(),
                lease_expires_at_ms: i64::MAX,
            };
            assert!(matches!(
                OAuthCredentialStore::claim_refresh(&store, target.to_owned(), claim.clone())
                    .await
                    .unwrap(),
                OAuthRefreshClaimResult::Claimed(_)
            ));
            assert!(
                OAuthCredentialStore::commit_refresh(
                    &store,
                    target.to_owned(),
                    claim,
                    provider("new")
                )
                .await
                .unwrap()
            );
            let current = OAuthCredentialStore::read(&store, target.to_owned())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.revision, 1);
            assert_eq!(current.credential.token.access_token.secret(), "new");
        }
    }

    #[tokio::test]
    async fn indeterminate_file_refresh_retains_credential_and_requires_reauthorization() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(directory.path().join("auth.json"));
        OAuthCredentialStore::write(&store, "provider:test".to_owned(), provider("old"))
            .await
            .unwrap();
        let claim = OAuthRefreshClaim {
            revision: 0,
            claim_id: "claim".to_owned(),
            lease_expires_at_ms: i64::MAX,
        };
        OAuthCredentialStore::claim_refresh(&store, "provider:test".to_owned(), claim.clone())
            .await
            .unwrap();
        OAuthCredentialStore::abort_refresh(&store, "provider:test".to_owned(), claim, true)
            .await
            .unwrap();
        let current = OAuthCredentialStore::read(&store, "provider:test".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert!(current.reauthorization_required);
        assert_eq!(current.credential.token.access_token.secret(), "old");
    }

    #[tokio::test]
    async fn file_replacement_and_reauthorization_invalidate_stale_refresh_claims() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(directory.path().join("auth.json"));
        let target = "provider:test".to_owned();
        OAuthCredentialStore::write(&store, target.clone(), provider("old"))
            .await
            .unwrap();
        let replacement_claim = OAuthRefreshClaim {
            revision: 0,
            claim_id: "replacement-claim".to_owned(),
            lease_expires_at_ms: i64::MAX,
        };
        OAuthCredentialStore::claim_refresh(&store, target.clone(), replacement_claim.clone())
            .await
            .unwrap();
        OAuthCredentialStore::write(&store, target.clone(), provider("replacement"))
            .await
            .unwrap();
        assert!(
            !OAuthCredentialStore::commit_refresh(
                &store,
                target.clone(),
                replacement_claim,
                provider("stale")
            )
            .await
            .unwrap()
        );

        let reauthorization_claim = OAuthRefreshClaim {
            revision: 1,
            claim_id: "reauthorization-claim".to_owned(),
            lease_expires_at_ms: i64::MAX,
        };
        OAuthCredentialStore::claim_refresh(&store, target.clone(), reauthorization_claim.clone())
            .await
            .unwrap();
        OAuthCredentialStore::delete(&store, target.clone())
            .await
            .unwrap();
        OAuthCredentialStore::write(&store, target.clone(), provider("reauthorized"))
            .await
            .unwrap();
        assert!(
            !OAuthCredentialStore::commit_refresh(
                &store,
                target.clone(),
                reauthorization_claim,
                provider("stale")
            )
            .await
            .unwrap()
        );
        let current = OAuthCredentialStore::read(&store, target)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            current.credential.token.access_token.secret(),
            "reauthorized"
        );
    }
}
