use waltr_agent::{
    McpOAuthCredentialStore, McpOAuthState, McpOAuthStoreFuture, McpRefreshClaim,
    McpRefreshClaimResult,
};
use sha2::{Digest as _, Sha256};
use sqlx::{postgres::PgRow, sqlite::SqliteRow};

use super::PersistencePool;

const ATTEMPT_RETENTION_MS: i64 = 60 * 60 * 1000;
const MAX_ACTIVE_ATTEMPTS: i64 = 1_024;
const MAX_ACTIVE_ATTEMPTS_PER_TARGET: i64 = 8;

#[derive(Clone)]
pub struct OutboundOAuthStore {
    pool: PersistencePool,
}

impl std::fmt::Debug for OutboundOAuthStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboundOAuthStore")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct DurableAttemptRecord {
    pub attempt_id: String,
    pub target_key: String,
    pub attempt_json: String,
    pub registration_method: String,
    pub completion_mode: String,
    pub status: String,
    pub failure_reason: Option<String>,
    pub expires_at_ms: i64,
}

impl std::fmt::Debug for DurableAttemptRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableAttemptRecord")
            .field("attempt_id", &"<redacted>")
            .field("target_key", &self.target_key)
            .field("attempt_json", &"<redacted>")
            .field("registration_method", &self.registration_method)
            .field("completion_mode", &self.completion_mode)
            .field("status", &self.status)
            .field("failure_reason", &self.failure_reason)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

impl OutboundOAuthStore {
    pub(super) const fn new(pool: PersistencePool) -> Self {
        Self { pool }
    }

    pub(crate) async fn save_attempt(
        &self,
        attempt: &DurableAttemptRecord,
        state: &str,
    ) -> Result<(), String> {
        let state_digest = state_digest(state);
        let now = now_ms();
        let retention_cutoff = now.saturating_sub(ATTEMPT_RETENTION_MS);
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(sanitized_store_error)?;
                sqlx::query("DELETE FROM outbound_oauth_attempts WHERE expires_at_ms < ? OR (status IN ('succeeded', 'failed', 'canceled', 'expired') AND updated_at_ms < ?)")
                    .bind(now).bind(retention_cutoff).execute(&mut *transaction).await.map_err(sanitized_store_error)?;
                let active = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM outbound_oauth_attempts WHERE status IN ('pending', 'completing')")
                    .fetch_one(&mut *transaction).await.map_err(sanitized_store_error)?;
                let target_active = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM outbound_oauth_attempts WHERE target_key = ? AND status IN ('pending', 'completing')")
                    .bind(&attempt.target_key).fetch_one(&mut *transaction).await.map_err(sanitized_store_error)?;
                if active >= MAX_ACTIVE_ATTEMPTS || target_active >= MAX_ACTIVE_ATTEMPTS_PER_TARGET
                {
                    return Err("too many active outbound OAuth attempts".to_owned());
                }
                sqlx::query("INSERT INTO outbound_oauth_attempts (attempt_id, state_digest, target_key, attempt_json, registration_method, completion_mode, status, expires_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(&attempt.attempt_id).bind(&state_digest).bind(&attempt.target_key)
                    .bind(&attempt.attempt_json).bind(&attempt.registration_method)
                    .bind(&attempt.completion_mode).bind(&attempt.status)
                    .bind(attempt.expires_at_ms).bind(now).execute(&mut *transaction).await
                    .map_err(sanitized_store_error)?;
                transaction.commit().await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                let mut transaction = pool.begin().await.map_err(sanitized_store_error)?;
                sqlx::query("SELECT pg_advisory_xact_lock(1263817807)")
                    .execute(&mut *transaction)
                    .await
                    .map_err(sanitized_store_error)?;
                sqlx::query("DELETE FROM waltr.outbound_oauth_attempts WHERE expires_at_ms < $1 OR (status IN ('succeeded', 'failed', 'canceled', 'expired') AND updated_at_ms < $2)")
                    .bind(now).bind(retention_cutoff).execute(&mut *transaction).await.map_err(sanitized_store_error)?;
                let active = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM waltr.outbound_oauth_attempts WHERE status IN ('pending', 'completing')")
                    .fetch_one(&mut *transaction).await.map_err(sanitized_store_error)?;
                let target_active = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM waltr.outbound_oauth_attempts WHERE target_key = $1 AND status IN ('pending', 'completing')")
                    .bind(&attempt.target_key).fetch_one(&mut *transaction).await.map_err(sanitized_store_error)?;
                if active >= MAX_ACTIVE_ATTEMPTS || target_active >= MAX_ACTIVE_ATTEMPTS_PER_TARGET
                {
                    return Err("too many active outbound OAuth attempts".to_owned());
                }
                sqlx::query("INSERT INTO waltr.outbound_oauth_attempts (attempt_id, state_digest, target_key, attempt_json, registration_method, completion_mode, status, expires_at_ms, updated_at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
                    .bind(&attempt.attempt_id).bind(&state_digest).bind(&attempt.target_key)
                    .bind(&attempt.attempt_json).bind(&attempt.registration_method)
                    .bind(&attempt.completion_mode).bind(&attempt.status)
                    .bind(attempt.expires_at_ms).bind(now).execute(&mut *transaction).await
                    .map_err(sanitized_store_error)?;
                transaction.commit().await.map_err(sanitized_store_error)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn attempt(
        &self,
        attempt_id: &str,
    ) -> Result<Option<DurableAttemptRecord>, String> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("SELECT attempt_id, target_key, attempt_json, registration_method, completion_mode, status, failure_reason, expires_at_ms FROM outbound_oauth_attempts WHERE attempt_id = ?")
                .bind(attempt_id).fetch_optional(pool).await.map_err(sanitized_store_error)?.as_ref().map(sqlite_row_to_attempt).transpose(),
            PersistencePool::PostgreSql(pool) => sqlx::query("SELECT attempt_id, target_key, attempt_json, registration_method, completion_mode, status, failure_reason, expires_at_ms FROM waltr.outbound_oauth_attempts WHERE attempt_id = $1")
                .bind(attempt_id).fetch_optional(pool).await.map_err(sanitized_store_error)?.as_ref().map(pg_row_to_attempt).transpose(),
        }
    }

    pub(crate) async fn attempt_by_state(
        &self,
        state: &str,
    ) -> Result<Option<DurableAttemptRecord>, String> {
        let state_digest = state_digest(state);
        match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("SELECT attempt_id, target_key, attempt_json, registration_method, completion_mode, status, failure_reason, expires_at_ms FROM outbound_oauth_attempts WHERE state_digest = ?")
                .bind(&state_digest).fetch_optional(pool).await.map_err(sanitized_store_error)?.as_ref().map(sqlite_row_to_attempt).transpose(),
            PersistencePool::PostgreSql(pool) => sqlx::query("SELECT attempt_id, target_key, attempt_json, registration_method, completion_mode, status, failure_reason, expires_at_ms FROM waltr.outbound_oauth_attempts WHERE state_digest = $1")
                .bind(&state_digest).fetch_optional(pool).await.map_err(sanitized_store_error)?.as_ref().map(pg_row_to_attempt).transpose(),
        }
    }

    pub(crate) async fn reauthorization_required(&self, target_key: &str) -> Result<bool, String> {
        self.read_state_row(target_key)
            .await
            .map(|row| row.is_some_and(|(_, _, required)| required))
    }

    pub(crate) async fn claim_attempt(
        &self,
        attempt_id: &str,
        claim_id: &str,
        lease_expires_at_ms: i64,
    ) -> Result<bool, String> {
        let now = now_ms();
        let affected = match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_attempts SET status = 'completing', completion_claim_id = ?, completion_claim_expires_at_ms = ?, updated_at_ms = ? WHERE attempt_id = ? AND expires_at_ms >= ? AND (status = 'pending' OR (status = 'completing' AND completion_claim_expires_at_ms <= ?))")
                .bind(claim_id).bind(lease_expires_at_ms).bind(now).bind(attempt_id).bind(now).bind(now).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_attempts SET status = 'completing', completion_claim_id = $1, completion_claim_expires_at_ms = $2, updated_at_ms = $3 WHERE attempt_id = $4 AND expires_at_ms >= $3 AND (status = 'pending' OR (status = 'completing' AND completion_claim_expires_at_ms <= $3))")
                .bind(claim_id).bind(lease_expires_at_ms).bind(now).bind(attempt_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
        };
        Ok(affected == 1)
    }

    pub(crate) async fn transition_pending_attempt(
        &self,
        attempt_id: &str,
        status: &str,
        failure: Option<&str>,
    ) -> Result<bool, String> {
        let affected = match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_attempts SET status = ?, failure_reason = ?, updated_at_ms = ? WHERE attempt_id = ? AND status = 'pending'")
                .bind(status).bind(failure).bind(now_ms()).bind(attempt_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_attempts SET status = $1, failure_reason = $2, updated_at_ms = $3 WHERE attempt_id = $4 AND status = 'pending'")
                .bind(status).bind(failure).bind(now_ms()).bind(attempt_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
        };
        Ok(affected == 1)
    }

    pub(crate) async fn finish_claimed_attempt(
        &self,
        attempt_id: &str,
        claim_id: &str,
        status: &str,
        failure: Option<&str>,
    ) -> Result<bool, String> {
        let affected = match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_attempts SET status = ?, completion_claim_id = NULL, completion_claim_expires_at_ms = NULL, failure_reason = ?, updated_at_ms = ? WHERE attempt_id = ? AND status = 'completing' AND completion_claim_id = ?")
                .bind(status).bind(failure).bind(now_ms()).bind(attempt_id).bind(claim_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_attempts SET status = $1, completion_claim_id = NULL, completion_claim_expires_at_ms = NULL, failure_reason = $2, updated_at_ms = $3 WHERE attempt_id = $4 AND status = 'completing' AND completion_claim_id = $5")
                .bind(status).bind(failure).bind(now_ms()).bind(attempt_id).bind(claim_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
        };
        Ok(affected == 1)
    }

    pub(crate) async fn complete_attempt_with_credential(
        &self,
        attempt_id: &str,
        claim_id: &str,
        target_key: &str,
        state: &McpOAuthState,
    ) -> Result<(), String> {
        let json = serde_json::to_string(state)
            .map_err(|_| "failed to encode outbound OAuth state".to_owned())?;
        let generation = i64::try_from(state.token.refresh_generation).unwrap_or(i64::MAX);
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(sanitized_store_error)?;
                sqlx::query("INSERT INTO outbound_oauth_credentials (target_key, state_json, refresh_generation, updated_at_ms) VALUES (?, ?, ?, ?) ON CONFLICT(target_key) DO UPDATE SET state_json = excluded.state_json, refresh_generation = excluded.refresh_generation, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = 0, updated_at_ms = excluded.updated_at_ms")
                    .bind(target_key).bind(&json).bind(generation).bind(now_ms()).execute(&mut *transaction).await.map_err(sanitized_store_error)?;
                let affected = sqlx::query("UPDATE outbound_oauth_attempts SET status = 'succeeded', completion_claim_id = NULL, completion_claim_expires_at_ms = NULL, failure_reason = NULL, updated_at_ms = ? WHERE attempt_id = ? AND status = 'completing' AND completion_claim_id = ?")
                    .bind(now_ms()).bind(attempt_id).bind(claim_id).execute(&mut *transaction).await.map_err(sanitized_store_error)?.rows_affected();
                if affected != 1 {
                    return Err("outbound OAuth attempt changed during completion".to_owned());
                }
                transaction.commit().await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                let mut transaction = pool.begin().await.map_err(sanitized_store_error)?;
                sqlx::query("INSERT INTO waltr.outbound_oauth_credentials (target_key, state_json, refresh_generation, updated_at_ms) VALUES ($1, $2, $3, $4) ON CONFLICT(target_key) DO UPDATE SET state_json = EXCLUDED.state_json, refresh_generation = EXCLUDED.refresh_generation, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = FALSE, updated_at_ms = EXCLUDED.updated_at_ms")
                    .bind(target_key).bind(&json).bind(generation).bind(now_ms()).execute(&mut *transaction).await.map_err(sanitized_store_error)?;
                let affected = sqlx::query("UPDATE waltr.outbound_oauth_attempts SET status = 'succeeded', completion_claim_id = NULL, completion_claim_expires_at_ms = NULL, failure_reason = NULL, updated_at_ms = $1 WHERE attempt_id = $2 AND status = 'completing' AND completion_claim_id = $3")
                    .bind(now_ms()).bind(attempt_id).bind(claim_id).execute(&mut *transaction).await.map_err(sanitized_store_error)?.rows_affected();
                if affected != 1 {
                    return Err("outbound OAuth attempt changed during completion".to_owned());
                }
                transaction.commit().await.map_err(sanitized_store_error)?;
            }
        }
        Ok(())
    }

    async fn mark_reauthorization(&self, target_key: &str, generation: u64) -> Result<(), String> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                sqlx::query("UPDATE outbound_oauth_credentials SET reauthorization_required = 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = ? WHERE target_key = ? AND refresh_generation = ?")
                .bind(now_ms()).bind(target_key).bind(i64::try_from(generation).unwrap_or(i64::MAX)).execute(pool).await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                sqlx::query("UPDATE waltr.outbound_oauth_credentials SET reauthorization_required = TRUE, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = $1 WHERE target_key = $2 AND refresh_generation = $3")
                .bind(now_ms()).bind(target_key).bind(i64::try_from(generation).unwrap_or(i64::MAX)).execute(pool).await.map_err(sanitized_store_error)?;
            }
        }
        Ok(())
    }
}

fn sqlite_row_to_attempt(row: &SqliteRow) -> Result<DurableAttemptRecord, String> {
    row_to_attempt_fields(row)
}

fn pg_row_to_attempt(row: &PgRow) -> Result<DurableAttemptRecord, String> {
    row_to_attempt_fields(row)
}

fn row_to_attempt_fields<R>(row: &R) -> Result<DurableAttemptRecord, String>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    Option<String>: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    i64: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    Ok(DurableAttemptRecord {
        attempt_id: row.try_get("attempt_id").map_err(sanitized_store_error)?,
        target_key: row.try_get("target_key").map_err(sanitized_store_error)?,
        attempt_json: row.try_get("attempt_json").map_err(sanitized_store_error)?,
        registration_method: row
            .try_get("registration_method")
            .map_err(sanitized_store_error)?,
        completion_mode: row
            .try_get("completion_mode")
            .map_err(sanitized_store_error)?,
        status: row.try_get("status").map_err(sanitized_store_error)?,
        failure_reason: row
            .try_get("failure_reason")
            .map_err(sanitized_store_error)?,
        expires_at_ms: row
            .try_get("expires_at_ms")
            .map_err(sanitized_store_error)?,
    })
}

impl McpOAuthCredentialStore for OutboundOAuthStore {
    fn read(&self, target_key: String) -> McpOAuthStoreFuture<'_, Option<McpOAuthState>> {
        Box::pin(async move { self.read_state(&target_key).await })
    }

    fn write(&self, target_key: String, state: McpOAuthState) -> McpOAuthStoreFuture<'_, ()> {
        Box::pin(async move { self.write_state(&target_key, &state).await })
    }

    fn delete(&self, target_key: String) -> McpOAuthStoreFuture<'_, ()> {
        Box::pin(async move {
            match &self.pool {
                PersistencePool::Sqlite(pool) => {
                    sqlx::query("DELETE FROM outbound_oauth_credentials WHERE target_key = ?")
                        .bind(target_key)
                        .execute(pool)
                        .await
                        .map_err(sanitized_store_error)?;
                }
                PersistencePool::PostgreSql(pool) => {
                    sqlx::query(
                        "DELETE FROM waltr.outbound_oauth_credentials WHERE target_key = $1",
                    )
                    .bind(target_key)
                    .execute(pool)
                    .await
                    .map_err(sanitized_store_error)?;
                }
            }
            Ok(())
        })
    }

    fn claim_refresh(
        &self,
        target_key: String,
        claim: McpRefreshClaim,
    ) -> McpOAuthStoreFuture<'_, McpRefreshClaimResult> {
        Box::pin(async move {
            let now = now_ms();
            let generation = i64::try_from(claim.generation).unwrap_or(i64::MAX);
            let affected = match &self.pool {
                PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_credentials SET refresh_claim_id = ?, refresh_claim_expires_at_ms = ?, updated_at_ms = ? WHERE target_key = ? AND refresh_generation = ? AND reauthorization_required = 0 AND refresh_claim_id IS NULL")
                    .bind(&claim.claim_id).bind(claim.lease_expires_at_ms).bind(now).bind(&target_key).bind(generation).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
                PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_credentials SET refresh_claim_id = $1, refresh_claim_expires_at_ms = $2, updated_at_ms = $3 WHERE target_key = $4 AND refresh_generation = $5 AND reauthorization_required = FALSE AND refresh_claim_id IS NULL")
                    .bind(&claim.claim_id).bind(claim.lease_expires_at_ms).bind(now).bind(&target_key).bind(generation).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            };
            if affected == 1 {
                return Ok(McpRefreshClaimResult::Claimed(claim));
            }
            for _ in 0..310 {
                let Some((state, claimed_until, reauth)) = self.read_state_row(&target_key).await?
                else {
                    return Ok(McpRefreshClaimResult::ReauthorizationRequired);
                };
                if reauth {
                    return Ok(McpRefreshClaimResult::ReauthorizationRequired);
                }
                if claimed_until.is_some_and(|expiry| expiry <= now_ms()) {
                    self.mark_reauthorization(&target_key, claim.generation)
                        .await?;
                    return Ok(McpRefreshClaimResult::ReauthorizationRequired);
                }
                if state.token.refresh_generation != claim.generation {
                    return Ok(McpRefreshClaimResult::Current(Box::new(state)));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Ok(McpRefreshClaimResult::ReauthorizationRequired)
        })
    }

    fn commit_refresh(
        &self,
        target_key: String,
        generation: u64,
        claim_id: String,
        state: McpOAuthState,
    ) -> McpOAuthStoreFuture<'_, bool> {
        Box::pin(async move {
            let json = serde_json::to_string(&state)
                .map_err(|_| "failed to encode outbound OAuth state".to_owned())?;
            let affected = match &self.pool {
                PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_credentials SET state_json = ?, refresh_generation = ?, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = ? WHERE target_key = ? AND refresh_generation = ? AND refresh_claim_id = ? AND reauthorization_required = 0")
                    .bind(json).bind(i64::try_from(state.token.refresh_generation).unwrap_or(i64::MAX)).bind(now_ms()).bind(target_key).bind(i64::try_from(generation).unwrap_or(i64::MAX)).bind(claim_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
                PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_credentials SET state_json = $1, refresh_generation = $2, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = $3 WHERE target_key = $4 AND refresh_generation = $5 AND refresh_claim_id = $6 AND reauthorization_required = FALSE")
                    .bind(json).bind(i64::try_from(state.token.refresh_generation).unwrap_or(i64::MAX)).bind(now_ms()).bind(target_key).bind(i64::try_from(generation).unwrap_or(i64::MAX)).bind(claim_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            };
            Ok(affected == 1)
        })
    }

    fn abort_refresh(
        &self,
        target_key: String,
        generation: u64,
        claim_id: String,
        indeterminate: bool,
    ) -> McpOAuthStoreFuture<'_, ()> {
        Box::pin(async move {
            match &self.pool {
                PersistencePool::Sqlite(pool) => {
                    sqlx::query("UPDATE outbound_oauth_credentials SET refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = ?, updated_at_ms = ? WHERE target_key = ? AND refresh_generation = ? AND refresh_claim_id = ?")
                    .bind(indeterminate).bind(now_ms()).bind(target_key).bind(i64::try_from(generation).unwrap_or(i64::MAX)).bind(claim_id).execute(pool).await.map_err(sanitized_store_error)?;
                }
                PersistencePool::PostgreSql(pool) => {
                    sqlx::query("UPDATE waltr.outbound_oauth_credentials SET refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = $1, updated_at_ms = $2 WHERE target_key = $3 AND refresh_generation = $4 AND refresh_claim_id = $5")
                    .bind(indeterminate).bind(now_ms()).bind(target_key).bind(i64::try_from(generation).unwrap_or(i64::MAX)).bind(claim_id).execute(pool).await.map_err(sanitized_store_error)?;
                }
            }
            Ok(())
        })
    }
}

impl OutboundOAuthStore {
    async fn read_state(&self, target_key: &str) -> Result<Option<McpOAuthState>, String> {
        self.read_state_row(target_key)
            .await
            .map(|row| row.map(|row| row.0))
    }

    async fn read_state_row(
        &self,
        target_key: &str,
    ) -> Result<Option<(McpOAuthState, Option<i64>, bool)>, String> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("SELECT state_json, refresh_claim_expires_at_ms, reauthorization_required FROM outbound_oauth_credentials WHERE target_key = ?")
                .bind(target_key).fetch_optional(pool).await.map_err(sanitized_store_error)?
                .as_ref().map(parse_state_row).transpose(),
            PersistencePool::PostgreSql(pool) => sqlx::query("SELECT state_json, refresh_claim_expires_at_ms, reauthorization_required FROM waltr.outbound_oauth_credentials WHERE target_key = $1")
                .bind(target_key).fetch_optional(pool).await.map_err(sanitized_store_error)?
                .as_ref().map(parse_state_row).transpose(),
        }
    }

    async fn write_state(&self, target_key: &str, state: &McpOAuthState) -> Result<(), String> {
        let json = serde_json::to_string(state)
            .map_err(|_| "failed to encode outbound OAuth state".to_owned())?;
        let generation = i64::try_from(state.token.refresh_generation).unwrap_or(i64::MAX);
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                sqlx::query("INSERT INTO outbound_oauth_credentials (target_key, state_json, refresh_generation, updated_at_ms) VALUES (?, ?, ?, ?) ON CONFLICT(target_key) DO UPDATE SET state_json = excluded.state_json, refresh_generation = excluded.refresh_generation, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = 0, updated_at_ms = excluded.updated_at_ms")
                .bind(target_key).bind(json).bind(generation).bind(now_ms()).execute(pool).await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                sqlx::query("INSERT INTO waltr.outbound_oauth_credentials (target_key, state_json, refresh_generation, updated_at_ms) VALUES ($1, $2, $3, $4) ON CONFLICT(target_key) DO UPDATE SET state_json = EXCLUDED.state_json, refresh_generation = EXCLUDED.refresh_generation, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = FALSE, updated_at_ms = EXCLUDED.updated_at_ms")
                .bind(target_key).bind(json).bind(generation).bind(now_ms()).execute(pool).await.map_err(sanitized_store_error)?;
            }
        }
        Ok(())
    }
}

fn parse_state_row<R>(row: &R) -> Result<(McpOAuthState, Option<i64>, bool), String>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    bool: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    let json: String = row.try_get("state_json").map_err(sanitized_store_error)?;
    let state = serde_json::from_str(&json)
        .map_err(|_| "stored outbound OAuth state is invalid".to_owned())?;
    let expiry = row
        .try_get("refresh_claim_expires_at_ms")
        .map_err(sanitized_store_error)?;
    let reauth = row
        .try_get("reauthorization_required")
        .map_err(sanitized_store_error)?;
    Ok((state, expiry, reauth))
}

fn sanitized_store_error<E>(_error: E) -> String {
    "outbound OAuth persistence failed".to_owned()
}

fn state_digest(state: &str) -> Vec<u8> {
    Sha256::digest(state.as_bytes()).to_vec()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use waltr_agent::{
        McpOAuthCredentialStore as _, McpOAuthEndpoints, McpOAuthRegistration, McpOAuthToken,
    };

    use super::*;
    use crate::persistence::{DatabaseConfig, Persistence};

    fn state(generation: u64, access: &str) -> McpOAuthState {
        McpOAuthState {
            version: 1,
            resource: "https://mcp.example/resource".to_owned(),
            issuer: "https://auth.example".to_owned(),
            registration: McpOAuthRegistration {
                client_id: "client".to_owned(),
                redirect_uri: "https://waltr.example/outbound-oauth/callback".to_owned(),
                token_endpoint_auth_method: "none".to_owned(),
                ..McpOAuthRegistration::default()
            },
            endpoints: McpOAuthEndpoints {
                token: "https://auth.example/token".to_owned(),
                ..McpOAuthEndpoints::default()
            },
            token: McpOAuthToken {
                access_token: access.to_owned(),
                refresh_token: Some("refresh".to_owned()),
                token_type: "Bearer".to_owned(),
                expires_at_ms: i64::MAX,
                refresh_generation: generation,
                ..McpOAuthToken::default()
            },
        }
    }

    async fn stores() -> (Arc<OutboundOAuthStore>, Arc<OutboundOAuthStore>) {
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        (
            persistence.outbound_oauth_store(),
            persistence.outbound_oauth_store(),
        )
    }

    fn claim(generation: u64, id: &str, lease_expires_at_ms: i64) -> McpRefreshClaim {
        McpRefreshClaim {
            generation,
            claim_id: id.to_owned(),
            lease_expires_at_ms,
        }
    }

    #[tokio::test]
    async fn refresh_claim_is_cross_replica_singleflight_and_generation_cas() {
        let (first, second) = stores().await;
        first
            .write("mcp:remote".to_owned(), state(3, "old"))
            .await
            .unwrap();
        assert!(matches!(
            first
                .claim_refresh("mcp:remote".to_owned(), claim(3, "first", now_ms() + 5_000))
                .await
                .unwrap(),
            McpRefreshClaimResult::Claimed(_)
        ));
        let waiter = tokio::spawn({
            let second = Arc::clone(&second);
            async move {
                second
                    .claim_refresh(
                        "mcp:remote".to_owned(),
                        claim(3, "second", now_ms() + 5_000),
                    )
                    .await
                    .unwrap()
            }
        });
        let mut refreshed = state(4, "new");
        refreshed.token.refresh_token = Some("rotated".to_owned());
        assert!(
            first
                .commit_refresh(
                    "mcp:remote".to_owned(),
                    3,
                    "first".to_owned(),
                    refreshed.clone()
                )
                .await
                .unwrap()
        );
        assert_eq!(
            waiter.await.unwrap(),
            McpRefreshClaimResult::Current(Box::new(refreshed))
        );
        assert!(
            !second
                .commit_refresh(
                    "mcp:remote".to_owned(),
                    3,
                    "second".to_owned(),
                    state(4, "stale")
                )
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn indeterminate_abort_and_expired_lease_require_reauthorization() {
        let (first, second) = stores().await;
        first
            .write("mcp:abort".to_owned(), state(0, "access"))
            .await
            .unwrap();
        first
            .claim_refresh("mcp:abort".to_owned(), claim(0, "claim", now_ms() + 5_000))
            .await
            .unwrap();
        first
            .abort_refresh("mcp:abort".to_owned(), 0, "claim".to_owned(), true)
            .await
            .unwrap();
        assert!(first.reauthorization_required("mcp:abort").await.unwrap());
        assert_eq!(
            second
                .claim_refresh("mcp:abort".to_owned(), claim(0, "next", now_ms() + 5_000))
                .await
                .unwrap(),
            McpRefreshClaimResult::ReauthorizationRequired
        );

        first
            .write("mcp:lease".to_owned(), state(0, "access"))
            .await
            .unwrap();
        first
            .claim_refresh("mcp:lease".to_owned(), claim(0, "claim", now_ms() + 5))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(
            second
                .claim_refresh("mcp:lease".to_owned(), claim(0, "next", now_ms() + 5_000))
                .await
                .unwrap(),
            McpRefreshClaimResult::ReauthorizationRequired
        );
    }

    #[tokio::test]
    async fn callback_attempt_hands_off_between_replicas_and_is_one_shot() {
        let (first, second) = stores().await;
        let attempt = DurableAttemptRecord {
            attempt_id: "attempt".to_owned(),
            target_key: "mcp:remote".to_owned(),
            attempt_json: "{}".to_owned(),
            registration_method: "dynamic".to_owned(),
            completion_mode: "server_callback".to_owned(),
            status: "pending".to_owned(),
            failure_reason: None,
            expires_at_ms: now_ms() + 60_000,
        };
        first.save_attempt(&attempt, "csrf-state").await.unwrap();
        assert_eq!(
            second
                .attempt_by_state("csrf-state")
                .await
                .unwrap()
                .unwrap()
                .attempt_id,
            "attempt"
        );
        assert!(
            second
                .claim_attempt("attempt", "first", now_ms() + 5_000)
                .await
                .unwrap()
        );
        assert!(
            !first
                .claim_attempt("attempt", "second", now_ms() + 5_000)
                .await
                .unwrap()
        );
        second
            .finish_claimed_attempt("attempt", "first", "succeeded", None)
            .await
            .unwrap();
        assert_eq!(
            first.attempt("attempt").await.unwrap().unwrap().status,
            "succeeded"
        );
    }

    #[tokio::test]
    async fn callback_completion_atomically_persists_and_removal_deletes_credential() {
        let (first, second) = stores().await;
        let attempt = DurableAttemptRecord {
            attempt_id: "complete".to_owned(),
            target_key: "mcp:remote".to_owned(),
            attempt_json: "{}".to_owned(),
            registration_method: "dynamic".to_owned(),
            completion_mode: "server_callback".to_owned(),
            status: "pending".to_owned(),
            failure_reason: None,
            expires_at_ms: now_ms() + 60_000,
        };
        first
            .save_attempt(&attempt, "complete-state")
            .await
            .unwrap();
        assert!(
            first
                .claim_attempt("complete", "worker", now_ms() + 5_000)
                .await
                .unwrap()
        );
        first
            .complete_attempt_with_credential(
                "complete",
                "worker",
                "mcp:remote",
                &state(0, "access"),
            )
            .await
            .unwrap();
        assert_eq!(
            second.attempt("complete").await.unwrap().unwrap().status,
            "succeeded"
        );
        assert_eq!(
            second
                .read("mcp:remote".to_owned())
                .await
                .unwrap()
                .unwrap()
                .token
                .access_token,
            "access"
        );
        second.delete("mcp:remote".to_owned()).await.unwrap();
        assert!(
            second
                .read("mcp:remote".to_owned())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn callback_attempt_claim_is_reclaimed_after_crash() {
        let (first, second) = stores().await;
        first
            .save_attempt(
                &DurableAttemptRecord {
                    attempt_id: "crashed".to_owned(),
                    target_key: "mcp:remote".to_owned(),
                    attempt_json: "secret verifier".to_owned(),
                    registration_method: "dynamic".to_owned(),
                    completion_mode: "server_callback".to_owned(),
                    status: "pending".to_owned(),
                    failure_reason: None,
                    expires_at_ms: now_ms() + 60_000,
                },
                "state",
            )
            .await
            .unwrap();
        assert!(
            first
                .claim_attempt("crashed", "dead-worker", now_ms() + 5)
                .await
                .unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            second
                .claim_attempt("crashed", "replacement", now_ms() + 5_000)
                .await
                .unwrap()
        );
        assert!(
            !first
                .finish_claimed_attempt("crashed", "dead-worker", "failed", None)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn callback_state_is_digested_and_terminal_attempts_are_not_cancelable() {
        let (first, _) = stores().await;
        let attempt = DurableAttemptRecord {
            attempt_id: "digested".to_owned(),
            target_key: "mcp:remote".to_owned(),
            attempt_json: "{}".to_owned(),
            registration_method: "dynamic".to_owned(),
            completion_mode: "server_callback".to_owned(),
            status: "pending".to_owned(),
            failure_reason: None,
            expires_at_ms: now_ms() + 60_000,
        };
        first.save_attempt(&attempt, "state-secret").await.unwrap();
        let PersistencePool::Sqlite(pool) = &first.pool else {
            unreachable!();
        };
        let (digest,): (Vec<u8>,) =
            sqlx::query_as("SELECT state_digest FROM outbound_oauth_attempts WHERE attempt_id = ?")
                .bind("digested")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(digest, state_digest("state-secret"));
        assert!(!String::from_utf8_lossy(&digest).contains("state-secret"));

        assert!(
            first
                .transition_pending_attempt("digested", "canceled", None)
                .await
                .unwrap()
        );
        assert!(
            !first
                .transition_pending_attempt("digested", "expired", None)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn active_attempt_quota_cleans_expired_records() {
        let (first, _) = stores().await;
        for index in 0..MAX_ACTIVE_ATTEMPTS_PER_TARGET {
            let attempt = DurableAttemptRecord {
                attempt_id: format!("active-{index}"),
                target_key: "mcp:remote".to_owned(),
                attempt_json: "{}".to_owned(),
                registration_method: "dynamic".to_owned(),
                completion_mode: "server_callback".to_owned(),
                status: "pending".to_owned(),
                failure_reason: None,
                expires_at_ms: now_ms() + 60_000,
            };
            first
                .save_attempt(&attempt, &format!("state-{index}"))
                .await
                .unwrap();
        }
        let over_quota = DurableAttemptRecord {
            attempt_id: "over-quota".to_owned(),
            target_key: "mcp:remote".to_owned(),
            attempt_json: "{}".to_owned(),
            registration_method: "dynamic".to_owned(),
            completion_mode: "server_callback".to_owned(),
            status: "pending".to_owned(),
            failure_reason: None,
            expires_at_ms: now_ms() + 60_000,
        };
        assert!(first.save_attempt(&over_quota, "over-state").await.is_err());

        first
            .transition_pending_attempt("active-0", "expired", None)
            .await
            .unwrap();
        first.save_attempt(&over_quota, "over-state").await.unwrap();
    }

    #[test]
    fn durable_attempt_debug_redacts_secrets() {
        let record = DurableAttemptRecord {
            attempt_id: "attempt-secret".to_owned(),
            target_key: "mcp:remote".to_owned(),
            attempt_json: "verifier-secret".to_owned(),
            registration_method: "dynamic".to_owned(),
            completion_mode: "manual".to_owned(),
            status: "pending".to_owned(),
            failure_reason: None,
            expires_at_ms: 1,
        };
        let debug = format!("{record:?}");
        assert!(!debug.contains("attempt-secret"));
        assert!(!debug.contains("state-secret"));
        assert!(!debug.contains("verifier-secret"));
    }
}
