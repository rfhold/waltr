use std::fmt;

use sqlx::{PgPool, Row as _, postgres::PgPoolOptions};

use super::{
    BoxFuture, McpPrincipalId, OidcResourceOwnerAttempt, OidcResourceOwnerStore, OidcStoreError,
    OidcStoreResult, valid_digest, validate,
};

/// `PostgreSQL` one-shot OIDC store using the MCP migrator and caller-owned pool.
#[derive(Clone)]
pub struct PostgresOidcResourceOwnerStore {
    pool: PgPool,
}
impl fmt::Debug for PostgresOidcResourceOwnerStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresOidcResourceOwnerStore")
            .finish_non_exhaustive()
    }
}
impl PostgresOidcResourceOwnerStore {
    /// Connect and run the complete MCP `PostgreSQL` migration history.
    ///
    /// # Errors
    ///
    /// Returns an error when connection or migration fails.
    pub async fn connect(database_url: &str) -> OidcStoreResult<Self> {
        let pool = PgPoolOptions::new()
            .connect(database_url)
            .await
            .map_err(unavailable)?;
        Self::from_pool(pool).await
    }
    /// Run the complete MCP migrator on and retain the supplied pool.
    ///
    /// # Errors
    ///
    /// Returns an error when migration fails.
    pub async fn from_pool(pool: PgPool) -> OidcStoreResult<Self> {
        crate::oauth::server::migrate_mcp_postgres(&pool)
            .await
            .map_err(|_| OidcStoreError::Unavailable)?;
        Ok(Self { pool })
    }
    /// Borrow the shared MCP pool.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }
}
impl OidcResourceOwnerStore for PostgresOidcResourceOwnerStore {
    fn create(&self, v: OidcResourceOwnerAttempt) -> BoxFuture<OidcStoreResult<bool>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            validate(&v)?;
            let result=sqlx::query("INSERT INTO mcp.oauth_oidc_resource_owner_attempts (transaction_id,state_digest,browser_correlation_digest,nonce,pkce_verifier,created_at_ms,expires_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING").bind(v.transaction_id).bind(v.state_digest).bind(v.browser_correlation_digest).bind(v.nonce).bind(v.pkce_verifier).bind(v.created_at_ms).bind(v.expires_at_ms).execute(&pool).await.map_err(unavailable)?;
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
            let row=sqlx::query("SELECT * FROM mcp.oauth_oidc_resource_owner_attempts WHERE state_digest=$1 AND created_at_ms<=$2 AND expires_at_ms>$2").bind(digest).bind(now).fetch_optional(&pool).await.map_err(unavailable)?;
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
            let row=sqlx::query("UPDATE mcp.oauth_oidc_resource_owner_attempts SET callback_claimed_at_ms=$1 WHERE transaction_id=$2 AND created_at_ms<=$1 AND expires_at_ms>$1 AND callback_claimed_at_ms IS NULL AND completed_at_ms IS NULL RETURNING *").bind(now).bind(id).fetch_optional(&pool).await.map_err(unavailable)?;
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
            let result=sqlx::query("UPDATE mcp.oauth_oidc_resource_owner_attempts SET principal_id=$1,completed_at_ms=$2 WHERE transaction_id=$3 AND created_at_ms<=$2 AND expires_at_ms>$2 AND callback_claimed_at_ms IS NOT NULL AND completed_at_ms IS NULL").bind(principal.as_str()).bind(now).bind(id).execute(&pool).await.map_err(unavailable)?;
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
            let row=sqlx::query("SELECT principal_id FROM mcp.oauth_oidc_resource_owner_attempts WHERE transaction_id=$1 AND browser_correlation_digest=$2 AND created_at_ms<=$3 AND expires_at_ms>$3 AND completed_at_ms IS NOT NULL AND consumed_at_ms IS NULL").bind(id).bind(digest).bind(now).fetch_optional(&pool).await.map_err(unavailable)?;
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
            let result=sqlx::query("UPDATE mcp.oauth_oidc_resource_owner_attempts SET consumed_at_ms=$1 WHERE transaction_id=$2 AND completed_at_ms IS NOT NULL AND consumed_at_ms IS NULL").bind(now).bind(id).execute(&pool).await.map_err(unavailable)?;
            Ok(result.rows_affected() == 1)
        })
    }
    fn cleanup(&self, now: i64, limit: usize) -> BoxFuture<OidcStoreResult<usize>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let limit = i64::try_from(limit).map_err(|_| OidcStoreError::InvalidData)?;
            let result=sqlx::query("DELETE FROM mcp.oauth_oidc_resource_owner_attempts WHERE transaction_id IN (SELECT transaction_id FROM mcp.oauth_oidc_resource_owner_attempts WHERE expires_at_ms<=$1 ORDER BY expires_at_ms,transaction_id LIMIT $2)").bind(now).bind(limit).execute(&pool).await.map_err(unavailable)?;
            usize::try_from(result.rows_affected()).map_err(|_| OidcStoreError::InvalidData)
        })
    }
}
fn decode(row: &sqlx::postgres::PgRow) -> OidcStoreResult<OidcResourceOwnerAttempt> {
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
