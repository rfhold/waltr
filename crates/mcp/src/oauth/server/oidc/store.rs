use std::{collections::BTreeMap, fmt, sync::Arc};

use tokio::sync::Mutex;

use crate::{McpPrincipalId, server::BoxFuture};

#[cfg(feature = "oauth-resource-owner-oidc-postgres")]
mod postgres;
#[cfg(feature = "oauth-resource-owner-oidc-sqlite")]
mod sqlite;
#[cfg(feature = "oauth-resource-owner-oidc-postgres")]
pub use postgres::PostgresOidcResourceOwnerStore;
#[cfg(feature = "oauth-resource-owner-oidc-sqlite")]
pub use sqlite::SqliteOidcResourceOwnerStore;

/// Persisted one-shot OIDC attempt. State and browser correlation are digest-only.
#[derive(Clone, PartialEq, Eq)]
pub struct OidcResourceOwnerAttempt {
    /// Parent hosted OAuth transaction identifier.
    pub transaction_id: String,
    /// SHA-256 digest of upstream OIDC state.
    pub state_digest: Vec<u8>,
    /// SHA-256 digest of plaintext browser correlation.
    pub browser_correlation_digest: Vec<u8>,
    /// Upstream ID-token nonce.
    pub nonce: String,
    /// Upstream PKCE verifier.
    pub pkce_verifier: String,
    /// Creation time as Unix milliseconds.
    pub created_at_ms: i64,
    /// Absolute expiry as Unix milliseconds.
    pub expires_at_ms: i64,
    /// Atomic callback claim time.
    pub callback_claimed_at_ms: Option<i64>,
    /// Verified mapping completion time.
    pub completed_at_ms: Option<i64>,
    /// Mapped principal, present only after completion.
    pub principal_id: Option<McpPrincipalId>,
    /// Atomic hosted-resume consumption time.
    pub consumed_at_ms: Option<i64>,
}

impl fmt::Debug for OidcResourceOwnerAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcResourceOwnerAttempt")
            .field("transaction_id", &self.transaction_id)
            .field("state_digest", &"[redacted]")
            .field("browser_correlation_digest", &"[redacted]")
            .field("nonce", &"[redacted]")
            .field("pkce_verifier", &"[redacted]")
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("callback_claimed_at_ms", &self.callback_claimed_at_ms)
            .field("completed_at_ms", &self.completed_at_ms)
            .field("principal_id", &self.principal_id)
            .field("consumed_at_ms", &self.consumed_at_ms)
            .finish()
    }
}

/// Sanitized OIDC persistence failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OidcStoreError {
    /// Persistence could not complete an operation.
    Unavailable,
    /// Supplied or persisted state violates the contract.
    InvalidData,
}
impl fmt::Display for OidcStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "OIDC store unavailable",
            Self::InvalidData => "OIDC store data invalid",
        })
    }
}
impl std::error::Error for OidcStoreError {}
/// Result returned by one-shot OIDC stores.
pub type OidcStoreResult<T> = std::result::Result<T, OidcStoreError>;

/// Object-safe atomic persistence contract for one-shot resource-owner OIDC.
pub trait OidcResourceOwnerStore: Send + Sync + 'static {
    /// Create one digest-bound attempt.
    fn create(&self, attempt: OidcResourceOwnerAttempt) -> BoxFuture<OidcStoreResult<bool>>;
    /// Load one live attempt by state digest.
    fn by_state(
        &self,
        state_digest: Vec<u8>,
        now_ms: i64,
    ) -> BoxFuture<OidcStoreResult<Option<OidcResourceOwnerAttempt>>>;
    /// Atomically claim a live callback once across replicas.
    fn claim_callback(
        &self,
        transaction_id: String,
        now_ms: i64,
    ) -> BoxFuture<OidcStoreResult<Option<OidcResourceOwnerAttempt>>>;
    /// Persist a mapped principal after a successful callback claim.
    fn complete(
        &self,
        transaction_id: String,
        principal_id: McpPrincipalId,
        now_ms: i64,
    ) -> BoxFuture<OidcStoreResult<bool>>;
    /// Read one live completion with exact correlation binding without consuming it.
    fn completed(
        &self,
        transaction_id: String,
        correlation_digest: Vec<u8>,
        now_ms: i64,
    ) -> BoxFuture<OidcStoreResult<Option<McpPrincipalId>>>;
    /// Best-effort mark a completion consumed after hosted authentication commits.
    fn finalize_completed(
        &self,
        transaction_id: String,
        now_ms: i64,
    ) -> BoxFuture<OidcStoreResult<bool>>;
    /// Delete at most `limit` expired attempts.
    fn cleanup(&self, now_ms: i64, limit: usize) -> BoxFuture<OidcStoreResult<usize>>;
}

/// Process-local behavioral reference and test store.
#[derive(Clone, Debug, Default)]
pub struct InMemoryOidcResourceOwnerStore {
    attempts: Arc<Mutex<BTreeMap<String, OidcResourceOwnerAttempt>>>,
}

impl OidcResourceOwnerStore for InMemoryOidcResourceOwnerStore {
    fn create(&self, attempt: OidcResourceOwnerAttempt) -> BoxFuture<OidcStoreResult<bool>> {
        let this = self.clone();
        Box::pin(async move {
            validate(&attempt)?;
            let mut values = this.attempts.lock().await;
            let created = if values.contains_key(&attempt.transaction_id)
                || values
                    .values()
                    .any(|v| v.state_digest == attempt.state_digest)
            {
                false
            } else {
                values.insert(attempt.transaction_id.clone(), attempt);
                true
            };
            drop(values);
            Ok(created)
        })
    }
    fn by_state(
        &self,
        digest: Vec<u8>,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<Option<OidcResourceOwnerAttempt>>> {
        let this = self.clone();
        Box::pin(async move {
            valid_digest(&digest)?;
            Ok(this
                .attempts
                .lock()
                .await
                .values()
                .find(|v| v.state_digest == digest && live(v, now))
                .cloned())
        })
    }
    fn claim_callback(
        &self,
        id: String,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<Option<OidcResourceOwnerAttempt>>> {
        let this = self.clone();
        Box::pin(async move {
            let mut values = this.attempts.lock().await;
            let claimed = values
                .get_mut(&id)
                .filter(|v| {
                    live(v, now)
                        && v.callback_claimed_at_ms.is_none()
                        && v.completed_at_ms.is_none()
                })
                .map(|v| {
                    v.callback_claimed_at_ms = Some(now);
                    v.clone()
                });
            drop(values);
            Ok(claimed)
        })
    }
    fn complete(
        &self,
        id: String,
        principal: McpPrincipalId,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<bool>> {
        let this = self.clone();
        Box::pin(async move {
            let mut values = this.attempts.lock().await;
            let completed = values
                .get_mut(&id)
                .filter(|v| {
                    live(v, now)
                        && v.callback_claimed_at_ms.is_some()
                        && v.completed_at_ms.is_none()
                })
                .is_some_and(|v| {
                    v.principal_id = Some(principal);
                    v.completed_at_ms = Some(now);
                    true
                });
            drop(values);
            Ok(completed)
        })
    }
    fn completed(
        &self,
        id: String,
        digest: Vec<u8>,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<Option<McpPrincipalId>>> {
        let this = self.clone();
        Box::pin(async move {
            valid_digest(&digest)?;
            let values = this.attempts.lock().await;
            let principal = values
                .get(&id)
                .filter(|v| {
                    live(v, now)
                        && v.completed_at_ms.is_some()
                        && v.consumed_at_ms.is_none()
                        && constant_time(&v.browser_correlation_digest, &digest)
                })
                .and_then(|v| v.principal_id.clone());
            drop(values);
            Ok(principal)
        })
    }
    fn finalize_completed(&self, id: String, now: i64) -> BoxFuture<OidcStoreResult<bool>> {
        let this = self.clone();
        Box::pin(async move {
            let mut values = this.attempts.lock().await;
            let finalized = values
                .get_mut(&id)
                .filter(|v| v.completed_at_ms.is_some() && v.consumed_at_ms.is_none())
                .is_some_and(|v| {
                    v.consumed_at_ms = Some(now);
                    true
                });
            drop(values);
            Ok(finalized)
        })
    }
    fn cleanup(&self, now: i64, limit: usize) -> BoxFuture<OidcStoreResult<usize>> {
        let this = self.clone();
        Box::pin(async move {
            let mut values = this.attempts.lock().await;
            let ids = values
                .iter()
                .filter(|(_, v)| v.expires_at_ms <= now)
                .take(limit)
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>();
            for id in &ids {
                values.remove(id);
            }
            let deleted = ids.len();
            drop(values);
            Ok(deleted)
        })
    }
}

pub(super) fn validate(v: &OidcResourceOwnerAttempt) -> OidcStoreResult<()> {
    if v.transaction_id.is_empty()
        || v.nonce.is_empty()
        || v.pkce_verifier.len() != 43
        || v.created_at_ms < 0
        || v.expires_at_ms <= v.created_at_ms
        || v.callback_claimed_at_ms.is_some()
        || v.completed_at_ms.is_some()
        || v.principal_id.is_some()
        || v.consumed_at_ms.is_some()
    {
        return Err(OidcStoreError::InvalidData);
    }
    valid_digest(&v.state_digest)?;
    valid_digest(&v.browser_correlation_digest)
}
pub(super) const fn valid_digest(v: &[u8]) -> OidcStoreResult<()> {
    if v.len() == 32 {
        Ok(())
    } else {
        Err(OidcStoreError::InvalidData)
    }
}
const fn live(v: &OidcResourceOwnerAttempt, now: i64) -> bool {
    v.created_at_ms <= now && v.expires_at_ms > now
}
pub(super) fn constant_time(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}
