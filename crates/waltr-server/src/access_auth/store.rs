use std::{error::Error, fmt, time::SystemTime};

use sha2::{Digest as _, Sha256};
use sqlx::{PgPool, SqlitePool};

use crate::persistence::{DatabaseBackend, Persistence};

/// Maximum number of unconsumed, unexpired login attempts retained across server instances.
pub const MAX_ACTIVE_LOGIN_ATTEMPTS: u32 = 1_024;
/// Maximum number of active browser sessions retained across all identities.
pub const MAX_ACTIVE_BROWSER_SESSIONS: u32 = 1_024;
/// Maximum number of active browser sessions retained for one issuer and subject.
pub const MAX_ACTIVE_BROWSER_SESSIONS_PER_SUBJECT: u32 = 32;

const POSTGRES_LOGIN_ADMISSION_LOCK_ID: i64 = 0x4b55_5249_4c4f_4749;
const POSTGRES_SESSION_ADMISSION_LOCK_ID: i64 = 0x4b55_5249_5345_5353;

/// SHA-256 digest used to correlate an OIDC authorization response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginStateDigest(String);

impl LoginStateDigest {
    /// Hash a plaintext state value for persistence and lookup.
    #[must_use]
    pub fn from_state(state: &str) -> Self {
        Self(hex_sha256(state.as_bytes()))
    }
}

/// SHA-256 digest binding an OIDC login attempt to its initiating browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginCorrelationDigest(String);

impl LoginCorrelationDigest {
    /// Hash a plaintext browser correlation value for persistence and lookup.
    #[must_use]
    pub fn from_correlation(correlation: &str) -> Self {
        Self(hex_sha256(correlation.as_bytes()))
    }
}

/// SHA-256 digest used to identify an opaque browser session token.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BrowserSessionTokenDigest(String);

impl BrowserSessionTokenDigest {
    /// Hash a plaintext browser session token for persistence and lookup.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        Self(hex_sha256(token.as_bytes()))
    }
}

/// A new, unconsumed OIDC browser login attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginAttempt {
    /// Digest of the secret state sent to the authorization endpoint.
    pub state_digest: LoginStateDigest,
    /// Digest of the independent secret retained only by the initiating browser.
    pub correlation_digest: LoginCorrelationDigest,
    /// Nonce expected in the resulting ID token.
    pub nonce: String,
    /// Verifier used to exchange the authorization code.
    pub pkce_verifier: String,
    /// Validated local absolute path used after login.
    pub return_path: String,
    /// Time at which the attempt was created.
    pub created_at: SystemTime,
    /// Time after which the attempt cannot be consumed.
    pub expires_at: SystemTime,
}

/// Login data returned to the single callback that consumes an attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumedLoginAttempt {
    /// Nonce expected in the resulting ID token.
    pub nonce: String,
    /// Verifier used to exchange the authorization code.
    pub pkce_verifier: String,
    /// Validated local absolute path used after login.
    pub return_path: String,
    /// Time at which the attempt was created.
    pub created_at: SystemTime,
    /// Time after which the attempt would have become invalid.
    pub expires_at: SystemTime,
    /// Time at which this callback claimed the attempt.
    pub consumed_at: SystemTime,
}

/// A new browser session backed by an opaque token digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserSession {
    /// Digest of the opaque token sent to the browser.
    pub token_digest: BrowserSessionTokenDigest,
    /// OIDC issuer that authenticated the browser.
    pub issuer: String,
    /// OIDC client that created the browser session.
    pub client_id: String,
    /// OIDC subject that authenticated the browser.
    pub subject: String,
    /// Time at which the session was created.
    pub created_at: SystemTime,
    /// Time after which the session cannot authenticate requests.
    pub expires_at: SystemTime,
    /// Most recent known use of the session.
    pub last_seen_at: SystemTime,
}

/// Authentication evidence for an active browser session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveBrowserSession {
    /// OIDC issuer that authenticated the browser.
    pub issuer: String,
    /// OIDC client that created the browser session.
    pub client_id: String,
    /// OIDC subject that authenticated the browser.
    pub subject: String,
    /// Time at which the session was created.
    pub created_at: SystemTime,
    /// Time after which the session cannot authenticate requests.
    pub expires_at: SystemTime,
    /// Most recent known use, including the successful lookup.
    pub last_seen_at: SystemTime,
}

/// Number of inactive records removed by opportunistic cleanup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupResult {
    /// Number of consumed or expired login attempts removed.
    pub login_attempts: u64,
    /// Number of expired or revoked browser sessions removed.
    pub browser_sessions: u64,
}

/// Failure to validate or persist access-authentication state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessAuthStoreError {
    /// A timestamp cannot be represented as a signed millisecond value.
    TimestampOutOfRange,
    /// A record violates an access-authentication lifecycle invariant.
    InvalidRecord(&'static str),
    /// The database-side active login-attempt cap has been reached.
    LoginAttemptCapacity,
    /// The server-wide active browser-session cap has been reached.
    BrowserSessionGlobalCapacity,
    /// The active browser-session cap for one issuer and subject has been reached.
    BrowserSessionIssuerSubjectCapacity,
    /// The selected persistence backend rejected a store operation.
    Database(DatabaseBackend),
}

impl fmt::Display for AccessAuthStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimestampOutOfRange => {
                formatter.write_str("access authentication timestamp is out of range")
            }
            Self::InvalidRecord(message) => formatter.write_str(message),
            Self::LoginAttemptCapacity => {
                formatter.write_str("active OIDC login attempt capacity has been reached")
            }
            Self::BrowserSessionGlobalCapacity => {
                formatter.write_str("global active browser session capacity has been reached")
            }
            Self::BrowserSessionIssuerSubjectCapacity => formatter
                .write_str("issuer-subject active browser session capacity has been reached"),
            Self::Database(backend) => write!(
                formatter,
                "failed to access authentication state in the {backend} database"
            ),
        }
    }
}

impl Error for AccessAuthStoreError {}

/// Cloneable SQL-backed persistence for OIDC login attempts and browser sessions.
#[derive(Clone, Debug)]
pub struct AccessAuthStore {
    pool: AccessAuthPool,
}

#[derive(Clone, Debug)]
enum AccessAuthPool {
    Sqlite(SqlitePool),
    PostgreSql(PgPool),
}

impl AccessAuthStore {
    /// Construct a store from initialized server persistence.
    ///
    /// # Panics
    ///
    /// Panics if persistence contains neither a `SQLite` nor `PostgreSQL` pool.
    #[must_use]
    pub fn new(persistence: &Persistence) -> Self {
        let pool = persistence.sqlite_pool().map_or_else(
            || {
                AccessAuthPool::PostgreSql(
                    persistence
                        .postgres_pool()
                        .expect("Persistence always contains one supported pool")
                        .clone(),
                )
            },
            |pool| AccessAuthPool::Sqlite(pool.clone()),
        );
        Self { pool }
    }

    /// Persist a new one-time login attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid records, exhausted capacity, or database failures.
    pub async fn insert_login_attempt(
        &self,
        attempt: &LoginAttempt,
    ) -> Result<(), AccessAuthStoreError> {
        self.insert_login_attempt_with_limit(attempt, MAX_ACTIVE_LOGIN_ATTEMPTS)
            .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "admission must remain atomic across each database transaction"
    )]
    async fn insert_login_attempt_with_limit(
        &self,
        attempt: &LoginAttempt,
        active_limit: u32,
    ) -> Result<(), AccessAuthStoreError> {
        validate_return_path(&attempt.return_path)?;
        if attempt.nonce.is_empty() || attempt.pkce_verifier.is_empty() {
            return Err(AccessAuthStoreError::InvalidRecord(
                "OIDC nonce and PKCE verifier must not be empty",
            ));
        }
        let created_at = system_time_to_millis(attempt.created_at)?;
        let expires_at = system_time_to_millis(attempt.expires_at)?;
        if expires_at <= created_at {
            return Err(AccessAuthStoreError::InvalidRecord(
                "OIDC login attempt expiry must follow creation",
            ));
        }

        if active_limit == 0 {
            return Err(AccessAuthStoreError::LoginAttemptCapacity);
        }

        let rows_affected = match &self.pool {
            AccessAuthPool::Sqlite(pool) => sqlx::query(
                r"
INSERT INTO waltr_oidc_login_attempts
    (state_digest, correlation_digest, nonce, pkce_verifier, return_path, created_at_ms, expires_at_ms)
SELECT ?, ?, ?, ?, ?, ?, ?
WHERE (
    SELECT count(*) FROM waltr_oidc_login_attempts
    WHERE consumed_at_ms IS NULL AND expires_at_ms > ?
) < ?
                ",
            )
            .bind(&attempt.state_digest.0)
            .bind(&attempt.correlation_digest.0)
            .bind(&attempt.nonce)
            .bind(&attempt.pkce_verifier)
            .bind(&attempt.return_path)
            .bind(created_at)
            .bind(expires_at)
            .bind(created_at)
            .bind(i64::from(active_limit))
            .execute(pool)
            .await
            .map_err(|_| database_error(DatabaseBackend::Sqlite))?
            .rows_affected(),
            AccessAuthPool::PostgreSql(pool) => {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                sqlx::query("SELECT pg_advisory_xact_lock($1)")
                    .bind(POSTGRES_LOGIN_ADMISSION_LOCK_ID)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                let rows_affected = sqlx::query(
                    r"
INSERT INTO waltr.oidc_login_attempts
    (state_digest, correlation_digest, nonce, pkce_verifier, return_path, created_at_ms, expires_at_ms)
SELECT $1, $2, $3, $4, $5, $6, $7
WHERE (
    SELECT count(*) FROM waltr.oidc_login_attempts
    WHERE consumed_at_ms IS NULL AND expires_at_ms > $6
) < $8
                ",
                )
                .bind(&attempt.state_digest.0)
                .bind(&attempt.correlation_digest.0)
                .bind(&attempt.nonce)
                .bind(&attempt.pkce_verifier)
                .bind(&attempt.return_path)
                .bind(created_at)
                .bind(expires_at)
                .bind(i64::from(active_limit))
                .execute(&mut *transaction)
                .await
                .map_err(|_| database_error(DatabaseBackend::PostgreSql))?
                .rows_affected();
                transaction
                    .commit()
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                rows_affected
            }
        };
        if rows_affected == 0 {
            Err(AccessAuthStoreError::LoginAttemptCapacity)
        } else {
            Ok(())
        }
    }

    /// Consume one matching unconsumed attempt when it is active at `now`.
    ///
    /// # Errors
    ///
    /// Returns an error when time conversion, querying, or row decoding fails.
    pub async fn consume_login_attempt(
        &self,
        state_digest: &LoginStateDigest,
        correlation_digest: &LoginCorrelationDigest,
        now: SystemTime,
    ) -> Result<Option<ConsumedLoginAttempt>, AccessAuthStoreError> {
        let now = system_time_to_millis(now)?;
        match &self.pool {
            AccessAuthPool::Sqlite(pool) => {
                let row = sqlx::query(
                    r"
UPDATE waltr_oidc_login_attempts
SET consumed_at_ms = ?
WHERE state_digest = ?
  AND correlation_digest = ?
  AND consumed_at_ms IS NULL
  AND created_at_ms <= ?
  AND expires_at_ms > ?
RETURNING nonce, pkce_verifier, return_path, created_at_ms, expires_at_ms, consumed_at_ms
                ",
                )
                .bind(now)
                .bind(&state_digest.0)
                .bind(&correlation_digest.0)
                .bind(now)
                .bind(now)
                .fetch_optional(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::Sqlite))?;
                decode_consumed_login_row(row)
            }
            AccessAuthPool::PostgreSql(pool) => {
                let row = sqlx::query(
                    r"
UPDATE waltr.oidc_login_attempts
SET consumed_at_ms = $1
WHERE state_digest = $2
  AND correlation_digest = $3
  AND consumed_at_ms IS NULL
  AND created_at_ms <= $1
  AND expires_at_ms > $1
RETURNING nonce, pkce_verifier, return_path, created_at_ms, expires_at_ms, consumed_at_ms
                ",
                )
                .bind(now)
                .bind(&state_digest.0)
                .bind(&correlation_digest.0)
                .fetch_optional(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                decode_consumed_login_row(row)
            }
        }
    }

    /// Persist a browser session identified only by its token digest.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid records, exhausted capacity, or database failures.
    pub async fn insert_browser_session(
        &self,
        session: &BrowserSession,
    ) -> Result<(), AccessAuthStoreError> {
        self.insert_browser_session_with_limits(
            session,
            MAX_ACTIVE_BROWSER_SESSIONS,
            MAX_ACTIVE_BROWSER_SESSIONS_PER_SUBJECT,
        )
        .await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "admission and capacity diagnosis share one atomic transaction"
    )]
    async fn insert_browser_session_with_limits(
        &self,
        session: &BrowserSession,
        global_limit: u32,
        subject_limit: u32,
    ) -> Result<(), AccessAuthStoreError> {
        if session.issuer.is_empty() || session.client_id.is_empty() || session.subject.is_empty() {
            return Err(AccessAuthStoreError::InvalidRecord(
                "browser session issuer, client ID, and subject must not be empty",
            ));
        }
        let created_at = system_time_to_millis(session.created_at)?;
        let expires_at = system_time_to_millis(session.expires_at)?;
        let last_seen_at = system_time_to_millis(session.last_seen_at)?;
        if expires_at <= created_at || last_seen_at < created_at || last_seen_at > expires_at {
            return Err(AccessAuthStoreError::InvalidRecord(
                "browser session timestamps are inconsistent",
            ));
        }
        if global_limit == 0 {
            return Err(AccessAuthStoreError::BrowserSessionGlobalCapacity);
        }
        if subject_limit == 0 {
            return Err(AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity);
        }

        match &self.pool {
            AccessAuthPool::Sqlite(pool) => {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|_| database_error(DatabaseBackend::Sqlite))?;
                let rows_affected = sqlx::query(
                    r"
INSERT INTO waltr_browser_sessions
    (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms)
SELECT ?, ?, ?, ?, ?, ?, ?
WHERE (
    SELECT count(*) FROM waltr_browser_sessions
    WHERE revoked_at_ms IS NULL AND expires_at_ms > ?
) < ?
AND (
    SELECT count(*) FROM waltr_browser_sessions
    WHERE revoked_at_ms IS NULL AND expires_at_ms > ? AND issuer = ? AND subject = ?
) < ?
                ",
                )
                .bind(&session.token_digest.0)
                .bind(&session.issuer)
                .bind(&session.client_id)
                .bind(&session.subject)
                .bind(created_at)
                .bind(expires_at)
                .bind(last_seen_at)
                .bind(created_at)
                .bind(i64::from(global_limit))
                .bind(created_at)
                .bind(&session.issuer)
                .bind(&session.subject)
                .bind(i64::from(subject_limit))
                .execute(&mut *transaction)
                .await
                .map_err(|_| database_error(DatabaseBackend::Sqlite))?
                .rows_affected();
                if rows_affected == 0 {
                    let (global, subject) = sqlx::query_as::<_, (i64, i64)>(
                        r"
SELECT count(*), coalesce(sum(CASE WHEN issuer = ? AND subject = ? THEN 1 ELSE 0 END), 0)
FROM waltr_browser_sessions
WHERE revoked_at_ms IS NULL AND expires_at_ms > ?
                        ",
                    )
                    .bind(&session.issuer)
                    .bind(&session.subject)
                    .bind(created_at)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|_| database_error(DatabaseBackend::Sqlite))?;
                    return Err(session_capacity_error(
                        global,
                        subject,
                        global_limit,
                        subject_limit,
                    ));
                }
                transaction
                    .commit()
                    .await
                    .map_err(|_| database_error(DatabaseBackend::Sqlite))?;
                Ok(())
            }
            AccessAuthPool::PostgreSql(pool) => {
                let mut transaction = pool
                    .begin()
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                sqlx::query("SELECT pg_advisory_xact_lock($1)")
                    .bind(POSTGRES_SESSION_ADMISSION_LOCK_ID)
                    .execute(&mut *transaction)
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                let rows_affected = sqlx::query(
                    r"
INSERT INTO waltr.browser_sessions
    (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms)
SELECT $1, $2, $3, $4, $5, $6, $7
WHERE (
    SELECT count(*) FROM waltr.browser_sessions
    WHERE revoked_at_ms IS NULL AND expires_at_ms > $5
) < $8
AND (
    SELECT count(*) FROM waltr.browser_sessions
    WHERE revoked_at_ms IS NULL AND expires_at_ms > $5 AND issuer = $2 AND subject = $4
) < $9
                ",
                )
                .bind(&session.token_digest.0)
                .bind(&session.issuer)
                .bind(&session.client_id)
                .bind(&session.subject)
                .bind(created_at)
                .bind(expires_at)
                .bind(last_seen_at)
                .bind(i64::from(global_limit))
                .bind(i64::from(subject_limit))
                .execute(&mut *transaction)
                .await
                .map_err(|_| database_error(DatabaseBackend::PostgreSql))?
                .rows_affected();
                if rows_affected == 0 {
                    let (global, subject) = sqlx::query_as::<_, (i64, i64)>(
                        r"
SELECT count(*), count(*) FILTER (WHERE issuer = $1 AND subject = $2)
FROM waltr.browser_sessions
WHERE revoked_at_ms IS NULL AND expires_at_ms > $3
                        ",
                    )
                    .bind(&session.issuer)
                    .bind(&session.subject)
                    .bind(created_at)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                    return Err(session_capacity_error(
                        global,
                        subject,
                        global_limit,
                        subject_limit,
                    ));
                }
                transaction
                    .commit()
                    .await
                    .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                Ok(())
            }
        }
    }

    /// Look up an active session and monotonically advance its last-seen time.
    ///
    /// # Errors
    ///
    /// Returns an error when time conversion, querying, or row decoding fails.
    pub async fn lookup_browser_session(
        &self,
        token_digest: &BrowserSessionTokenDigest,
        expected_issuer: &str,
        expected_client_id: &str,
        now: SystemTime,
    ) -> Result<Option<ActiveBrowserSession>, AccessAuthStoreError> {
        let now = system_time_to_millis(now)?;
        match &self.pool {
            AccessAuthPool::Sqlite(pool) => {
                let row = sqlx::query(
                    r"
UPDATE waltr_browser_sessions
SET last_seen_at_ms = max(last_seen_at_ms, ?)
WHERE token_digest = ?
  AND issuer = ?
  AND client_id = ?
  AND revoked_at_ms IS NULL
  AND created_at_ms <= ?
  AND expires_at_ms > ?
RETURNING issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms
                ",
                )
                .bind(now)
                .bind(&token_digest.0)
                .bind(expected_issuer)
                .bind(expected_client_id)
                .bind(now)
                .bind(now)
                .fetch_optional(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::Sqlite))?;
                decode_active_session_row(row)
            }
            AccessAuthPool::PostgreSql(pool) => {
                let row = sqlx::query(
                    r"
UPDATE waltr.browser_sessions
SET last_seen_at_ms = GREATEST(last_seen_at_ms, $1)
WHERE token_digest = $2
  AND issuer = $3
  AND client_id = $4
  AND revoked_at_ms IS NULL
  AND created_at_ms <= $1
  AND expires_at_ms > $1
RETURNING issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms
                ",
                )
                .bind(now)
                .bind(&token_digest.0)
                .bind(expected_issuer)
                .bind(expected_client_id)
                .fetch_optional(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::PostgreSql))?;
                decode_active_session_row(row)
            }
        }
    }

    /// Return whether a session is active without updating its last-seen time.
    ///
    /// # Errors
    ///
    /// Returns an error when time conversion or the database query fails.
    pub async fn is_browser_session_active(
        &self,
        token_digest: &BrowserSessionTokenDigest,
        now: SystemTime,
    ) -> Result<bool, AccessAuthStoreError> {
        let now = system_time_to_millis(now)?;
        match &self.pool {
            AccessAuthPool::Sqlite(pool) => sqlx::query_scalar(
                r"
SELECT EXISTS(
    SELECT 1 FROM waltr_browser_sessions
    WHERE token_digest = ?
      AND revoked_at_ms IS NULL
      AND created_at_ms <= ?
      AND expires_at_ms > ?
)
                ",
            )
            .bind(&token_digest.0)
            .bind(now)
            .bind(now)
            .fetch_one(pool)
            .await
            .map_err(|_| database_error(DatabaseBackend::Sqlite)),
            AccessAuthPool::PostgreSql(pool) => sqlx::query_scalar(
                r"
SELECT EXISTS(
    SELECT 1 FROM waltr.browser_sessions
    WHERE token_digest = $1
      AND revoked_at_ms IS NULL
      AND created_at_ms <= $2
      AND expires_at_ms > $2
)
                ",
            )
            .bind(&token_digest.0)
            .bind(now)
            .fetch_one(pool)
            .await
            .map_err(|_| database_error(DatabaseBackend::PostgreSql)),
        }
    }

    /// Revoke active sessions created under a different OIDC configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when time conversion or the database update fails.
    pub async fn revoke_browser_sessions_outside_configuration(
        &self,
        issuer: &str,
        client_id: &str,
        now: SystemTime,
    ) -> Result<(), AccessAuthStoreError> {
        let now = system_time_to_millis(now)?;
        match &self.pool {
            AccessAuthPool::Sqlite(pool) => sqlx::query(
                r"
UPDATE waltr_browser_sessions
SET revoked_at_ms = ?
WHERE revoked_at_ms IS NULL
  AND (issuer <> ? OR client_id <> ?)
                ",
            )
            .bind(now)
            .bind(issuer)
            .bind(client_id)
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|_| database_error(DatabaseBackend::Sqlite)),
            AccessAuthPool::PostgreSql(pool) => sqlx::query(
                r"
UPDATE waltr.browser_sessions
SET revoked_at_ms = $1
WHERE revoked_at_ms IS NULL
  AND (issuer <> $2 OR client_id <> $3)
                ",
            )
            .bind(now)
            .bind(issuer)
            .bind(client_id)
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|_| database_error(DatabaseBackend::PostgreSql)),
        }
    }

    /// Revoke a session if present. Repeated revocation leaves the original revocation time intact.
    ///
    /// # Errors
    ///
    /// Returns an error when time conversion or the database update fails.
    pub async fn revoke_browser_session(
        &self,
        token_digest: &BrowserSessionTokenDigest,
        now: SystemTime,
    ) -> Result<(), AccessAuthStoreError> {
        let now = system_time_to_millis(now)?;
        match &self.pool {
            AccessAuthPool::Sqlite(pool) => sqlx::query(
                "UPDATE waltr_browser_sessions SET revoked_at_ms = coalesce(revoked_at_ms, ?) WHERE token_digest = ?",
            )
            .bind(now)
            .bind(&token_digest.0)
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|_| database_error(DatabaseBackend::Sqlite)),
            AccessAuthPool::PostgreSql(pool) => sqlx::query(
                "UPDATE waltr.browser_sessions SET revoked_at_ms = coalesce(revoked_at_ms, $1) WHERE token_digest = $2",
            )
            .bind(now)
            .bind(&token_digest.0)
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|_| database_error(DatabaseBackend::PostgreSql)),
        }
    }

    /// Delete at most `limit` inactive records from each access-authentication table.
    ///
    /// # Errors
    ///
    /// Returns an error when time conversion or a database cleanup query fails.
    #[allow(
        clippy::too_many_lines,
        reason = "cleanup applies the same bounded operation to both database backends"
    )]
    pub async fn cleanup(
        &self,
        now: SystemTime,
        limit: u32,
    ) -> Result<CleanupResult, AccessAuthStoreError> {
        if limit == 0 {
            return Ok(CleanupResult::default());
        }
        let now = system_time_to_millis(now)?;
        let limit = i64::from(limit);
        match &self.pool {
            AccessAuthPool::Sqlite(pool) => {
                let login_attempts = sqlx::query(
                    r"
DELETE FROM waltr_oidc_login_attempts
WHERE state_digest IN (
    SELECT state_digest FROM waltr_oidc_login_attempts
    WHERE consumed_at_ms IS NOT NULL OR expires_at_ms <= ?
    ORDER BY expires_at_ms
    LIMIT ?
)
                    ",
                )
                .bind(now)
                .bind(limit)
                .execute(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::Sqlite))?
                .rows_affected();
                let browser_sessions = sqlx::query(
                    r"
DELETE FROM waltr_browser_sessions
WHERE token_digest IN (
    SELECT token_digest FROM waltr_browser_sessions
    WHERE revoked_at_ms IS NOT NULL OR expires_at_ms <= ?
    ORDER BY expires_at_ms
    LIMIT ?
)
                    ",
                )
                .bind(now)
                .bind(limit)
                .execute(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::Sqlite))?
                .rows_affected();
                Ok(CleanupResult {
                    login_attempts,
                    browser_sessions,
                })
            }
            AccessAuthPool::PostgreSql(pool) => {
                let login_attempts = sqlx::query(
                    r"
DELETE FROM waltr.oidc_login_attempts
WHERE state_digest IN (
    SELECT state_digest FROM waltr.oidc_login_attempts
    WHERE consumed_at_ms IS NOT NULL OR expires_at_ms <= $1
    ORDER BY expires_at_ms
    LIMIT $2
)
                    ",
                )
                .bind(now)
                .bind(limit)
                .execute(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::PostgreSql))?
                .rows_affected();
                let browser_sessions = sqlx::query(
                    r"
DELETE FROM waltr.browser_sessions
WHERE token_digest IN (
    SELECT token_digest FROM waltr.browser_sessions
    WHERE revoked_at_ms IS NOT NULL OR expires_at_ms <= $1
    ORDER BY expires_at_ms
    LIMIT $2
)
                    ",
                )
                .bind(now)
                .bind(limit)
                .execute(pool)
                .await
                .map_err(|_| database_error(DatabaseBackend::PostgreSql))?
                .rows_affected();
                Ok(CleanupResult {
                    login_attempts,
                    browser_sessions,
                })
            }
        }
    }

    /// Attempt bounded inactive-record cleanup without affecting the committed caller operation.
    pub async fn cleanup_best_effort(&self, now: SystemTime, limit: u32) {
        if let Err(error) = self.cleanup(now, limit).await {
            tracing::warn!(%error, "access authentication cleanup failed");
        }
    }
}

fn validate_return_path(path: &str) -> Result<(), AccessAuthStoreError> {
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return Err(AccessAuthStoreError::InvalidRecord(
            "OIDC return path must be a local absolute path",
        ));
    }
    Ok(())
}

fn session_capacity_error(
    global: i64,
    subject: i64,
    global_limit: u32,
    subject_limit: u32,
) -> AccessAuthStoreError {
    if global >= i64::from(global_limit) {
        AccessAuthStoreError::BrowserSessionGlobalCapacity
    } else {
        debug_assert!(subject >= i64::from(subject_limit));
        AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity
    }
}

const fn database_error(backend: DatabaseBackend) -> AccessAuthStoreError {
    AccessAuthStoreError::Database(backend)
}

const fn corrupt_timestamp() -> AccessAuthStoreError {
    AccessAuthStoreError::InvalidRecord("persisted access authentication record is invalid")
}

fn decode_consumed_login_row<R>(
    row: Option<R>,
) -> Result<Option<ConsumedLoginAttempt>, AccessAuthStoreError>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    row.map(|row| {
        Ok(ConsumedLoginAttempt {
            nonce: row.try_get("nonce").map_err(|_| corrupt_timestamp())?,
            pkce_verifier: row
                .try_get("pkce_verifier")
                .map_err(|_| corrupt_timestamp())?,
            return_path: row
                .try_get("return_path")
                .map_err(|_| corrupt_timestamp())?,
            created_at: millis_to_system_time(
                row.try_get("created_at_ms")
                    .map_err(|_| corrupt_timestamp())?,
            )?,
            expires_at: millis_to_system_time(
                row.try_get("expires_at_ms")
                    .map_err(|_| corrupt_timestamp())?,
            )?,
            consumed_at: millis_to_system_time(
                row.try_get("consumed_at_ms")
                    .map_err(|_| corrupt_timestamp())?,
            )?,
        })
    })
    .transpose()
}

fn decode_active_session_row<R>(
    row: Option<R>,
) -> Result<Option<ActiveBrowserSession>, AccessAuthStoreError>
where
    R: sqlx::Row,
    for<'a> &'a str: sqlx::ColumnIndex<R>,
    String: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: for<'r> sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    row.map(|row| {
        Ok(ActiveBrowserSession {
            issuer: row.try_get("issuer").map_err(|_| corrupt_timestamp())?,
            client_id: row.try_get("client_id").map_err(|_| corrupt_timestamp())?,
            subject: row.try_get("subject").map_err(|_| corrupt_timestamp())?,
            created_at: millis_to_system_time(
                row.try_get("created_at_ms")
                    .map_err(|_| corrupt_timestamp())?,
            )?,
            expires_at: millis_to_system_time(
                row.try_get("expires_at_ms")
                    .map_err(|_| corrupt_timestamp())?,
            )?,
            last_seen_at: millis_to_system_time(
                row.try_get("last_seen_at_ms")
                    .map_err(|_| corrupt_timestamp())?,
            )?,
        })
    })
    .transpose()
}

fn system_time_to_millis(time: SystemTime) -> Result<i64, AccessAuthStoreError> {
    let milliseconds = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_millis())
            .map_err(|_| AccessAuthStoreError::TimestampOutOfRange)?,
        Err(error) => -i128::try_from(error.duration().as_millis())
            .map_err(|_| AccessAuthStoreError::TimestampOutOfRange)?,
    };
    i64::try_from(milliseconds).map_err(|_| AccessAuthStoreError::TimestampOutOfRange)
}

fn millis_to_system_time(milliseconds: i64) -> Result<SystemTime, AccessAuthStoreError> {
    let duration = std::time::Duration::from_millis(milliseconds.unsigned_abs());
    if milliseconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(duration)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(duration)
    }
    .ok_or(AccessAuthStoreError::TimestampOutOfRange)
}

fn hex_sha256(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "cross-backend store contracts keep each scenario setup together"
)]
mod tests {
    use std::{env, fs, path::PathBuf, sync::Arc, time::Duration};

    use testcontainers::{
        GenericImage, ImageExt,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };
    use tokio::sync::Barrier;
    use uuid::Uuid;

    use super::*;
    use crate::persistence::{DatabaseConfig, Persistence};

    const BASE_MILLIS: u64 = 1_800_000_000_000;

    #[tokio::test]
    async fn sqlite_attempt_survives_reconstruction_and_persists_only_digest() {
        let database = TestDatabase::new("reconstruct.sqlite3");
        let config = DatabaseConfig::from_url(&database.url()).expect("database config");
        let persistence = Persistence::initialize(config.clone())
            .await
            .expect("initialize persistence");
        let state = "plaintext-state-must-not-be-stored";
        let correlation = "plaintext-correlation-must-not-be-stored";
        let session_token = "plaintext-session-token-must-not-be-stored";
        let attempt = login_attempt_with_correlation(state, correlation, 0, 60_000);
        let session = browser_session(session_token, 0, 120_000);
        let store = AccessAuthStore::new(&persistence);
        store
            .insert_login_attempt(&attempt)
            .await
            .expect("insert login attempt");
        store
            .insert_browser_session(&session)
            .await
            .expect("insert browser session");

        let pool = persistence.sqlite_pool().expect("SQLite pool");
        let persisted_state =
            sqlx::query_scalar::<_, String>("SELECT state_digest FROM waltr_oidc_login_attempts")
                .fetch_one(pool)
                .await
                .expect("read state digest");
        let persisted_correlation = sqlx::query_scalar::<_, String>(
            "SELECT correlation_digest FROM waltr_oidc_login_attempts",
        )
        .fetch_one(pool)
        .await
        .expect("read correlation digest");
        let persisted_token =
            sqlx::query_scalar::<_, String>("SELECT token_digest FROM waltr_browser_sessions")
                .fetch_one(pool)
                .await
                .expect("read token digest");
        assert_eq!(persisted_state, attempt.state_digest.0);
        assert_eq!(persisted_correlation, attempt.correlation_digest.0);
        assert_eq!(persisted_token, session.token_digest.0);
        assert_ne!(persisted_state, state);
        assert_ne!(persisted_correlation, correlation);
        assert_ne!(persisted_token, session_token);
        drop(store);
        drop(persistence);

        let reconstructed = Persistence::initialize(config)
            .await
            .expect("reconstruct persistence");
        let consumed = AccessAuthStore::new(&reconstructed)
            .consume_login_attempt(
                &attempt.state_digest,
                &attempt.correlation_digest,
                time(1_000),
            )
            .await
            .expect("consume reconstructed attempt")
            .expect("attempt exists");
        assert_eq!(consumed.return_path, "/sessions/current");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_attempt_consumption_has_exactly_one_winner() {
        let (_database, persistence) = sqlite("concurrent-consume.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let attempt = login_attempt("one-winner", 0, 60_000);
        store
            .insert_login_attempt(&attempt)
            .await
            .expect("insert attempt");
        let barrier = Arc::new(Barrier::new(3));
        let first = consume_after_barrier(
            store.clone(),
            attempt.state_digest.clone(),
            attempt.correlation_digest.clone(),
            barrier.clone(),
        );
        let second = consume_after_barrier(
            store,
            attempt.state_digest,
            attempt.correlation_digest,
            barrier.clone(),
        );
        let (first, second, _) = tokio::join!(first, second, barrier.wait());
        let winners = [
            first.expect("first consume"),
            second.expect("second consume"),
        ]
        .into_iter()
        .flatten()
        .count();
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn expired_and_replayed_attempts_are_not_consumed() {
        let (_database, persistence) = sqlite("attempt-lifecycle.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let expired = login_attempt("expired", 0, 100);
        let active = login_attempt("replayed", 0, 10_000);
        store
            .insert_login_attempt(&expired)
            .await
            .expect("expired insert");
        store
            .insert_login_attempt(&active)
            .await
            .expect("active insert");

        assert!(
            store
                .consume_login_attempt(
                    &expired.state_digest,
                    &expired.correlation_digest,
                    time(100),
                )
                .await
                .expect("expired consume")
                .is_none()
        );
        assert!(
            store
                .consume_login_attempt(
                    &active.state_digest,
                    &active.correlation_digest,
                    time(1_000),
                )
                .await
                .expect("first consume")
                .is_some()
        );
        assert!(
            store
                .consume_login_attempt(
                    &active.state_digest,
                    &active.correlation_digest,
                    time(2_000),
                )
                .await
                .expect("replayed consume")
                .is_none()
        );
    }

    #[tokio::test]
    async fn mismatched_correlation_does_not_consume_valid_attempt() {
        let (_database, persistence) = sqlite("correlation-mismatch.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let attempt = login_attempt_with_correlation("state", "right-browser", 0, 10_000);
        store
            .insert_login_attempt(&attempt)
            .await
            .expect("insert attempt");

        assert!(
            store
                .consume_login_attempt(
                    &attempt.state_digest,
                    &LoginCorrelationDigest::from_correlation("wrong-browser"),
                    time(1_000),
                )
                .await
                .expect("mismatch lookup")
                .is_none()
        );
        assert!(
            store
                .consume_login_attempt(
                    &attempt.state_digest,
                    &attempt.correlation_digest,
                    time(1_000),
                )
                .await
                .expect("matching lookup")
                .is_some()
        );
    }

    #[tokio::test]
    async fn active_login_attempt_cap_rejects_growth_but_not_expired_rows() {
        let (_database, persistence) = sqlite("attempt-cap.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        for state in ["first", "second"] {
            store
                .insert_login_attempt_with_limit(&login_attempt(state, 0, 10_000), 2)
                .await
                .expect("within cap");
        }
        assert_eq!(
            store
                .insert_login_attempt_with_limit(&login_attempt("third", 0, 10_000), 2)
                .await,
            Err(AccessAuthStoreError::LoginAttemptCapacity)
        );
        store
            .insert_login_attempt_with_limit(&login_attempt("after-expiry", 10_000, 20_000), 2)
            .await
            .expect("expired rows do not consume active capacity");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_login_admission_cannot_exceed_cap() {
        let (_database, persistence) = sqlite("concurrent-cap.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let barrier = Arc::new(Barrier::new(3));
        let first = insert_after_barrier(
            store.clone(),
            login_attempt("cap-first", 0, 10_000),
            1,
            barrier.clone(),
        );
        let second = insert_after_barrier(
            store,
            login_attempt("cap-second", 0, 10_000),
            1,
            barrier.clone(),
        );
        let (first, second, _) = tokio::join!(first, second, barrier.wait());
        let results = [first, second];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AccessAuthStoreError::LoginAttemptCapacity)))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn session_lookup_rejects_expired_and_revoked_sessions() {
        let (_database, persistence) = sqlite("session-lifecycle.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let active = browser_session("active-token", 0, 10_000);
        let expired = browser_session("expired-token", 0, 100);
        let revoked = browser_session("revoked-token", 0, 10_000);
        for session in [&active, &expired, &revoked] {
            store
                .insert_browser_session(session)
                .await
                .expect("insert session");
        }
        store
            .revoke_browser_session(&revoked.token_digest, time(500))
            .await
            .expect("revoke session");

        let found = store
            .lookup_browser_session(
                &active.token_digest,
                &active.issuer,
                &active.client_id,
                time(1_000),
            )
            .await
            .expect("active lookup")
            .expect("active session");
        assert_eq!(found.subject, "subject-123");
        assert_eq!(found.last_seen_at, time(1_000));
        assert!(
            store
                .lookup_browser_session(
                    &expired.token_digest,
                    &expired.issuer,
                    &expired.client_id,
                    time(100),
                )
                .await
                .expect("expired lookup")
                .is_none()
        );
        assert!(
            store
                .lookup_browser_session(
                    &revoked.token_digest,
                    &revoked.issuer,
                    &revoked.client_id,
                    time(1_000),
                )
                .await
                .expect("revoked lookup")
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_lookup_requires_exact_configuration_and_active_check_is_non_mutating() {
        let (_database, persistence) = sqlite("session-configuration.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let session = browser_session("configuration-token", 0, 10_000);
        store.insert_browser_session(&session).await.unwrap();

        assert!(
            store
                .lookup_browser_session(
                    &session.token_digest,
                    "https://other-issuer.example",
                    &session.client_id,
                    time(1_000),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .lookup_browser_session(
                    &session.token_digest,
                    &session.issuer,
                    "other-client",
                    time(1_000),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .is_browser_session_active(&session.token_digest, time(1_000))
                .await
                .unwrap()
        );
        let last_seen = sqlx::query_scalar::<_, i64>(
            "SELECT last_seen_at_ms FROM waltr_browser_sessions WHERE token_digest = ?",
        )
        .bind(&session.token_digest.0)
        .fetch_one(persistence.sqlite_pool().unwrap())
        .await
        .unwrap();
        assert_eq!(
            last_seen,
            system_time_to_millis(session.last_seen_at).unwrap()
        );

        let found = store
            .lookup_browser_session(
                &session.token_digest,
                &session.issuer,
                &session.client_id,
                time(1_000),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.client_id, session.client_id);
        assert_eq!(found.last_seen_at, time(1_000));
    }

    #[tokio::test]
    async fn browser_session_subject_and_global_caps_are_distinct() {
        let (_database, persistence) = sqlite("session-caps.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        for token in ["subject-first", "subject-second"] {
            store
                .insert_browser_session_with_limits(
                    &browser_session_for(token, "issuer", "subject", 0, 10_000),
                    10,
                    2,
                )
                .await
                .expect("within subject cap");
        }
        assert_eq!(
            store
                .insert_browser_session_with_limits(
                    &browser_session_for("subject-third", "issuer", "subject", 0, 10_000),
                    10,
                    2,
                )
                .await,
            Err(AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity)
        );
        store
            .insert_browser_session_with_limits(
                &browser_session_for("other-issuer", "other-issuer", "subject", 0, 10_000),
                10,
                2,
            )
            .await
            .expect("issuer and subject jointly define the identity cap");
        assert_eq!(
            store
                .insert_browser_session_with_limits(
                    &browser_session_for("global", "other-issuer", "other", 0, 10_000),
                    3,
                    2,
                )
                .await,
            Err(AccessAuthStoreError::BrowserSessionGlobalCapacity)
        );
    }

    #[tokio::test]
    async fn expired_and_revoked_sessions_free_admission_capacity() {
        let (_database, persistence) = sqlite("session-cap-lifecycle.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let expired = browser_session_for("expired-cap", "issuer", "subject", 0, 100);
        store
            .insert_browser_session_with_limits(&expired, 1, 1)
            .await
            .expect("insert expiring session");
        let active = browser_session_for("active-after-expiry", "issuer", "subject", 100, 10_000);
        store
            .insert_browser_session_with_limits(&active, 1, 1)
            .await
            .expect("expired session frees capacity");
        store
            .revoke_browser_session(&active.token_digest, time(200))
            .await
            .expect("revoke active session");
        store
            .insert_browser_session_with_limits(
                &browser_session_for("active-after-revoke", "issuer", "subject", 200, 10_000),
                1,
                1,
            )
            .await
            .expect("revoked session frees capacity");
    }

    #[tokio::test]
    async fn configuration_revocation_frees_subject_and_global_capacity() {
        assert_eq!(MAX_ACTIVE_BROWSER_SESSIONS_PER_SUBJECT, 32);
        assert_eq!(MAX_ACTIVE_BROWSER_SESSIONS, 1_024);
        let (_database, persistence) = sqlite("configuration-capacity.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let wrong_client = browser_session_for_client(
            "wrong-client",
            "issuer",
            "old-client",
            "subject",
            0,
            10_000,
        );
        let wrong_issuer = browser_session_for_client(
            "wrong-issuer",
            "old-issuer",
            "client",
            "other-subject",
            0,
            10_000,
        );
        for session in [&wrong_client, &wrong_issuer] {
            store
                .insert_browser_session_with_limits(session, 2, 1)
                .await
                .unwrap();
        }

        store
            .revoke_browser_sessions_outside_configuration("issuer", "client", time(1_000))
            .await
            .unwrap();
        for session in [&wrong_client, &wrong_issuer] {
            assert!(
                !store
                    .is_browser_session_active(&session.token_digest, time(1_000))
                    .await
                    .unwrap()
            );
        }
        store
            .insert_browser_session_with_limits(
                &browser_session_for_client(
                    "replacement-subject",
                    "issuer",
                    "client",
                    "subject",
                    1_000,
                    10_000,
                ),
                2,
                1,
            )
            .await
            .expect("mismatched client no longer consumes subject capacity");
        store
            .insert_browser_session_with_limits(
                &browser_session_for_client(
                    "replacement-global",
                    "issuer",
                    "client",
                    "other-subject",
                    1_000,
                    10_000,
                ),
                2,
                1,
            )
            .await
            .expect("mismatched configurations no longer consume global capacity");
    }

    #[tokio::test]
    async fn browser_session_client_id_must_not_be_empty() {
        let (_database, persistence) = sqlite("empty-client-id.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let mut session = browser_session("empty-client", 0, 10_000);
        session.client_id.clear();
        assert!(matches!(
            store.insert_browser_session(&session).await,
            Err(AccessAuthStoreError::InvalidRecord(_))
        ));
    }

    #[tokio::test]
    async fn sqlite_browser_session_backend_contract() {
        let (_database, persistence) = sqlite("browser-session-contract.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        assert_browser_session_backend_contract(&store).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_browser_session_admission_cannot_exceed_global_cap() {
        let (_database, persistence) = sqlite("concurrent-session-cap.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let barrier = Arc::new(Barrier::new(3));
        let first = insert_session_after_barrier(
            store.clone(),
            browser_session_for("session-first", "issuer", "first", 0, 10_000),
            1,
            10,
            barrier.clone(),
        );
        let second = insert_session_after_barrier(
            store,
            browser_session_for("session-second", "issuer", "second", 0, 10_000),
            1,
            10,
            barrier.clone(),
        );
        let (first, second, _) = tokio::join!(first, second, barrier.wait());
        let results = [first, second];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(AccessAuthStoreError::BrowserSessionGlobalCapacity)
                ))
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_sqlite_browser_session_admission_cannot_exceed_subject_cap() {
        let (_database, persistence) = sqlite("concurrent-session-subject-cap.sqlite3").await;
        assert_concurrent_subject_admission(&AccessAuthStore::new(&persistence)).await;
    }

    #[tokio::test]
    async fn revocation_is_idempotent() {
        let (_database, persistence) = sqlite("idempotent-revoke.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let session = browser_session("revoke-twice", 0, 10_000);
        store
            .insert_browser_session(&session)
            .await
            .expect("insert session");
        store
            .revoke_browser_session(&session.token_digest, time(500))
            .await
            .expect("first revoke");
        store
            .revoke_browser_session(&session.token_digest, time(900))
            .await
            .expect("second revoke");
        let revoked_at = sqlx::query_scalar::<_, i64>(
            "SELECT revoked_at_ms FROM waltr_browser_sessions WHERE token_digest = ?",
        )
        .bind(&session.token_digest.0)
        .fetch_one(persistence.sqlite_pool().expect("SQLite pool"))
        .await
        .expect("read revocation time");
        assert_eq!(
            revoked_at,
            system_time_to_millis(time(500)).expect("timestamp")
        );
    }

    #[tokio::test]
    async fn cleanup_is_bounded_and_preserves_active_records() {
        let (_database, persistence) = sqlite("cleanup.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let active_attempt = login_attempt("active-attempt", 0, 10_000);
        let expired_attempt = login_attempt("expired-attempt", 0, 100);
        let active_session = browser_session("active-session", 0, 10_000);
        let expired_session = browser_session("expired-session", 0, 100);
        store
            .insert_login_attempt(&active_attempt)
            .await
            .expect("active attempt");
        store
            .insert_login_attempt(&expired_attempt)
            .await
            .expect("expired attempt");
        store
            .insert_browser_session(&active_session)
            .await
            .expect("active session");
        store
            .insert_browser_session(&expired_session)
            .await
            .expect("expired session");

        assert_eq!(
            store.cleanup(time(1_000), 0).await.expect("zero cleanup"),
            CleanupResult::default()
        );
        assert_eq!(
            store
                .cleanup(time(1_000), 1)
                .await
                .expect("bounded cleanup"),
            CleanupResult {
                login_attempts: 1,
                browser_sessions: 1,
            }
        );
        assert!(
            store
                .consume_login_attempt(
                    &active_attempt.state_digest,
                    &active_attempt.correlation_digest,
                    time(1_000),
                )
                .await
                .expect("active attempt lookup")
                .is_some()
        );
        assert!(
            store
                .lookup_browser_session(
                    &active_session.token_digest,
                    &active_session.issuer,
                    &active_session.client_id,
                    time(1_000),
                )
                .await
                .expect("active session lookup")
                .is_some()
        );
    }

    #[tokio::test]
    async fn cleanup_failure_is_advisory_after_a_committed_insert() {
        let (_database, persistence) = sqlite("cleanup-failure.sqlite3").await;
        let store = AccessAuthStore::new(&persistence);
        let expired = login_attempt("expired-cleanup", 0, 100);
        let committed = login_attempt("committed-login", 1_000, 10_000);
        store.insert_login_attempt(&expired).await.unwrap();
        store.insert_login_attempt(&committed).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_auth_cleanup BEFORE DELETE ON waltr_oidc_login_attempts BEGIN SELECT RAISE(ABORT, 'cleanup failed'); END",
        )
        .execute(persistence.sqlite_pool().unwrap())
        .await
        .unwrap();

        assert!(store.cleanup(time(1_000), 1).await.is_err());
        store.cleanup_best_effort(time(1_000), 1).await;
        assert!(
            store
                .consume_login_attempt(
                    &committed.state_digest,
                    &committed.correlation_digest,
                    time(2_000),
                )
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn sqlite_v2_migration_is_idempotent() {
        let database = TestDatabase::new("migration.sqlite3");
        let config = DatabaseConfig::from_url(&database.url()).expect("database config");
        let first = Persistence::initialize(config.clone())
            .await
            .expect("first migration");
        let second = Persistence::initialize(config)
            .await
            .expect("second migration");
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 2 AND description = 'oidc access auth'",
        )
        .fetch_one(second.sqlite_pool().expect("SQLite pool"))
        .await
        .expect("count V2 migration");
        assert_eq!(count, 1);
        let columns = sqlx::query_as::<_, (i64, String, String, i64, Option<String>, i64)>(
            "PRAGMA table_info(waltr_browser_sessions)",
        )
        .fetch_all(second.sqlite_pool().expect("SQLite pool"))
        .await
        .expect("read browser session columns");
        let client_id = columns
            .iter()
            .find(|(_, name, _, _, _, _)| name == "client_id")
            .expect("browser sessions have client_id");
        assert_eq!(client_id.2, "TEXT");
        assert_eq!(client_id.3, 1);
        let pool = second.sqlite_pool().expect("SQLite pool");
        let indexes = sqlx::query_scalar::<_, String>(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name IN ('waltr_oidc_login_attempts_expiry_idx', 'waltr_browser_sessions_expiry_idx') ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .expect("read access-auth expiry indexes");
        assert_eq!(
            indexes,
            [
                "waltr_browser_sessions_expiry_idx",
                "waltr_oidc_login_attempts_expiry_idx"
            ]
        );
        for index in [
            "waltr_oidc_login_attempts_expiry_idx",
            "waltr_browser_sessions_expiry_idx",
        ] {
            let indexed_columns = sqlx::query_scalar::<_, String>(
                "SELECT name FROM pragma_index_info(?) ORDER BY seqno",
            )
            .bind(index)
            .fetch_all(pool)
            .await
            .expect("read SQLite expiry index columns");
            assert_eq!(indexed_columns, ["expires_at_ms"], "{index}");
        }
        sqlx::query(
            "INSERT INTO waltr_browser_sessions (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms) VALUES (?, 'issuer', '', 'subject', 0, 1, 0)",
        )
        .bind("a".repeat(64))
        .execute(pool)
        .await
        .expect_err("SQLite migration must reject an empty client ID");
        sqlx::query(
            "INSERT INTO waltr_browser_sessions (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms) VALUES (?, 'issuer', 'x', 'subject', 0, 1, 0)",
        )
        .bind("e".repeat(64))
        .execute(pool)
        .await
        .expect("SQLite migration must accept a one-character client ID");
        sqlx::query(
            "INSERT INTO waltr_browser_sessions (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms) VALUES (?, 'issuer', 'client', 'subject', 1, 1, 1)",
        )
        .bind("b".repeat(64))
        .execute(pool)
        .await
        .expect_err("SQLite migration must reject non-increasing session expiry");
        sqlx::query(
            "INSERT INTO waltr_oidc_login_attempts (state_digest, correlation_digest, nonce, pkce_verifier, return_path, created_at_ms, expires_at_ms) VALUES (?, ?, 'nonce', 'verifier', '/', 1, 1)",
        )
        .bind("c".repeat(64))
        .bind("d".repeat(64))
        .execute(pool)
        .await
        .expect_err("SQLite migration must reject non-increasing login expiry");
        drop(first);
    }

    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn postgres_store_core_behavior_and_v2_migration() {
        let image = GenericImage::new("postgres", "16-alpine")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_USER", "waltr")
            .with_env_var("POSTGRES_PASSWORD", "waltr")
            .with_env_var("POSTGRES_DB", "waltr");
        let container = image.start().await.expect("start PostgreSQL container");
        let port = container
            .get_host_port_ipv4(5432.tcp())
            .await
            .expect("PostgreSQL port");
        let url = format!("postgres://waltr:waltr@127.0.0.1:{port}/waltr?sslmode=disable");
        let config = DatabaseConfig::from_url(&url).expect("database config");
        let persistence = Persistence::initialize(config.clone())
            .await
            .expect("initialize PostgreSQL");
        Persistence::initialize(config)
            .await
            .expect("idempotent PostgreSQL migration");
        let store = AccessAuthStore::new(&persistence);
        assert_browser_session_backend_contract(&store).await;
        assert_concurrent_subject_admission(&store).await;
        let attempt = login_attempt("postgres-state", 0, 10_000);
        store
            .insert_login_attempt(&attempt)
            .await
            .expect("insert attempt");
        let barrier = Arc::new(Barrier::new(3));
        let first = consume_after_barrier(
            store.clone(),
            attempt.state_digest.clone(),
            attempt.correlation_digest.clone(),
            barrier.clone(),
        );
        let second = consume_after_barrier(
            store.clone(),
            attempt.state_digest,
            attempt.correlation_digest,
            barrier.clone(),
        );
        let (first, second, _) = tokio::join!(first, second, barrier.wait());
        assert_eq!(
            [
                first.expect("first consume"),
                second.expect("second consume")
            ]
            .into_iter()
            .flatten()
            .count(),
            1
        );
        let admission_barrier = Arc::new(Barrier::new(3));
        let first_admission = insert_after_barrier(
            store.clone(),
            login_attempt("postgres-cap-first", 0, 10_000),
            1,
            admission_barrier.clone(),
        );
        let second_admission = insert_after_barrier(
            store.clone(),
            login_attempt("postgres-cap-second", 0, 10_000),
            1,
            admission_barrier.clone(),
        );
        let (first_admission, second_admission, _) =
            tokio::join!(first_admission, second_admission, admission_barrier.wait());
        let admission_results = [first_admission, second_admission];
        assert_eq!(
            admission_results
                .iter()
                .filter(|result| result.is_ok())
                .count(),
            1
        );
        assert_eq!(
            admission_results
                .iter()
                .filter(|result| matches!(result, Err(AccessAuthStoreError::LoginAttemptCapacity)))
                .count(),
            1
        );
        let session = browser_session("postgres-token", 0, 10_000);
        store
            .insert_browser_session(&session)
            .await
            .expect("insert session");
        assert!(
            store
                .lookup_browser_session(
                    &session.token_digest,
                    &session.issuer,
                    &session.client_id,
                    time(1_000),
                )
                .await
                .expect("lookup")
                .is_some()
        );
        store
            .revoke_browser_session(&session.token_digest, time(2_000))
            .await
            .expect("revoke");
        assert!(
            store
                .lookup_browser_session(
                    &session.token_digest,
                    &session.issuer,
                    &session.client_id,
                    time(3_000),
                )
                .await
                .expect("revoked lookup")
                .is_none()
        );
        let session_admission_barrier = Arc::new(Barrier::new(3));
        let first_session = insert_session_after_barrier(
            store.clone(),
            browser_session_for("postgres-session-first", "issuer", "first", 3_000, 10_000),
            1,
            10,
            session_admission_barrier.clone(),
        );
        let second_session = insert_session_after_barrier(
            store.clone(),
            browser_session_for("postgres-session-second", "issuer", "second", 3_000, 10_000),
            1,
            10,
            session_admission_barrier.clone(),
        );
        let (first_session, second_session, _) = tokio::join!(
            first_session,
            second_session,
            session_admission_barrier.wait()
        );
        let session_results = [first_session, second_session];
        assert_eq!(
            session_results
                .iter()
                .filter(|result| result.is_ok())
                .count(),
            1
        );
        assert_eq!(
            session_results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(AccessAuthStoreError::BrowserSessionGlobalCapacity)
                ))
                .count(),
            1
        );
        let versions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM waltr._sqlx_migrations")
            .fetch_one(persistence.postgres_pool().expect("PostgreSQL pool"))
            .await
            .expect("count migrations");
        assert_eq!(versions, 2);
        let column = sqlx::query_as::<_, (String, String)>(
            "SELECT data_type, is_nullable FROM information_schema.columns WHERE table_schema = 'waltr' AND table_name = 'browser_sessions' AND column_name = 'client_id'",
        )
        .fetch_one(persistence.postgres_pool().expect("PostgreSQL pool"))
        .await
        .expect("browser sessions have client_id");
        assert_eq!(column, ("text".to_owned(), "NO".to_owned()));
        let pool = persistence.postgres_pool().expect("PostgreSQL pool");
        let indexes = sqlx::query_scalar::<_, String>(
            "SELECT indexname FROM pg_indexes WHERE schemaname = 'waltr' AND indexname IN ('oidc_login_attempts_expiry_idx', 'browser_sessions_expiry_idx') ORDER BY indexname",
        )
        .fetch_all(pool)
        .await
        .expect("read access-auth expiry indexes");
        assert_eq!(
            indexes,
            [
                "browser_sessions_expiry_idx",
                "oidc_login_attempts_expiry_idx"
            ]
        );
        let index_definitions = sqlx::query_as::<_, (String, String)>(
            "SELECT indexname, indexdef FROM pg_indexes WHERE schemaname = 'waltr' AND indexname IN ('oidc_login_attempts_expiry_idx', 'browser_sessions_expiry_idx') ORDER BY indexname",
        )
        .fetch_all(pool)
        .await
        .expect("read PostgreSQL expiry index definitions");
        assert!(
            index_definitions
                .iter()
                .all(|(_, definition)| definition.strip_suffix(" (expires_at_ms)").is_some())
        );
        sqlx::query(
            "INSERT INTO waltr.browser_sessions (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms) VALUES ($1, 'issuer', '', 'subject', 0, 1, 0)",
        )
        .bind("a".repeat(64))
        .execute(pool)
        .await
        .expect_err("PostgreSQL migration must reject an empty client ID");
        sqlx::query(
            "INSERT INTO waltr.browser_sessions (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms) VALUES ($1, 'issuer', 'x', 'subject', 0, 1, 0)",
        )
        .bind("e".repeat(64))
        .execute(pool)
        .await
        .expect("PostgreSQL migration must accept a one-character client ID");
        sqlx::query(
            "INSERT INTO waltr.browser_sessions (token_digest, issuer, client_id, subject, created_at_ms, expires_at_ms, last_seen_at_ms) VALUES ($1, 'issuer', 'client', 'subject', 1, 1, 1)",
        )
        .bind("b".repeat(64))
        .execute(pool)
        .await
        .expect_err("PostgreSQL migration must reject non-increasing session expiry");
        sqlx::query(
            "INSERT INTO waltr.oidc_login_attempts (state_digest, correlation_digest, nonce, pkce_verifier, return_path, created_at_ms, expires_at_ms) VALUES ($1, $2, 'nonce', 'verifier', '/', 1, 1)",
        )
        .bind("c".repeat(64))
        .bind("d".repeat(64))
        .execute(pool)
        .await
        .expect_err("PostgreSQL migration must reject non-increasing login expiry");

        let mismatch = browser_session_for_client(
            "postgres-mismatch",
            "issuer",
            "old-client",
            "subject",
            3_000,
            10_000,
        );
        store.insert_browser_session(&mismatch).await.unwrap();
        assert!(
            store
                .lookup_browser_session(
                    &mismatch.token_digest,
                    &mismatch.issuer,
                    "client",
                    time(4_000),
                )
                .await
                .unwrap()
                .is_none()
        );
        store
            .revoke_browser_sessions_outside_configuration("issuer", "client", time(4_000))
            .await
            .unwrap();
        assert!(
            !store
                .is_browser_session_active(&mismatch.token_digest, time(4_000))
                .await
                .unwrap()
        );
    }

    async fn consume_after_barrier(
        store: AccessAuthStore,
        digest: LoginStateDigest,
        correlation_digest: LoginCorrelationDigest,
        barrier: Arc<Barrier>,
    ) -> Result<Option<ConsumedLoginAttempt>, AccessAuthStoreError> {
        barrier.wait().await;
        store
            .consume_login_attempt(&digest, &correlation_digest, time(1_000))
            .await
    }

    async fn insert_after_barrier(
        store: AccessAuthStore,
        attempt: LoginAttempt,
        active_limit: u32,
        barrier: Arc<Barrier>,
    ) -> Result<(), AccessAuthStoreError> {
        barrier.wait().await;
        store
            .insert_login_attempt_with_limit(&attempt, active_limit)
            .await
    }

    async fn insert_session_after_barrier(
        store: AccessAuthStore,
        session: BrowserSession,
        global_limit: u32,
        subject_limit: u32,
        barrier: Arc<Barrier>,
    ) -> Result<(), AccessAuthStoreError> {
        barrier.wait().await;
        store
            .insert_browser_session_with_limits(&session, global_limit, subject_limit)
            .await
    }

    async fn assert_browser_session_backend_contract(store: &AccessAuthStore) {
        let mut empty_client = browser_session("contract-empty-client", 0, 10_000);
        empty_client.client_id.clear();
        assert!(matches!(
            store.insert_browser_session(&empty_client).await,
            Err(AccessAuthStoreError::InvalidRecord(_))
        ));

        let active = browser_session_for_client(
            "contract-active",
            "contract-issuer",
            "contract-client",
            "contract-subject",
            0,
            10_000,
        );
        store.insert_browser_session(&active).await.unwrap();
        let last_seen = persisted_last_seen(store, &active.token_digest).await;
        assert!(
            store
                .is_browser_session_active(&active.token_digest, time(1_000))
                .await
                .unwrap()
        );
        assert_eq!(
            persisted_last_seen(store, &active.token_digest).await,
            last_seen
        );
        assert!(
            store
                .lookup_browser_session(
                    &active.token_digest,
                    "contract-wrong-issuer",
                    &active.client_id,
                    time(1_000),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .lookup_browser_session(
                    &active.token_digest,
                    &active.issuer,
                    "wrong-client",
                    time(1_000),
                )
                .await
                .unwrap()
                .is_none()
        );
        let found = store
            .lookup_browser_session(
                &active.token_digest,
                &active.issuer,
                &active.client_id,
                time(1_000),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.client_id, active.client_id);
        assert_eq!(found.subject, active.subject);
        store
            .revoke_browser_session(&active.token_digest, time(1_000))
            .await
            .unwrap();

        for token in ["contract-subject-first", "contract-subject-second"] {
            store
                .insert_browser_session_with_limits(
                    &browser_session_for_client(
                        token,
                        "contract-cap-issuer",
                        "contract-cap-client",
                        "contract-cap-subject",
                        0,
                        10_000,
                    ),
                    10,
                    2,
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .insert_browser_session_with_limits(
                    &browser_session_for_client(
                        "contract-subject-third",
                        "contract-cap-issuer",
                        "contract-cap-client",
                        "contract-cap-subject",
                        0,
                        10_000,
                    ),
                    10,
                    2,
                )
                .await,
            Err(AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity)
        );
        store.cleanup(time(20_000), 100).await.unwrap();

        let wrong_client = browser_session_for_client(
            "contract-wrong-client",
            "contract-current-issuer",
            "contract-old-client",
            "contract-current-subject",
            0,
            10_000,
        );
        let wrong_issuer = browser_session_for_client(
            "contract-wrong-issuer",
            "contract-old-issuer",
            "contract-current-client",
            "contract-other-subject",
            0,
            10_000,
        );
        for session in [&wrong_client, &wrong_issuer] {
            store
                .insert_browser_session_with_limits(session, 2, 1)
                .await
                .unwrap();
        }
        store
            .revoke_browser_sessions_outside_configuration(
                "contract-current-issuer",
                "contract-current-client",
                time(1_000),
            )
            .await
            .unwrap();
        for session in [&wrong_client, &wrong_issuer] {
            assert!(
                !store
                    .is_browser_session_active(&session.token_digest, time(1_000))
                    .await
                    .unwrap()
            );
        }
        store
            .insert_browser_session_with_limits(
                &browser_session_for_client(
                    "contract-replacement-subject",
                    "contract-current-issuer",
                    "contract-current-client",
                    "contract-current-subject",
                    1_000,
                    10_000,
                ),
                2,
                1,
            )
            .await
            .expect("configuration revocation frees subject capacity");
        store
            .insert_browser_session_with_limits(
                &browser_session_for_client(
                    "contract-replacement-global",
                    "contract-current-issuer",
                    "contract-current-client",
                    "contract-other-subject",
                    1_000,
                    10_000,
                ),
                2,
                1,
            )
            .await
            .expect("configuration revocation frees global capacity");
        store.cleanup(time(20_000), 100).await.unwrap();
    }

    async fn assert_concurrent_subject_admission(store: &AccessAuthStore) {
        let barrier = Arc::new(Barrier::new(3));
        let first = insert_session_after_barrier(
            store.clone(),
            browser_session_for(
                "concurrent-subject-first",
                "subject-issuer",
                "subject",
                0,
                10_000,
            ),
            10,
            1,
            barrier.clone(),
        );
        let second = insert_session_after_barrier(
            store.clone(),
            browser_session_for(
                "concurrent-subject-second",
                "subject-issuer",
                "subject",
                0,
                10_000,
            ),
            10,
            1,
            barrier.clone(),
        );
        let (first, second, _) = tokio::join!(first, second, barrier.wait());
        let results = [first, second];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity)
                ))
                .count(),
            1
        );
        store.cleanup(time(20_000), 100).await.unwrap();
    }

    async fn persisted_last_seen(
        store: &AccessAuthStore,
        token_digest: &BrowserSessionTokenDigest,
    ) -> i64 {
        match &store.pool {
            AccessAuthPool::Sqlite(pool) => sqlx::query_scalar(
                "SELECT last_seen_at_ms FROM waltr_browser_sessions WHERE token_digest = ?",
            )
            .bind(&token_digest.0)
            .fetch_one(pool)
            .await
            .unwrap(),
            AccessAuthPool::PostgreSql(pool) => sqlx::query_scalar(
                "SELECT last_seen_at_ms FROM waltr.browser_sessions WHERE token_digest = $1",
            )
            .bind(&token_digest.0)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    async fn sqlite(name: &str) -> (TestDatabase, Persistence) {
        let database = TestDatabase::new(name);
        let persistence = Persistence::initialize(
            DatabaseConfig::from_url(&database.url()).expect("database config"),
        )
        .await
        .expect("initialize SQLite");
        (database, persistence)
    }

    fn login_attempt(state: &str, created_offset: u64, expires_offset: u64) -> LoginAttempt {
        login_attempt_with_correlation(
            state,
            &format!("correlation-{state}"),
            created_offset,
            expires_offset,
        )
    }

    fn login_attempt_with_correlation(
        state: &str,
        correlation: &str,
        created_offset: u64,
        expires_offset: u64,
    ) -> LoginAttempt {
        LoginAttempt {
            state_digest: LoginStateDigest::from_state(state),
            correlation_digest: LoginCorrelationDigest::from_correlation(correlation),
            nonce: "nonce-123".to_owned(),
            pkce_verifier: "pkce-verifier-123".to_owned(),
            return_path: "/sessions/current".to_owned(),
            created_at: time(created_offset),
            expires_at: time(expires_offset),
        }
    }

    fn browser_session(token: &str, created_offset: u64, expires_offset: u64) -> BrowserSession {
        browser_session_for(
            token,
            "https://issuer.example",
            "subject-123",
            created_offset,
            expires_offset,
        )
    }

    fn browser_session_for(
        token: &str,
        issuer: &str,
        subject: &str,
        created_offset: u64,
        expires_offset: u64,
    ) -> BrowserSession {
        browser_session_for_client(
            token,
            issuer,
            "waltr-test-client",
            subject,
            created_offset,
            expires_offset,
        )
    }

    fn browser_session_for_client(
        token: &str,
        issuer: &str,
        client_id: &str,
        subject: &str,
        created_offset: u64,
        expires_offset: u64,
    ) -> BrowserSession {
        BrowserSession {
            token_digest: BrowserSessionTokenDigest::from_token(token),
            issuer: issuer.to_owned(),
            client_id: client_id.to_owned(),
            subject: subject.to_owned(),
            created_at: time(created_offset),
            expires_at: time(expires_offset),
            last_seen_at: time(created_offset),
        }
    }

    fn time(offset_millis: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(BASE_MILLIS + offset_millis)
    }

    #[test]
    fn timestamps_use_checked_signed_milliseconds() {
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_millis(1);
        assert_eq!(system_time_to_millis(before_epoch), Ok(-1));
        assert_eq!(millis_to_system_time(-1), Ok(before_epoch));
    }

    struct TestDatabase {
        root: PathBuf,
        filename: String,
    }

    impl TestDatabase {
        fn new(filename: &str) -> Self {
            Self {
                root: env::temp_dir().join(format!("waltr-access-auth-test-{}", Uuid::new_v4())),
                filename: filename.to_owned(),
            }
        }

        fn url(&self) -> String {
            format!("sqlite://{}", self.root.join(&self.filename).display())
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            match fs::remove_dir_all(&self.root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove temporary database directory: {error}"),
            }
        }
    }
}
