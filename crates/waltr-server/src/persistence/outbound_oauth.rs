use waltr_agent::{
    OAuthCredential, OAuthCredentialRecord, OAuthCredentialStore, OAuthCredentialStoreFuture,
    OAuthRefreshClaim, OAuthRefreshClaimResult,
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
        credential: &OAuthCredential,
    ) -> Result<(), String> {
        let json = encode_credential(credential)?;
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(sanitized_store_error)?;
                sqlx::query("INSERT INTO outbound_oauth_credentials (target_key, credential_json, revision, updated_at_ms) VALUES (?, ?, 0, ?) ON CONFLICT(target_key) DO UPDATE SET credential_json = excluded.credential_json, revision = outbound_oauth_credentials.revision + 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = 0, updated_at_ms = excluded.updated_at_ms")
                    .bind(target_key).bind(&json).bind(now_ms()).execute(&mut *transaction).await.map_err(sanitized_store_error)?;
                let affected = sqlx::query("UPDATE outbound_oauth_attempts SET status = 'succeeded', completion_claim_id = NULL, completion_claim_expires_at_ms = NULL, failure_reason = NULL, updated_at_ms = ? WHERE attempt_id = ? AND status = 'completing' AND completion_claim_id = ?")
                    .bind(now_ms()).bind(attempt_id).bind(claim_id).execute(&mut *transaction).await.map_err(sanitized_store_error)?.rows_affected();
                if affected != 1 {
                    return Err("outbound OAuth attempt changed during completion".to_owned());
                }
                transaction.commit().await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                let mut transaction = pool.begin().await.map_err(sanitized_store_error)?;
                sqlx::query("INSERT INTO waltr.outbound_oauth_credentials (target_key, credential_json, revision, updated_at_ms) VALUES ($1, $2, 0, $3) ON CONFLICT(target_key) DO UPDATE SET credential_json = EXCLUDED.credential_json, revision = waltr.outbound_oauth_credentials.revision + 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = FALSE, updated_at_ms = EXCLUDED.updated_at_ms")
                    .bind(target_key).bind(&json).bind(now_ms()).execute(&mut *transaction).await.map_err(sanitized_store_error)?;
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

    async fn mark_reauthorization(&self, target_key: &str, revision: u64) -> Result<(), String> {
        let revision = revision_i64(revision)?;
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                sqlx::query("UPDATE outbound_oauth_credentials SET reauthorization_required = 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = ? WHERE target_key = ? AND revision = ?")
                .bind(now_ms()).bind(target_key).bind(revision).execute(pool).await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                sqlx::query("UPDATE waltr.outbound_oauth_credentials SET reauthorization_required = TRUE, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = $1 WHERE target_key = $2 AND revision = $3")
                .bind(now_ms()).bind(target_key).bind(revision).execute(pool).await.map_err(sanitized_store_error)?;
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

impl OAuthCredentialStore for OutboundOAuthStore {
    fn read(
        &self,
        target_key: String,
    ) -> OAuthCredentialStoreFuture<'_, Option<OAuthCredentialRecord>> {
        Box::pin(async move { self.read_credential(&target_key).await })
    }

    fn write(
        &self,
        target_key: String,
        credential: OAuthCredential,
    ) -> OAuthCredentialStoreFuture<'_, ()> {
        Box::pin(async move { self.write_credential(&target_key, &credential).await })
    }

    fn delete(&self, target_key: String) -> OAuthCredentialStoreFuture<'_, ()> {
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
        claim: OAuthRefreshClaim,
    ) -> OAuthCredentialStoreFuture<'_, OAuthRefreshClaimResult> {
        Box::pin(async move {
            let now = now_ms();
            let revision = revision_i64(claim.revision)?;
            let affected = match &self.pool {
                PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_credentials SET refresh_claim_id = ?, refresh_claim_expires_at_ms = ?, updated_at_ms = ? WHERE target_key = ? AND revision = ? AND reauthorization_required = 0 AND refresh_claim_id IS NULL")
                    .bind(&claim.claim_id).bind(claim.lease_expires_at_ms).bind(now).bind(&target_key).bind(revision).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
                PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_credentials SET refresh_claim_id = $1, refresh_claim_expires_at_ms = $2, updated_at_ms = $3 WHERE target_key = $4 AND revision = $5 AND reauthorization_required = FALSE AND refresh_claim_id IS NULL")
                    .bind(&claim.claim_id).bind(claim.lease_expires_at_ms).bind(now).bind(&target_key).bind(revision).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            };
            if affected == 1 {
                return Ok(OAuthRefreshClaimResult::Claimed(claim));
            }
            for _ in 0..310 {
                let Some((record, claimed_until, reauth)) =
                    self.read_credential_row(&target_key).await?
                else {
                    return Ok(OAuthRefreshClaimResult::ReauthorizationRequired);
                };
                if reauth {
                    return Ok(OAuthRefreshClaimResult::ReauthorizationRequired);
                }
                if claimed_until.is_some_and(|expiry| expiry <= now_ms()) {
                    self.mark_reauthorization(&target_key, claim.revision)
                        .await?;
                    return Ok(OAuthRefreshClaimResult::ReauthorizationRequired);
                }
                if record.revision != claim.revision {
                    return Ok(OAuthRefreshClaimResult::Current(Box::new(record)));
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
            let json = encode_credential(&credential)?;
            let revision = revision_i64(claim.revision)?;
            let affected = match &self.pool {
                PersistencePool::Sqlite(pool) => sqlx::query("UPDATE outbound_oauth_credentials SET credential_json = ?, revision = revision + 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = ? WHERE target_key = ? AND revision = ? AND refresh_claim_id = ? AND reauthorization_required = 0")
                    .bind(json).bind(now_ms()).bind(target_key).bind(revision).bind(claim.claim_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
                PersistencePool::PostgreSql(pool) => sqlx::query("UPDATE waltr.outbound_oauth_credentials SET credential_json = $1, revision = revision + 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, updated_at_ms = $2 WHERE target_key = $3 AND revision = $4 AND refresh_claim_id = $5 AND reauthorization_required = FALSE")
                    .bind(json).bind(now_ms()).bind(target_key).bind(revision).bind(claim.claim_id).execute(pool).await.map_err(sanitized_store_error)?.rows_affected(),
            };
            Ok(affected == 1)
        })
    }

    fn abort_refresh(
        &self,
        target_key: String,
        claim: OAuthRefreshClaim,
        indeterminate: bool,
    ) -> OAuthCredentialStoreFuture<'_, ()> {
        Box::pin(async move {
            let revision = revision_i64(claim.revision)?;
            match &self.pool {
                PersistencePool::Sqlite(pool) => {
                    sqlx::query("UPDATE outbound_oauth_credentials SET refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = ?, updated_at_ms = ? WHERE target_key = ? AND revision = ? AND refresh_claim_id = ?")
                    .bind(indeterminate).bind(now_ms()).bind(target_key).bind(revision).bind(claim.claim_id).execute(pool).await.map_err(sanitized_store_error)?;
                }
                PersistencePool::PostgreSql(pool) => {
                    sqlx::query("UPDATE waltr.outbound_oauth_credentials SET refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = $1, updated_at_ms = $2 WHERE target_key = $3 AND revision = $4 AND refresh_claim_id = $5")
                    .bind(indeterminate).bind(now_ms()).bind(target_key).bind(revision).bind(claim.claim_id).execute(pool).await.map_err(sanitized_store_error)?;
                }
            }
            Ok(())
        })
    }
}

impl OutboundOAuthStore {
    async fn read_credential(
        &self,
        target_key: &str,
    ) -> Result<Option<OAuthCredentialRecord>, String> {
        self.read_credential_row(target_key)
            .await
            .map(|row| row.map(|row| row.0))
    }

    async fn read_credential_row(
        &self,
        target_key: &str,
    ) -> Result<Option<(OAuthCredentialRecord, Option<i64>, bool)>, String> {
        match &self.pool {
            PersistencePool::Sqlite(pool) => sqlx::query("SELECT credential_json, revision, refresh_claim_expires_at_ms, reauthorization_required FROM outbound_oauth_credentials WHERE target_key = ?")
                .bind(target_key).fetch_optional(pool).await.map_err(sanitized_store_error)?
                .as_ref().map(parse_credential_row).transpose(),
            PersistencePool::PostgreSql(pool) => sqlx::query("SELECT credential_json, revision, refresh_claim_expires_at_ms, reauthorization_required FROM waltr.outbound_oauth_credentials WHERE target_key = $1")
                .bind(target_key).fetch_optional(pool).await.map_err(sanitized_store_error)?
                .as_ref().map(parse_credential_row).transpose(),
        }
    }

    async fn write_credential(
        &self,
        target_key: &str,
        credential: &OAuthCredential,
    ) -> Result<(), String> {
        let json = encode_credential(credential)?;
        match &self.pool {
            PersistencePool::Sqlite(pool) => {
                sqlx::query("INSERT INTO outbound_oauth_credentials (target_key, credential_json, revision, updated_at_ms) VALUES (?, ?, 0, ?) ON CONFLICT(target_key) DO UPDATE SET credential_json = excluded.credential_json, revision = outbound_oauth_credentials.revision + 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = 0, updated_at_ms = excluded.updated_at_ms")
                .bind(target_key).bind(json).bind(now_ms()).execute(pool).await.map_err(sanitized_store_error)?;
            }
            PersistencePool::PostgreSql(pool) => {
                sqlx::query("INSERT INTO waltr.outbound_oauth_credentials (target_key, credential_json, revision, updated_at_ms) VALUES ($1, $2, 0, $3) ON CONFLICT(target_key) DO UPDATE SET credential_json = EXCLUDED.credential_json, revision = waltr.outbound_oauth_credentials.revision + 1, refresh_claim_id = NULL, refresh_claim_expires_at_ms = NULL, reauthorization_required = FALSE, updated_at_ms = EXCLUDED.updated_at_ms")
                .bind(target_key).bind(json).bind(now_ms()).execute(pool).await.map_err(sanitized_store_error)?;
            }
        }
        Ok(())
    }
}

fn parse_credential_row<R>(row: &R) -> Result<(OAuthCredentialRecord, Option<i64>, bool), String>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    i64: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    bool: for<'a> sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    let json: String = row
        .try_get("credential_json")
        .map_err(sanitized_store_error)?;
    let credential = serde_json::from_str(&json)
        .map_err(|_| "stored outbound OAuth credential is invalid".to_owned())?;
    let revision: i64 = row.try_get("revision").map_err(sanitized_store_error)?;
    let revision = u64::try_from(revision)
        .map_err(|_| "stored outbound OAuth credential revision is invalid".to_owned())?;
    let expiry = row
        .try_get("refresh_claim_expires_at_ms")
        .map_err(sanitized_store_error)?;
    let reauth = row
        .try_get("reauthorization_required")
        .map_err(sanitized_store_error)?;
    Ok((
        OAuthCredentialRecord {
            credential,
            revision,
            reauthorization_required: reauth,
        },
        expiry,
        reauth,
    ))
}

fn encode_credential(credential: &OAuthCredential) -> Result<String, String> {
    serde_json::to_string(credential)
        .map_err(|_| "failed to encode outbound OAuth credential".to_owned())
}

fn revision_i64(revision: u64) -> Result<i64, String> {
    i64::try_from(revision)
        .map_err(|_| "outbound OAuth credential revision is out of range".to_owned())
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
        McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration, OAuthCredentialStore as _,
        OAuthTokenSet,
    };

    use super::*;
    use crate::persistence::{DatabaseConfig, Persistence};

    fn state(_generation: u64, access: &str) -> OAuthCredential {
        OAuthCredential::mcp(
            OAuthTokenSet::bearer(access, Some("refresh".to_owned()), None),
            McpOAuthContext {
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
            },
        )
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

    fn claim(revision: u64, id: &str, lease_expires_at_ms: i64) -> OAuthRefreshClaim {
        OAuthRefreshClaim {
            revision,
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
                .claim_refresh("mcp:remote".to_owned(), claim(0, "first", now_ms() + 5_000))
                .await
                .unwrap(),
            OAuthRefreshClaimResult::Claimed(_)
        ));
        let waiter = tokio::spawn({
            let second = Arc::clone(&second);
            async move {
                second
                    .claim_refresh(
                        "mcp:remote".to_owned(),
                        claim(0, "second", now_ms() + 5_000),
                    )
                    .await
                    .unwrap()
            }
        });
        let mut refreshed = state(4, "new");
        refreshed.token.refresh_token = Some(oauth2::RefreshToken::new("rotated".to_owned()));
        assert!(
            first
                .commit_refresh(
                    "mcp:remote".to_owned(),
                    claim(0, "first", now_ms() + 5_000),
                    refreshed.clone()
                )
                .await
                .unwrap()
        );
        let OAuthRefreshClaimResult::Current(current) = waiter.await.unwrap() else {
            panic!("waiter should observe the current credential");
        };
        assert_eq!(current.credential, refreshed);
        assert_eq!(current.revision, 1);
        assert!(
            !second
                .commit_refresh(
                    "mcp:remote".to_owned(),
                    claim(0, "second", now_ms() + 5_000),
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
            .abort_refresh(
                "mcp:abort".to_owned(),
                claim(0, "claim", now_ms() + 5_000),
                true,
            )
            .await
            .unwrap();
        assert!(
            first
                .read("mcp:abort".to_owned())
                .await
                .unwrap()
                .unwrap()
                .reauthorization_required
        );
        assert_eq!(
            second
                .claim_refresh("mcp:abort".to_owned(), claim(0, "next", now_ms() + 5_000))
                .await
                .unwrap(),
            OAuthRefreshClaimResult::ReauthorizationRequired
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
            OAuthRefreshClaimResult::ReauthorizationRequired
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
                .credential
                .token
                .access_token
                .secret(),
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
