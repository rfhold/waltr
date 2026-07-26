//! `SQLite` Streamable HTTP session store contract and migration tests.

mod streamable_http_store_support;

use std::{
    str::FromStr as _,
    sync::Arc,
    time::{Duration, SystemTime},
};

use mcp::server::*;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tempfile::TempDir;

use streamable_http_store_support::{
    assert_global_admission_ignores_retained_expired,
    assert_principal_admission_ignores_retained_expired, assert_store_contract,
    global_expiry_admission_config, initialize, key, principal_expiry_admission_config, request,
    test_config,
};

#[tokio::test]
async fn sqlite_memory_pool_safeguards() {
    let store =
        SqliteStreamableHttpSessionStore::connect_with_config("sqlite::memory:", test_config())
            .await
            .expect("open memory store");
    initialize(
        &store,
        key("memory", StreamableHttpSessionOwner::Anonymous),
        SystemTime::now(),
        Vec::new(),
    )
    .await;

    let incompatible = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("open default pool");
    let mut invalid_config = test_config();
    invalid_config.max_sessions = 0;
    assert!(matches!(
        SqliteStreamableHttpSessionStore::from_pool_with_config(
            incompatible.clone(),
            invalid_config,
        )
        .await,
        Err(StreamableHttpSessionStoreError::InvalidData)
    ));
    assert!(matches!(
        SqliteStreamableHttpSessionStore::from_pool(incompatible).await,
        Err(StreamableHttpSessionStoreError::Unavailable)
    ));
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let compatible = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .max_lifetime(None)
        .idle_timeout(None)
        .connect_with(options)
        .await
        .unwrap();
    SqliteStreamableHttpSessionStore::from_pool(compatible)
        .await
        .expect("compatible pool");
}

#[tokio::test]
async fn sqlite_store_passes_shared_contract_across_reopen() {
    let database = SqliteDatabase::new();
    let first = SqliteStreamableHttpSessionStore::connect_with_config(&database.url, test_config())
        .await
        .unwrap();
    let reopened =
        SqliteStreamableHttpSessionStore::connect_with_config(&database.url, test_config())
            .await
            .unwrap();
    assert_store_contract(Arc::new(first), Arc::new(reopened)).await;
}

#[tokio::test]
async fn sqlite_admission_counts_only_unexpired_sessions() {
    let global_database = SqliteDatabase::new();
    let global = SqliteStreamableHttpSessionStore::connect_with_config(
        &global_database.url,
        global_expiry_admission_config(),
    )
    .await
    .unwrap();
    assert_global_admission_ignores_retained_expired(&global).await;

    let principal_database = SqliteDatabase::new();
    let principal = SqliteStreamableHttpSessionStore::connect_with_config(
        &principal_database.url,
        principal_expiry_admission_config(),
    )
    .await
    .unwrap();
    assert_principal_admission_ignores_retained_expired(&principal).await;
}

#[tokio::test]
async fn sqlite_idle_delivery_is_a_plain_read_and_schema_constraints_hold() {
    let database = SqliteDatabase::new();
    let store = SqliteStreamableHttpSessionStore::connect_with_config(&database.url, test_config())
        .await
        .unwrap();
    let pool = SqlitePool::connect(&database.url).await.unwrap();
    let now = SystemTime::now();
    let idle = key("idle", StreamableHttpSessionOwner::Anonymous);
    initialize(&store, idle.clone(), now, Vec::new()).await;
    let before = sqlx::query_scalar::<_, i64>(
        "SELECT updated_at_ms FROM mcp_streamable_http_sessions WHERE session_id = 'idle'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(matches!(
        store
            .prepare_delivery(StreamableHttpPrepareDeliveryRequest {
                session: request(idle, now + Duration::from_millis(50)),
                acknowledged_event_id: None,
            })
            .await
            .unwrap(),
        StreamableHttpPrepareDelivery::Idle { .. }
    ));
    let after = sqlx::query_scalar::<_, i64>(
        "SELECT updated_at_ms FROM mcp_streamable_http_sessions WHERE session_id = 'idle'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        store
            .append_events(StreamableHttpAppendEventsRequest {
                session: request(
                    key("idle", StreamableHttpSessionOwner::Anonymous),
                    now + Duration::from_millis(60),
                ),
                messages: Vec::new(),
            })
            .await
            .unwrap(),
        StreamableHttpAppendEvents::Events(Vec::new())
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT updated_at_ms FROM mcp_streamable_http_sessions WHERE session_id = 'idle'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        before
    );

    let invalid_counter = sqlx::query(
        "UPDATE mcp_streamable_http_sessions SET acknowledged_event_sequence = 2 WHERE session_id = 'idle'",
    ).execute(&pool).await;
    assert!(invalid_counter.is_err());
    let invalid_claim = sqlx::query(
        "UPDATE mcp_streamable_http_sessions SET lifecycle = 'initializing' WHERE session_id = 'idle'",
    ).execute(&pool).await;
    assert!(invalid_claim.is_err());
    let invalid_bytes = sqlx::query(
        "INSERT INTO mcp_streamable_http_events (session_id, event_sequence, message, message_bytes, created_at_ms) VALUES ('idle', 1, 'é', 1, 1)",
    ).execute(&pool).await;
    assert!(invalid_bytes.is_err());
}

#[tokio::test]
async fn sqlite_migration_history_uses_checksum_validation() {
    let database = SqliteDatabase::new();
    SqliteStreamableHttpSessionStore::connect(&database.url)
        .await
        .unwrap();
    let pool = SqlitePool::connect(&database.url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 1 AND success = TRUE",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        SqliteStreamableHttpSessionStore::connect(&database.url).await,
        Err(StreamableHttpSessionStoreError::InvalidData)
    ));
}

struct SqliteDatabase {
    _directory: TempDir,
    url: String,
}

impl SqliteDatabase {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.sqlite3");
        Self {
            _directory: directory,
            url: format!("sqlite://{}", path.display()),
        }
    }
}
