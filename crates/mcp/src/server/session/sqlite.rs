use std::{fmt, str::FromStr as _, time::SystemTime};

use sqlx::{
    Connection as _, Row as _, Sqlite, SqliteConnection, SqlitePool, Transaction,
    pool::PoolConnection,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use super::{
    BoxFuture, Duration, StreamableHttpAbandonInitialization,
    StreamableHttpAbandonInitializationRequest, StreamableHttpAppendEvents,
    StreamableHttpAppendEventsRequest, StreamableHttpClaimInitialization,
    StreamableHttpCleanupExpired, StreamableHttpCompleteInitialization,
    StreamableHttpCompleteInitializationRequest, StreamableHttpCreateSession,
    StreamableHttpCreateSessionRequest, StreamableHttpDeleteSession, StreamableHttpEvent,
    StreamableHttpEventId, StreamableHttpInspectSession, StreamableHttpPrepareDelivery,
    StreamableHttpPrepareDeliveryRequest, StreamableHttpSession, StreamableHttpSessionLifecycle,
    StreamableHttpSessionOwner, StreamableHttpSessionRequest, StreamableHttpSessionStore,
    StreamableHttpSessionStoreConfig, StreamableHttpSessionStoreError,
    StreamableHttpSessionStoreResult, duration_ms, fresh_id, migration_error, persisted_event,
    serialize_messages, system_time_from_ms, system_time_ms,
};

#[cfg(test)]
use super::StreamableHttpSessionKey;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/sqlite");

#[cfg(test)]
#[derive(Clone)]
struct DeliveryTestBarrier {
    initial_read: std::sync::Arc<tokio::sync::Semaphore>,
    resume: std::sync::Arc<tokio::sync::Semaphore>,
}

#[cfg(test)]
impl DeliveryTestBarrier {
    fn new() -> Self {
        Self {
            initial_read: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
            resume: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }

    async fn wait_for_initial_read(&self) {
        self.initial_read
            .acquire()
            .await
            .expect("test barrier remains open")
            .forget();
    }

    fn resume_delivery(&self) {
        self.resume.add_permits(1);
    }

    async fn pause_after_initial_read(&self) {
        self.initial_read.add_permits(1);
        self.resume
            .acquire()
            .await
            .expect("test barrier remains open")
            .forget();
    }
}

/// SQLite-backed persistence for MCP Streamable HTTP sessions.
#[derive(Clone)]
pub struct SqliteStreamableHttpSessionStore {
    pool: SqlitePool,
    config: StreamableHttpSessionStoreConfig,
    #[cfg(test)]
    delivery_test_barrier: Option<DeliveryTestBarrier>,
}

impl fmt::Debug for SqliteStreamableHttpSessionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqliteStreamableHttpSessionStore")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SqliteStreamableHttpSessionStore {
    /// Connect, migrate, and use default store policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL is invalid or connection or migration fails.
    pub async fn connect(database_url: &str) -> StreamableHttpSessionStoreResult<Self> {
        Self::connect_with_config(database_url, StreamableHttpSessionStoreConfig::default()).await
    }

    /// Connect, migrate, and use explicit store policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the URL or policy is invalid or connection or migration fails.
    pub async fn connect_with_config(
        database_url: &str,
        config: StreamableHttpSessionStoreConfig,
    ) -> StreamableHttpSessionStoreResult<Self> {
        let in_memory = database_url_uses_memory(database_url);
        let options = SqliteConnectOptions::from_str(database_url)
            .map_err(|_| StreamableHttpSessionStoreError::Unavailable)?
            .create_if_missing(true)
            .foreign_keys(true);
        let mut pool_options = SqlitePoolOptions::new();
        if in_memory {
            pool_options = pool_options
                .max_connections(1)
                .min_connections(1)
                .max_lifetime(None)
                .idle_timeout(None);
        }
        let pool = pool_options
            .connect_with(options)
            .await
            .map_err(unavailable)?;
        Self::from_pool_with_config(pool, config).await
    }

    /// Migrate an existing pool and use default store policy.
    ///
    /// # Errors
    ///
    /// Returns an error when migration or store initialization fails.
    pub async fn from_pool(pool: SqlitePool) -> StreamableHttpSessionStoreResult<Self> {
        Self::from_pool_with_config(pool, StreamableHttpSessionStoreConfig::default()).await
    }

    /// Migrate an existing pool and use explicit store policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the policy, migration, or store initialization fails.
    pub async fn from_pool_with_config(
        pool: SqlitePool,
        config: StreamableHttpSessionStoreConfig,
    ) -> StreamableHttpSessionStoreResult<Self> {
        config.validate()?;
        validate_pool(&pool).await?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|error| migration_error(&error))?;
        Ok(Self {
            pool,
            config,
            #[cfg(test)]
            delivery_test_barrier: None,
        })
    }

    #[cfg(test)]
    fn with_delivery_test_barrier(mut self, barrier: DeliveryTestBarrier) -> Self {
        self.delivery_test_barrier = Some(barrier);
        self
    }
}

impl StreamableHttpSessionStore for SqliteStreamableHttpSessionStore {
    fn config(&self) -> StreamableHttpSessionStoreConfig {
        self.config.clone()
    }

    #[allow(clippy::too_many_lines)]
    fn create_session(
        &self,
        request: StreamableHttpCreateSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCreateSession>> {
        let pool = self.pool.clone();
        let config = self.config.clone();
        Box::pin(async move {
            let now = system_time_ms(request.now)?;
            let expires = now
                .checked_add(duration_ms(config.session_ttl)?)
                .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
            let owner_key = request.key.owner.storage_key();
            let mut connection = acquire_connection(&pool).await?;
            let mut transaction = immediate_transaction(&mut connection).await?;
            cleanup_expired_tx(&mut transaction, now, config.cleanup_batch_size).await?;
            if sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM mcp_streamable_http_sessions WHERE session_id = ?",
            )
            .bind(&request.key.session_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(unavailable)?
                != 0
            {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCreateSession::AlreadyExists);
            }
            let global = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM mcp_streamable_http_sessions WHERE expires_at_ms > ?",
            )
            .bind(now)
            .fetch_one(&mut *transaction)
            .await
            .map_err(unavailable)?;
            if usize::try_from(global).unwrap_or(usize::MAX) >= config.max_sessions {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCreateSession::CapacityExceeded);
            }
            if matches!(
                request.key.owner,
                StreamableHttpSessionOwner::Authenticated(_)
            ) {
                let owner_count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM mcp_streamable_http_sessions WHERE owner_key = ? AND expires_at_ms > ?",
                )
                .bind(&owner_key)
                .bind(now)
                .fetch_one(&mut *transaction)
                .await
                .map_err(unavailable)?;
                if usize::try_from(owner_count).unwrap_or(usize::MAX)
                    >= config.max_sessions_per_principal
                {
                    transaction.commit().await.map_err(unavailable)?;
                    return Ok(StreamableHttpCreateSession::CapacityExceeded);
                }
            }
            let operation_id = fresh_id();
            let stream_id = fresh_id();
            sqlx::query(
                r"
INSERT INTO mcp_streamable_http_sessions (
    session_id, owner_key, protocol_version, lifecycle,
    initialization_operation_id, event_stream_id,
    created_at_ms, updated_at_ms, expires_at_ms
) VALUES (?, ?, ?, 'initialize_responded', ?, ?, ?, ?, ?)
                ",
            )
            .bind(&request.key.session_id)
            .bind(owner_key)
            .bind(&request.protocol_version)
            .bind(&operation_id)
            .bind(stream_id)
            .bind(now)
            .bind(now)
            .bind(expires)
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(StreamableHttpCreateSession::Created(
                StreamableHttpSession {
                    lifecycle: StreamableHttpSessionLifecycle::InitializeResponded,
                    protocol_version: request.protocol_version,
                    initialization_operation_id: operation_id,
                    expires_at: system_time_from_ms(expires)?,
                    pending_event_count: 0,
                    pending_event_bytes: 0,
                },
            ))
        })
    }

    fn inspect_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpInspectSession>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let now = system_time_ms(request.now)?;
            let mut connection = acquire_connection(&pool).await?;
            let Some(row) = load_session(&mut *connection, &request, now).await? else {
                return Ok(StreamableHttpInspectSession::NotFound);
            };
            if !row.version_matches(&request) {
                return Ok(StreamableHttpInspectSession::ProtocolVersionMismatch);
            }
            Ok(StreamableHttpInspectSession::Session(row.public()?))
        })
    }

    fn claim_initialization(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpClaimInitialization>> {
        let pool = self.pool.clone();
        let lease = self.config.initialization_claim_lease;
        Box::pin(async move {
            let now = system_time_ms(request.now)?;
            let mut connection = acquire_connection(&pool).await?;
            let mut transaction = immediate_transaction(&mut connection).await?;
            let Some(row) = load_session(&mut *transaction, &request, now).await? else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpClaimInitialization::NotFound);
            };
            if !row.version_matches(&request) {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpClaimInitialization::ProtocolVersionMismatch);
            }
            if row.lifecycle == StreamableHttpSessionLifecycle::Initializing
                && row.claim_expires_at_ms.is_some_and(|expires| expires > now)
            {
                let retry = Duration::from_millis(
                    u64::try_from(row.claim_expires_at_ms.unwrap_or(now) - now).unwrap_or(0),
                );
                transaction.commit().await.map_err(unavailable)?;
                return Ok(
                    StreamableHttpClaimInitialization::InitializationInProgress {
                        retry_after: retry,
                    },
                );
            }
            if !matches!(
                row.lifecycle,
                StreamableHttpSessionLifecycle::InitializeResponded
                    | StreamableHttpSessionLifecycle::Initializing
            ) {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpClaimInitialization::LifecycleMismatch);
            }
            let claim_id = fresh_id();
            let claim_expires = now
                .checked_add(duration_ms(lease)?)
                .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
            sqlx::query(
                r"
UPDATE mcp_streamable_http_sessions
SET lifecycle = 'initializing', initialization_claim_id = ?,
    initialization_claim_expires_at_ms = ?, updated_at_ms = ?
WHERE session_id = ?
                ",
            )
            .bind(&claim_id)
            .bind(claim_expires)
            .bind(now)
            .bind(&request.key.session_id)
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(StreamableHttpClaimInitialization::Claimed {
                claim_id,
                operation_id: row.operation_id,
            })
        })
    }

    fn complete_initialization(
        &self,
        request: StreamableHttpCompleteInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCompleteInitialization>> {
        let pool = self.pool.clone();
        let config = self.config.clone();
        Box::pin(async move {
            let messages = serialize_messages(request.messages, &config)?;
            let now = system_time_ms(request.session.now)?;
            let mut connection = acquire_connection(&pool).await?;
            let mut transaction = immediate_transaction(&mut connection).await?;
            let Some(row) = load_session(&mut *transaction, &request.session, now).await? else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCompleteInitialization::NotFound);
            };
            if !row.version_matches(&request.session) {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCompleteInitialization::ProtocolVersionMismatch);
            }
            if row.lifecycle == StreamableHttpSessionLifecycle::Initialized
                && row.completed_claim_id.as_deref() == Some(&request.claim_id)
            {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCompleteInitialization::AlreadyCompleted);
            }
            if row.lifecycle != StreamableHttpSessionLifecycle::Initializing
                || row.claim_id.as_deref() != Some(&request.claim_id)
                || row.claim_expires_at_ms.is_none_or(|expires| expires <= now)
            {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCompleteInitialization::LifecycleMismatch);
            }
            let Some((events, next, count, bytes)) = append_events_tx(
                &mut transaction,
                &request.session.key.session_id,
                &row,
                messages,
                now,
                &config,
            )
            .await?
            else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpCompleteInitialization::CapacityExceeded);
            };
            sqlx::query(
                r"
UPDATE mcp_streamable_http_sessions
SET lifecycle = 'initialized', initialization_claim_id = NULL,
    initialization_claim_expires_at_ms = NULL,
    completed_initialization_claim_id = ?, next_event_sequence = ?,
    pending_event_count = ?, pending_event_bytes = ?, updated_at_ms = ?
WHERE session_id = ?
                ",
            )
            .bind(&request.claim_id)
            .bind(next)
            .bind(count)
            .bind(bytes)
            .bind(now)
            .bind(&request.session.key.session_id)
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(StreamableHttpCompleteInitialization::Completed(events))
        })
    }

    fn abandon_initialization(
        &self,
        request: StreamableHttpAbandonInitializationRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAbandonInitialization>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let now = system_time_ms(request.session.now)?;
            let mut connection = acquire_connection(&pool).await?;
            let mut transaction = immediate_transaction(&mut connection).await?;
            let Some(row) = load_session(&mut *transaction, &request.session, now).await? else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAbandonInitialization::NotFound);
            };
            if !row.version_matches(&request.session) {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAbandonInitialization::ProtocolVersionMismatch);
            }
            if row.lifecycle != StreamableHttpSessionLifecycle::Initializing
                || row.claim_id.as_deref() != Some(&request.claim_id)
            {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAbandonInitialization::LifecycleMismatch);
            }
            sqlx::query(
                r"
UPDATE mcp_streamable_http_sessions
SET lifecycle = 'initialize_responded', initialization_claim_id = NULL,
    initialization_claim_expires_at_ms = NULL, updated_at_ms = ?
WHERE session_id = ?
                ",
            )
            .bind(now)
            .bind(&request.session.key.session_id)
            .execute(&mut *transaction)
            .await
            .map_err(unavailable)?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(StreamableHttpAbandonInitialization::Abandoned)
        })
    }

    fn append_events(
        &self,
        request: StreamableHttpAppendEventsRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpAppendEvents>> {
        let pool = self.pool.clone();
        let config = self.config.clone();
        Box::pin(async move {
            let messages = serialize_messages(request.messages, &config)?;
            let now = system_time_ms(request.session.now)?;
            let mut connection = acquire_connection(&pool).await?;
            if messages.is_empty() {
                let Some(row) = load_session(&mut *connection, &request.session, now).await? else {
                    return Ok(StreamableHttpAppendEvents::NotFound);
                };
                if !row.version_matches(&request.session) {
                    return Ok(StreamableHttpAppendEvents::ProtocolVersionMismatch);
                }
                return Ok(
                    if row.lifecycle == StreamableHttpSessionLifecycle::Initialized {
                        StreamableHttpAppendEvents::Events(Vec::new())
                    } else {
                        StreamableHttpAppendEvents::LifecycleMismatch
                    },
                );
            }
            let mut transaction = immediate_transaction(&mut connection).await?;
            let Some(row) = load_session(&mut *transaction, &request.session, now).await? else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAppendEvents::NotFound);
            };
            if !row.version_matches(&request.session) {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAppendEvents::ProtocolVersionMismatch);
            }
            if row.lifecycle != StreamableHttpSessionLifecycle::Initialized {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAppendEvents::LifecycleMismatch);
            }
            let Some((events, next, count, bytes)) = append_events_tx(
                &mut transaction,
                &request.session.key.session_id,
                &row,
                messages,
                now,
                &config,
            )
            .await?
            else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpAppendEvents::CapacityExceeded);
            };
            if !events.is_empty() {
                sqlx::query(
                    r"
UPDATE mcp_streamable_http_sessions
SET next_event_sequence = ?, pending_event_count = ?,
    pending_event_bytes = ?, updated_at_ms = ?
WHERE session_id = ?
                    ",
                )
                .bind(next)
                .bind(count)
                .bind(bytes)
                .bind(now)
                .bind(&request.session.key.session_id)
                .execute(&mut *transaction)
                .await
                .map_err(unavailable)?;
            }
            transaction.commit().await.map_err(unavailable)?;
            Ok(StreamableHttpAppendEvents::Events(events))
        })
    }

    fn prepare_delivery(
        &self,
        request: StreamableHttpPrepareDeliveryRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery>> {
        let pool = self.pool.clone();
        let config = self.config.clone();
        #[cfg(test)]
        let delivery_test_barrier = self.delivery_test_barrier.clone();
        Box::pin(async move {
            let now = system_time_ms(request.session.now)?;
            let mut connection = acquire_connection(&pool).await?;
            let Some(initial) = load_session(&mut *connection, &request.session, now).await? else {
                return Ok(StreamableHttpPrepareDelivery::NotFound);
            };
            if !initial.version_matches(&request.session) {
                return Ok(StreamableHttpPrepareDelivery::ProtocolVersionMismatch);
            }
            if initial.lifecycle != StreamableHttpSessionLifecycle::Initialized {
                return Ok(StreamableHttpPrepareDelivery::LifecycleMismatch);
            }
            if request.acknowledged_event_id.is_none()
                && initial.delivery_id.is_none()
                && initial.next_sequence - 1 <= initial.delivered_sequence
            {
                return Ok(StreamableHttpPrepareDelivery::Idle {
                    priming_event_id: StreamableHttpEventId {
                        stream_id: initial.stream_id.clone(),
                        sequence: 0,
                    },
                });
            }
            #[cfg(test)]
            if let Some(barrier) = delivery_test_barrier {
                barrier.pause_after_initial_read().await;
            }
            let mut transaction = immediate_transaction(&mut connection).await?;
            let Some(row) = load_session(&mut *transaction, &request.session, now).await? else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpPrepareDelivery::NotFound);
            };
            let outcome = prepare_delivery_tx(&mut transaction, row, request, now, &config).await?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(outcome)
        })
    }

    fn delete_session(
        &self,
        request: StreamableHttpSessionRequest,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpDeleteSession>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let now = system_time_ms(request.now)?;
            let mut connection = acquire_connection(&pool).await?;
            let mut transaction = immediate_transaction(&mut connection).await?;
            let Some(row) = load_session(&mut *transaction, &request, now).await? else {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpDeleteSession::NotFound);
            };
            if !row.version_matches(&request) {
                transaction.commit().await.map_err(unavailable)?;
                return Ok(StreamableHttpDeleteSession::ProtocolVersionMismatch);
            }
            sqlx::query("DELETE FROM mcp_streamable_http_sessions WHERE session_id = ?")
                .bind(request.key.session_id)
                .execute(&mut *transaction)
                .await
                .map_err(unavailable)?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(StreamableHttpDeleteSession::Deleted)
        })
    }

    fn cleanup_expired(
        &self,
        now: SystemTime,
        limit: usize,
    ) -> BoxFuture<StreamableHttpSessionStoreResult<StreamableHttpCleanupExpired>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let now = system_time_ms(now)?;
            let mut connection = acquire_connection(&pool).await?;
            let mut transaction = immediate_transaction(&mut connection).await?;
            let outcome = cleanup_expired_tx(&mut transaction, now, limit).await?;
            transaction.commit().await.map_err(unavailable)?;
            Ok(outcome)
        })
    }
}

#[derive(Debug)]
struct SessionRow {
    protocol_version: String,
    lifecycle: StreamableHttpSessionLifecycle,
    operation_id: String,
    claim_id: Option<String>,
    claim_expires_at_ms: Option<i64>,
    completed_claim_id: Option<String>,
    stream_id: String,
    next_sequence: i64,
    delivered_sequence: i64,
    acknowledged_sequence: i64,
    pending_count: i64,
    pending_bytes: i64,
    delivery_id: Option<String>,
    delivery_from: Option<i64>,
    delivery_through: Option<i64>,
    delivery_expires_at_ms: Option<i64>,
    expires_at_ms: i64,
}

impl SessionRow {
    fn version_matches(&self, request: &StreamableHttpSessionRequest) -> bool {
        request
            .presented_protocol_version
            .as_ref()
            .is_none_or(|version| *version == self.protocol_version)
    }

    fn public(&self) -> StreamableHttpSessionStoreResult<StreamableHttpSession> {
        Ok(StreamableHttpSession {
            lifecycle: self.lifecycle,
            protocol_version: self.protocol_version.clone(),
            initialization_operation_id: self.operation_id.clone(),
            expires_at: system_time_from_ms(self.expires_at_ms)?,
            pending_event_count: usize::try_from(self.pending_count)
                .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?,
            pending_event_bytes: usize::try_from(self.pending_bytes)
                .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?,
        })
    }
}

async fn load_session<'e, E>(
    executor: E,
    request: &StreamableHttpSessionRequest,
    now: i64,
) -> StreamableHttpSessionStoreResult<Option<SessionRow>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(
        r"
SELECT protocol_version, lifecycle, initialization_operation_id,
    initialization_claim_id, initialization_claim_expires_at_ms,
    completed_initialization_claim_id, event_stream_id, next_event_sequence,
    delivered_event_sequence, acknowledged_event_sequence, pending_event_count,
    pending_event_bytes, delivery_id, delivery_from_sequence,
    delivery_through_sequence, delivery_lease_expires_at_ms, expires_at_ms
FROM mcp_streamable_http_sessions
WHERE session_id = ? AND owner_key = ? AND expires_at_ms > ?
        ",
    )
    .bind(&request.key.session_id)
    .bind(request.key.owner.storage_key())
    .bind(now)
    .fetch_optional(executor)
    .await
    .map_err(unavailable)?;
    row.as_ref().map(parse_session_row).transpose()
}

fn parse_session_row(
    row: &sqlx::sqlite::SqliteRow,
) -> StreamableHttpSessionStoreResult<SessionRow> {
    let lifecycle = row.try_get::<String, _>("lifecycle").map_err(invalid)?;
    let parsed = SessionRow {
        protocol_version: row.try_get("protocol_version").map_err(invalid)?,
        lifecycle: StreamableHttpSessionLifecycle::from_storage_name(&lifecycle)?,
        operation_id: row
            .try_get("initialization_operation_id")
            .map_err(invalid)?,
        claim_id: row.try_get("initialization_claim_id").map_err(invalid)?,
        claim_expires_at_ms: row
            .try_get("initialization_claim_expires_at_ms")
            .map_err(invalid)?,
        completed_claim_id: row
            .try_get("completed_initialization_claim_id")
            .map_err(invalid)?,
        stream_id: row.try_get("event_stream_id").map_err(invalid)?,
        next_sequence: row.try_get("next_event_sequence").map_err(invalid)?,
        delivered_sequence: row.try_get("delivered_event_sequence").map_err(invalid)?,
        acknowledged_sequence: row
            .try_get("acknowledged_event_sequence")
            .map_err(invalid)?,
        pending_count: row.try_get("pending_event_count").map_err(invalid)?,
        pending_bytes: row.try_get("pending_event_bytes").map_err(invalid)?,
        delivery_id: row.try_get("delivery_id").map_err(invalid)?,
        delivery_from: row.try_get("delivery_from_sequence").map_err(invalid)?,
        delivery_through: row.try_get("delivery_through_sequence").map_err(invalid)?,
        delivery_expires_at_ms: row
            .try_get("delivery_lease_expires_at_ms")
            .map_err(invalid)?,
        expires_at_ms: row.try_get("expires_at_ms").map_err(invalid)?,
    };
    validate_row(&parsed)?;
    Ok(parsed)
}

fn validate_row(row: &SessionRow) -> StreamableHttpSessionStoreResult<()> {
    let claim_pair = row.claim_id.is_some() == row.claim_expires_at_ms.is_some();
    let delivery_quad = [
        row.delivery_id.is_some(),
        row.delivery_from.is_some(),
        row.delivery_through.is_some(),
        row.delivery_expires_at_ms.is_some(),
    ];
    let delivery_valid =
        delivery_quad.iter().all(|value| *value) || delivery_quad.iter().all(|value| !*value);
    if row.protocol_version.is_empty()
        || row.operation_id.is_empty()
        || row.stream_id.is_empty()
        || row.next_sequence <= 0
        || row.acknowledged_sequence < 0
        || row.acknowledged_sequence > row.delivered_sequence
        || row.delivered_sequence >= row.next_sequence
        || row.pending_count < 0
        || row.pending_bytes < 0
        || !claim_pair
        || !delivery_valid
        || (row.lifecycle == StreamableHttpSessionLifecycle::Initializing && row.claim_id.is_none())
    {
        return Err(StreamableHttpSessionStoreError::InvalidData);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn append_events_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    session_id: &str,
    row: &SessionRow,
    messages: Vec<(serde_json::Value, String, usize)>,
    now: i64,
    config: &StreamableHttpSessionStoreConfig,
) -> StreamableHttpSessionStoreResult<Option<(Vec<StreamableHttpEvent>, i64, i64, i64)>> {
    let added_count =
        i64::try_from(messages.len()).map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    let added_bytes = messages.iter().try_fold(0_i64, |total, (_, _, bytes)| {
        total.checked_add(i64::try_from(*bytes).ok()?)
    });
    let Some(added_bytes) = added_bytes else {
        return Err(StreamableHttpSessionStoreError::InvalidData);
    };
    let next = row
        .next_sequence
        .checked_add(added_count)
        .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
    let count = row
        .pending_count
        .checked_add(added_count)
        .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
    let bytes = row
        .pending_bytes
        .checked_add(added_bytes)
        .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
    if usize::try_from(count).unwrap_or(usize::MAX) > config.max_events_per_session
        || usize::try_from(bytes).unwrap_or(usize::MAX) > config.max_pending_bytes_per_session
        || next <= 0
    {
        return Ok(None);
    }
    let mut events = Vec::with_capacity(messages.len());
    for (sequence, (message, serialized, message_bytes)) in (row.next_sequence..next).zip(messages)
    {
        sqlx::query(
            r"
INSERT INTO mcp_streamable_http_events
    (session_id, event_sequence, message, message_bytes, created_at_ms)
VALUES (?, ?, ?, ?, ?)
            ",
        )
        .bind(session_id)
        .bind(sequence)
        .bind(serialized)
        .bind(
            i64::try_from(message_bytes)
                .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?,
        )
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(unavailable)?;
        events.push(StreamableHttpEvent {
            id: StreamableHttpEventId {
                stream_id: row.stream_id.clone(),
                sequence: u64::try_from(sequence)
                    .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?,
            },
            message,
        });
    }
    Ok(Some((events, next, count, bytes)))
}

#[allow(clippy::too_many_lines)]
async fn prepare_delivery_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    mut row: SessionRow,
    request: StreamableHttpPrepareDeliveryRequest,
    now: i64,
    config: &StreamableHttpSessionStoreConfig,
) -> StreamableHttpSessionStoreResult<StreamableHttpPrepareDelivery> {
    let mut changed = false;
    if let Some(cursor) = request.acknowledged_event_id {
        let sequence = i64::try_from(cursor.sequence)
            .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
        let sequence_out_of_range =
            sequence < row.acknowledged_sequence || sequence > row.delivered_sequence;
        if cursor.stream_id != row.stream_id || sequence_out_of_range {
            return Ok(StreamableHttpPrepareDelivery::InvalidCursor);
        }
        if sequence > row.acknowledged_sequence {
            let deleted = sqlx::query(
                "DELETE FROM mcp_streamable_http_events WHERE session_id = ? AND event_sequence <= ?",
            )
            .bind(&request.session.key.session_id)
            .bind(sequence)
            .execute(&mut **transaction)
            .await
            .map_err(unavailable)?;
            let deleted_count = i64::try_from(deleted.rows_affected())
                .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
            let pending_bytes = sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(message_bytes), 0) FROM mcp_streamable_http_events WHERE session_id = ?",
            )
            .bind(&request.session.key.session_id)
            .fetch_one(&mut **transaction)
            .await
            .map_err(unavailable)?;
            row.pending_count = row
                .pending_count
                .checked_sub(deleted_count)
                .ok_or(StreamableHttpSessionStoreError::InvalidData)?;
            row.pending_bytes = pending_bytes;
            row.acknowledged_sequence = sequence;
            changed = true;
        }
    }
    if row.delivery_id.is_some()
        && (row
            .delivery_through
            .is_some_and(|through| through <= row.acknowledged_sequence)
            || row
                .delivery_expires_at_ms
                .is_some_and(|expires| expires <= now))
    {
        row.delivery_id = None;
        row.delivery_from = None;
        row.delivery_through = None;
        row.delivery_expires_at_ms = None;
        changed = true;
    }
    if let (Some(_), Some(expires)) = (&row.delivery_id, row.delivery_expires_at_ms) {
        if changed {
            update_delivery_state(transaction, &request.session.key.session_id, &row, now).await?;
        }
        return Ok(StreamableHttpPrepareDelivery::DeliveryBusy {
            retry_after: Duration::from_millis(u64::try_from(expires - now).unwrap_or(0)),
        });
    }
    let limit = i64::try_from(config.delivery_batch_size)
        .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    let rows = sqlx::query(
        r"
SELECT event_sequence, message
FROM mcp_streamable_http_events
WHERE session_id = ? AND event_sequence > ?
ORDER BY event_sequence
LIMIT ?
        ",
    )
    .bind(&request.session.key.session_id)
    .bind(row.acknowledged_sequence)
    .bind(limit)
    .fetch_all(&mut **transaction)
    .await
    .map_err(unavailable)?;
    let events = rows
        .into_iter()
        .map(|event| {
            persisted_event(
                &row.stream_id,
                event.try_get("event_sequence").map_err(invalid)?,
                &event.try_get::<String, _>("message").map_err(invalid)?,
            )
        })
        .collect::<StreamableHttpSessionStoreResult<Vec<_>>>()?;
    let Some(first) = events.first() else {
        if changed {
            update_delivery_state(transaction, &request.session.key.session_id, &row, now).await?;
        }
        return Ok(StreamableHttpPrepareDelivery::Idle {
            priming_event_id: StreamableHttpEventId {
                stream_id: row.stream_id.clone(),
                sequence: 0,
            },
        });
    };
    let through = i64::try_from(events.last().expect("nonempty batch").id.sequence)
        .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    let delivery_id = fresh_id();
    row.delivered_sequence = row.delivered_sequence.max(through);
    row.delivery_id = Some(delivery_id.clone());
    row.delivery_from = Some(
        i64::try_from(first.id.sequence)
            .map_err(|_| StreamableHttpSessionStoreError::InvalidData)?,
    );
    row.delivery_through = Some(through);
    row.delivery_expires_at_ms = Some(
        now.checked_add(duration_ms(config.delivery_lease)?)
            .ok_or(StreamableHttpSessionStoreError::InvalidData)?,
    );
    update_delivery_state(transaction, &request.session.key.session_id, &row, now).await?;
    Ok(StreamableHttpPrepareDelivery::Events {
        delivery_id,
        events,
    })
}

async fn update_delivery_state(
    transaction: &mut Transaction<'_, Sqlite>,
    session_id: &str,
    row: &SessionRow,
    now: i64,
) -> StreamableHttpSessionStoreResult<()> {
    sqlx::query(
        r"
UPDATE mcp_streamable_http_sessions
SET acknowledged_event_sequence = ?, delivered_event_sequence = ?,
    pending_event_count = ?, pending_event_bytes = ?, delivery_id = ?,
    delivery_from_sequence = ?, delivery_through_sequence = ?,
    delivery_lease_expires_at_ms = ?, updated_at_ms = ?
WHERE session_id = ?
        ",
    )
    .bind(row.acknowledged_sequence)
    .bind(row.delivered_sequence)
    .bind(row.pending_count)
    .bind(row.pending_bytes)
    .bind(&row.delivery_id)
    .bind(row.delivery_from)
    .bind(row.delivery_through)
    .bind(row.delivery_expires_at_ms)
    .bind(now)
    .bind(session_id)
    .execute(&mut **transaction)
    .await
    .map_err(unavailable)?;
    Ok(())
}

async fn cleanup_expired_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    now: i64,
    limit: usize,
) -> StreamableHttpSessionStoreResult<StreamableHttpCleanupExpired> {
    let limit = i64::try_from(limit).map_err(|_| StreamableHttpSessionStoreError::InvalidData)?;
    let ids = sqlx::query_scalar::<_, String>(
        r"
SELECT session_id FROM mcp_streamable_http_sessions
WHERE expires_at_ms <= ?
ORDER BY expires_at_ms, session_id
LIMIT ?
        ",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(&mut **transaction)
    .await
    .map_err(unavailable)?;
    let mut events_deleted = 0_usize;
    for id in &ids {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mcp_streamable_http_events WHERE session_id = ?",
        )
        .bind(id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(unavailable)?;
        events_deleted =
            events_deleted.saturating_add(usize::try_from(count).unwrap_or(usize::MAX));
        sqlx::query("DELETE FROM mcp_streamable_http_sessions WHERE session_id = ?")
            .bind(id)
            .execute(&mut **transaction)
            .await
            .map_err(unavailable)?;
    }
    Ok(StreamableHttpCleanupExpired {
        sessions_deleted: ids.len(),
        events_deleted,
    })
}

async fn acquire_connection(
    pool: &SqlitePool,
) -> StreamableHttpSessionStoreResult<PoolConnection<Sqlite>> {
    let mut connection = pool.acquire().await.map_err(unavailable)?;
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *connection)
        .await
        .map_err(unavailable)?;
    if sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
        .fetch_one(&mut *connection)
        .await
        .map_err(unavailable)?
        != 1
    {
        return Err(StreamableHttpSessionStoreError::Unavailable);
    }
    Ok(connection)
}

async fn validate_pool(pool: &SqlitePool) -> StreamableHttpSessionStoreResult<()> {
    let mut connection = pool.acquire().await.map_err(unavailable)?;
    let database_file = sqlx::query_scalar::<_, String>(
        "SELECT file FROM pragma_database_list WHERE name = 'main'",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(unavailable)?;
    let in_memory = database_file.is_empty();
    drop(connection);
    if !in_memory {
        return Ok(());
    }
    let options = pool.options();
    if options.get_min_connections() >= 1
        && options.get_max_lifetime().is_none()
        && options.get_idle_timeout().is_none()
        && options.get_max_connections() == 1
    {
        Ok(())
    } else {
        Err(StreamableHttpSessionStoreError::Unavailable)
    }
}

fn database_url_uses_memory(database_url: &str) -> bool {
    let database_url = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);
    let (database, parameters) = database_url
        .split_once('?')
        .map_or((database_url, ""), |parts| parts);
    database == ":memory:"
        || parameters.split('&').any(|parameter| {
            parameter
                .split_once('=')
                .is_some_and(|(name, value)| name == "mode" && value == "memory")
        })
}

async fn immediate_transaction(
    connection: &mut SqliteConnection,
) -> StreamableHttpSessionStoreResult<Transaction<'_, Sqlite>> {
    connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(unavailable)
}

fn unavailable<E>(_: E) -> StreamableHttpSessionStoreError {
    StreamableHttpSessionStoreError::Unavailable
}

fn invalid<E>(_: E) -> StreamableHttpSessionStoreError {
    StreamableHttpSessionStoreError::InvalidData
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn delivery_delete_race_returns_not_found_after_successful_initial_read() {
        let directory = tempfile::tempdir().expect("temporary SQLite directory");
        let database_url = format!(
            "sqlite://{}",
            directory.path().join("mcp.sqlite3").display()
        );
        let barrier = DeliveryTestBarrier::new();
        let store = Arc::new(
            SqliteStreamableHttpSessionStore::connect(&database_url)
                .await
                .expect("SQLite store")
                .with_delivery_test_barrier(barrier.clone()),
        );
        let pool = SqlitePool::connect(&database_url)
            .await
            .expect("SQLite race pool");
        let now = SystemTime::now();
        insert_initialized_session(&pool, now).await;

        let delivery = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                        session: session_request(now),
                        acknowledged_event_id: None,
                    })
                    .await
            })
        };
        barrier.wait_for_initial_read().await;
        let mut connection = pool.acquire().await.expect("SQLite delete connection");
        let mut deletion = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .expect("SQLite delete transaction");
        sqlx::query("DELETE FROM mcp_streamable_http_sessions WHERE session_id = ?")
            .bind("delivery-delete-race")
            .execute(&mut *deletion)
            .await
            .expect("delete session");
        deletion.commit().await.expect("commit deletion");
        barrier.resume_delivery();

        assert_eq!(
            delivery
                .await
                .expect("delivery task")
                .expect("store result"),
            StreamableHttpPrepareDelivery::NotFound
        );
    }

    async fn insert_initialized_session(pool: &SqlitePool, now: SystemTime) {
        let now_ms = system_time_ms(now).expect("current epoch time");
        let expires_at_ms = now_ms + duration_ms(Duration::from_mins(1)).unwrap();
        let message = serde_json::to_string(&json!("queued")).unwrap();
        let message_bytes = i64::try_from(message.len()).unwrap();
        sqlx::query(
            r"
INSERT INTO mcp_streamable_http_sessions (
    session_id, owner_key, protocol_version, lifecycle,
    initialization_operation_id, event_stream_id, next_event_sequence,
    pending_event_count, pending_event_bytes, created_at_ms, updated_at_ms, expires_at_ms
) VALUES (?, 'anonymous', '2025-11-25', 'initialized', 'operation', 'stream', 2, 1, ?, ?, ?, ?)
            ",
        )
        .bind("delivery-delete-race")
        .bind(message_bytes)
        .bind(now_ms)
        .bind(now_ms)
        .bind(expires_at_ms)
        .execute(pool)
        .await
        .expect("insert initialized session");
        sqlx::query(
            r"
INSERT INTO mcp_streamable_http_events
    (session_id, event_sequence, message, message_bytes, created_at_ms)
VALUES ('delivery-delete-race', 1, ?, ?, ?)
            ",
        )
        .bind(message)
        .bind(message_bytes)
        .bind(now_ms)
        .execute(pool)
        .await
        .expect("insert queued event");
    }

    fn session_request(now: SystemTime) -> StreamableHttpSessionRequest {
        StreamableHttpSessionRequest {
            key: StreamableHttpSessionKey {
                session_id: "delivery-delete-race".to_owned(),
                owner: StreamableHttpSessionOwner::Anonymous,
            },
            presented_protocol_version: Some("2025-11-25".to_owned()),
            now,
        }
    }
}
