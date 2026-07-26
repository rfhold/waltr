//! `PostgreSQL` Streamable HTTP session store contract and locking tests.

mod streamable_http_store_support;

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use mcp::server::*;
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use streamable_http_store_support::{
    assert_global_admission_ignores_retained_expired,
    assert_principal_admission_ignores_retained_expired, assert_store_contract, create_request,
    global_expiry_admission_config, initialize, key, owner, principal_expiry_admission_config,
    request, test_config,
};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

#[tokio::test]
async fn postgres_rejects_invalid_config_before_migration() {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://mcp:mcp@127.0.0.1:1/mcp")
        .unwrap();
    let mut config = test_config();
    config.delivery_batch_size = 0;
    assert!(matches!(
        PostgresStreamableHttpSessionStore::from_pool_with_config(pool, config).await,
        Err(StreamableHttpSessionStoreError::InvalidData)
    ));
}

#[tokio::test]
#[ignore = "requires Docker"]
#[allow(clippy::too_many_lines)]
async fn postgres_store_contract_admission_and_lock_serialization() {
    let image = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "mcp")
        .with_env_var("POSTGRES_PASSWORD", "mcp")
        .with_env_var("POSTGRES_DB", "mcp");
    let container = image.start().await.expect("start PostgreSQL");
    let port = container.get_host_port_ipv4(5432.tcp()).await.unwrap();
    let url = format!("postgres://mcp:mcp@127.0.0.1:{port}/mcp");

    let (first, reopened) = tokio::join!(
        PostgresStreamableHttpSessionStore::connect_with_config(&url, test_config()),
        PostgresStreamableHttpSessionStore::connect_with_config(&url, test_config()),
    );
    let first = Arc::new(first.unwrap());
    let reopened = Arc::new(reopened.unwrap());
    assert_store_contract(first.clone(), reopened).await;

    let pool = PgPool::connect(&url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mcp._sqlx_migrations WHERE version = 1 AND success = TRUE",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT to_regclass('public._sqlx_migrations')::TEXT",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        None
    );

    sqlx::query("TRUNCATE mcp.streamable_http_sessions CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let mut cap = test_config();
    cap.max_sessions = 1;
    cap.max_sessions_per_principal = 10;
    let capped = Arc::new(
        PostgresStreamableHttpSessionStore::connect_with_config(&url, cap)
            .await
            .unwrap(),
    );
    let now = SystemTime::now();
    let (left, right) = tokio::join!(
        capped.create_session(create_request(key("admission-a", owner("a")), now)),
        capped.create_session(create_request(key("admission-b", owner("b")), now)),
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, StreamableHttpCreateSession::Created(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, StreamableHttpCreateSession::CapacityExceeded))
            .count(),
        1
    );

    sqlx::query("TRUNCATE mcp.streamable_http_sessions CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let global = PostgresStreamableHttpSessionStore::connect_with_config(
        &url,
        global_expiry_admission_config(),
    )
    .await
    .unwrap();
    assert_global_admission_ignores_retained_expired(&global).await;

    sqlx::query("TRUNCATE mcp.streamable_http_sessions CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let principal = PostgresStreamableHttpSessionStore::connect_with_config(
        &url,
        principal_expiry_admission_config(),
    )
    .await
    .unwrap();
    assert_principal_admission_ignores_retained_expired(&principal).await;
    sqlx::query("TRUNCATE mcp.streamable_http_sessions CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let locked = key("locked", StreamableHttpSessionOwner::Anonymous);
    initialize(&*first, locked.clone(), now, Vec::new()).await;
    let mut transaction = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT 1 FROM mcp.streamable_http_sessions WHERE session_id = 'locked' FOR UPDATE",
    )
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    let appending = {
        let first = first.clone();
        let locked = locked.clone();
        tokio::spawn(async move {
            first
                .append_events(StreamableHttpAppendEventsRequest {
                    session: request(locked, now),
                    messages: vec![json!("serialized")],
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!appending.is_finished());
    transaction.rollback().await.unwrap();
    assert!(matches!(
        appending.await.unwrap().unwrap(),
        StreamableHttpAppendEvents::Events(_)
    ));
    assert!(sqlx::query(
        "INSERT INTO mcp.streamable_http_events (session_id, event_sequence, message, message_bytes, created_at_ms) VALUES ('locked', 99, 'é', 1, 1)",
    ).execute(&pool).await.is_err());

    assert_migration_checksum_and_search_path_reset(&pool).await;
}

async fn assert_migration_checksum_and_search_path_reset(pool: &PgPool) {
    let isolated = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    sqlx::query("SET search_path TO pg_catalog")
        .execute(&isolated)
        .await
        .unwrap();
    PostgresStreamableHttpSessionStore::from_pool(isolated.clone())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW search_path")
            .fetch_one(&isolated)
            .await
            .unwrap(),
        r#""$user", public"#
    );
    sqlx::query("UPDATE mcp._sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = 1")
        .execute(pool)
        .await
        .unwrap();
    assert!(matches!(
        PostgresStreamableHttpSessionStore::from_pool(isolated.clone()).await,
        Err(StreamableHttpSessionStoreError::InvalidData)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW search_path")
            .fetch_one(&isolated)
            .await
            .unwrap(),
        r#""$user", public"#
    );
}
