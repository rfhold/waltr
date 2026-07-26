//! `SQLite` OAuth authorization store conformance and migration tests.

mod oauth_store_support;

use std::{borrow::Cow, str::FromStr as _, sync::Arc};

use mcp::oauth::{
    McpPrincipalId, OAuthAuthorizationStore, OAuthAuthorizationStoreError,
    OAuthValidateAccessTokenRequest, SqliteOAuthAuthorizationStore,
};
use sqlx::{
    Connection as _, SqliteConnection, SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

static MIGRATOR: Migrator = sqlx::migrate!("migrations/sqlite");

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "migration, reopen, locking, and storage assertions form one store contract"
)]
async fn sqlite_store_passes_contract_across_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}",
        directory.path().join("mcp.sqlite3").display()
    );
    let options = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    legacy_migrator().run(&pool).await.unwrap();
    sqlx::query("INSERT INTO mcp_oauth_clients (client_id,display_metadata,created_at_ms,redirect_limit) VALUES ('legacy-client','legacy metadata',1,4)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let first = Arc::new(
        SqliteOAuthAuthorizationStore::connect_with_config(&url, oauth_store_support::config())
            .await
            .unwrap(),
    );
    let reopened = Arc::new(
        SqliteOAuthAuthorizationStore::connect_with_config(&url, oauth_store_support::config())
            .await
            .unwrap(),
    );
    let validator = first.clone();
    oauth_store_support::assert_store_contract(first, reopened).await;
    let mut writer = SqliteConnection::connect(&url).await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut writer)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        validator.validate_access_token(missing_validation()),
    )
    .await
    .expect("validation must not wait for the SQLite writer lock")
    .unwrap();
    sqlx::query("ROLLBACK").execute(&mut writer).await.unwrap();
    let pool = SqlitePool::connect(&url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT display_metadata,registration_method,redirect_limit FROM mcp_oauth_clients WHERE client_id = 'legacy-client'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        ("legacy metadata".to_owned(), "preconfigured".to_owned(), 4)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mcp_oauth_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT client_secret_digest FROM mcp_oauth_clients WHERE client_id = 'client'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        vec![31; 32]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mcp_oauth_refresh_tokens WHERE length(token_digest) = 32"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mcp_oauth_signing_keys WHERE encrypted_private_material IS NOT NULL AND length(encryption_nonce) = 12"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'mcp_oauth_authorization_state'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );

    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 2")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        SqliteOAuthAuthorizationStore::connect(&url).await,
        Err(OAuthAuthorizationStoreError::InvalidData)
    ));
}

fn legacy_migrator() -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version <= 2)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    }
}

fn missing_validation() -> OAuthValidateAccessTokenRequest {
    OAuthValidateAccessTokenRequest {
        jti: "missing".to_owned(),
        client_id: "client".to_owned(),
        grant_id: "grant".to_owned(),
        principal_id: McpPrincipalId::new("alice").unwrap(),
        resource: "https://resource.example/mcp".to_owned(),
        scopes: vec!["read".to_owned()],
        now_ms: 20_000,
    }
}
