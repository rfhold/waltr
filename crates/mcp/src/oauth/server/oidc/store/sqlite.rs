use std::{fmt, str::FromStr as _};

use sqlx::{
    Row as _, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use super::{
    BoxFuture, McpPrincipalId, OidcResourceOwnerAttempt, OidcResourceOwnerStore, OidcStoreError,
    OidcStoreResult, valid_digest, validate,
};

/// `SQLite` one-shot OIDC store using the MCP migrator and caller-owned pool.
#[derive(Clone)]
pub struct SqliteOidcResourceOwnerStore {
    pool: SqlitePool,
}

impl fmt::Debug for SqliteOidcResourceOwnerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteOidcResourceOwnerStore")
            .finish_non_exhaustive()
    }
}

impl SqliteOidcResourceOwnerStore {
    /// Connect and run the complete MCP `SQLite` migration history.
    ///
    /// # Errors
    ///
    /// Returns an error when connection or migration fails.
    pub async fn connect(database_url: &str) -> OidcStoreResult<Self> {
        let memory = database_url.contains(":memory:") || database_url.contains("mode=memory");
        let options = SqliteConnectOptions::from_str(database_url)
            .map_err(unavailable)?
            .create_if_missing(true)
            .foreign_keys(true);
        let mut builder = SqlitePoolOptions::new();
        if memory {
            builder = builder
                .max_connections(1)
                .min_connections(1)
                .max_lifetime(None)
                .idle_timeout(None);
        }
        let pool = builder.connect_with(options).await.map_err(unavailable)?;
        Self::from_pool(pool).await
    }
    /// Run the complete MCP migrator on and retain the supplied pool.
    ///
    /// # Errors
    ///
    /// Returns an error when migration fails.
    pub async fn from_pool(pool: SqlitePool) -> OidcStoreResult<Self> {
        crate::oauth::server::migrate_mcp_sqlite(&pool)
            .await
            .map_err(|_| OidcStoreError::Unavailable)?;
        Ok(Self { pool })
    }
    /// Borrow the shared MCP pool.
    #[must_use]
    pub const fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

impl OidcResourceOwnerStore for SqliteOidcResourceOwnerStore {
    fn create(&self, v: OidcResourceOwnerAttempt) -> BoxFuture<OidcStoreResult<bool>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            validate(&v)?;
            let result=sqlx::query("INSERT OR IGNORE INTO mcp_oauth_oidc_resource_owner_attempts (transaction_id,state_digest,browser_correlation_digest,nonce,pkce_verifier,created_at_ms,expires_at_ms) VALUES (?,?,?,?,?,?,?)").bind(v.transaction_id).bind(v.state_digest).bind(v.browser_correlation_digest).bind(v.nonce).bind(v.pkce_verifier).bind(v.created_at_ms).bind(v.expires_at_ms).execute(&pool).await.map_err(unavailable)?;
            Ok(result.rows_affected() == 1)
        })
    }
    fn by_state(
        &self,
        digest: Vec<u8>,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<Option<OidcResourceOwnerAttempt>>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            valid_digest(&digest)?;
            let row=sqlx::query("SELECT * FROM mcp_oauth_oidc_resource_owner_attempts WHERE state_digest=? AND created_at_ms<=? AND expires_at_ms>?").bind(digest).bind(now).bind(now).fetch_optional(&pool).await.map_err(unavailable)?;
            row.map(|r| decode(&r)).transpose()
        })
    }
    fn claim_callback(
        &self,
        id: String,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<Option<OidcResourceOwnerAttempt>>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let row=sqlx::query("UPDATE mcp_oauth_oidc_resource_owner_attempts SET callback_claimed_at_ms=? WHERE transaction_id=? AND created_at_ms<=? AND expires_at_ms>? AND callback_claimed_at_ms IS NULL AND completed_at_ms IS NULL RETURNING *").bind(now).bind(id).bind(now).bind(now).fetch_optional(&pool).await.map_err(unavailable)?;
            row.map(|r| decode(&r)).transpose()
        })
    }
    fn complete(
        &self,
        id: String,
        principal: McpPrincipalId,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<bool>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let result=sqlx::query("UPDATE mcp_oauth_oidc_resource_owner_attempts SET principal_id=?,completed_at_ms=? WHERE transaction_id=? AND created_at_ms<=? AND expires_at_ms>? AND callback_claimed_at_ms IS NOT NULL AND completed_at_ms IS NULL").bind(principal.as_str()).bind(now).bind(id).bind(now).bind(now).execute(&pool).await.map_err(unavailable)?;
            Ok(result.rows_affected() == 1)
        })
    }
    fn completed(
        &self,
        id: String,
        digest: Vec<u8>,
        now: i64,
    ) -> BoxFuture<OidcStoreResult<Option<McpPrincipalId>>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            valid_digest(&digest)?;
            let row=sqlx::query("SELECT principal_id FROM mcp_oauth_oidc_resource_owner_attempts WHERE transaction_id=? AND browser_correlation_digest=? AND created_at_ms<=? AND expires_at_ms>? AND completed_at_ms IS NOT NULL AND consumed_at_ms IS NULL").bind(id).bind(digest).bind(now).bind(now).fetch_optional(&pool).await.map_err(unavailable)?;
            row.map(|r| {
                r.try_get::<String, _>("principal_id")
                    .map_err(unavailable)
                    .and_then(|v| McpPrincipalId::new(v).map_err(|_| OidcStoreError::InvalidData))
            })
            .transpose()
        })
    }
    fn finalize_completed(&self, id: String, now: i64) -> BoxFuture<OidcStoreResult<bool>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let result=sqlx::query("UPDATE mcp_oauth_oidc_resource_owner_attempts SET consumed_at_ms=? WHERE transaction_id=? AND completed_at_ms IS NOT NULL AND consumed_at_ms IS NULL").bind(now).bind(id).execute(&pool).await.map_err(unavailable)?;
            Ok(result.rows_affected() == 1)
        })
    }
    fn cleanup(&self, now: i64, limit: usize) -> BoxFuture<OidcStoreResult<usize>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let limit = i64::try_from(limit).map_err(|_| OidcStoreError::InvalidData)?;
            let result=sqlx::query("DELETE FROM mcp_oauth_oidc_resource_owner_attempts WHERE transaction_id IN (SELECT transaction_id FROM mcp_oauth_oidc_resource_owner_attempts WHERE expires_at_ms<=? ORDER BY expires_at_ms,transaction_id LIMIT ?)").bind(now).bind(limit).execute(&pool).await.map_err(unavailable)?;
            usize::try_from(result.rows_affected()).map_err(|_| OidcStoreError::InvalidData)
        })
    }
}

fn decode(row: &sqlx::sqlite::SqliteRow) -> OidcStoreResult<OidcResourceOwnerAttempt> {
    let principal: Option<String> = row.try_get("principal_id").map_err(unavailable)?;
    Ok(OidcResourceOwnerAttempt {
        transaction_id: row.try_get("transaction_id").map_err(unavailable)?,
        state_digest: row.try_get("state_digest").map_err(unavailable)?,
        browser_correlation_digest: row
            .try_get("browser_correlation_digest")
            .map_err(unavailable)?,
        nonce: row.try_get("nonce").map_err(unavailable)?,
        pkce_verifier: row.try_get("pkce_verifier").map_err(unavailable)?,
        created_at_ms: row.try_get("created_at_ms").map_err(unavailable)?,
        expires_at_ms: row.try_get("expires_at_ms").map_err(unavailable)?,
        callback_claimed_at_ms: row.try_get("callback_claimed_at_ms").map_err(unavailable)?,
        completed_at_ms: row.try_get("completed_at_ms").map_err(unavailable)?,
        principal_id: principal
            .map(McpPrincipalId::new)
            .transpose()
            .map_err(|_| OidcStoreError::InvalidData)?,
        consumed_at_ms: row.try_get("consumed_at_ms").map_err(unavailable)?,
    })
}
fn unavailable<E>(_: E) -> OidcStoreError {
    OidcStoreError::Unavailable
}
