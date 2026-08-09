//! `PostgreSQL` OAuth authorization store conformance.

mod oauth_store_support;

use std::{borrow::Cow, sync::Arc};

use mcp::oauth::{
    McpPrincipalId, OAuthAuthorizationStore, OAuthValidateAccessTokenRequest,
    PostgresOAuthAuthorizationStore,
};
use sqlx::{PgPool, migrate::Migrator};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

static MIGRATOR: Migrator = sqlx::migrate!("migrations/postgresql");

#[tokio::test]
#[ignore = "requires Docker"]
#[allow(
    clippy::too_many_lines,
    reason = "migration, reopen, locking, and storage assertions form one store contract"
)]
async fn postgres_store_passes_contract_across_reopen() {
    let image = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "mcp")
        .with_env_var("POSTGRES_PASSWORD", "mcp")
        .with_env_var("POSTGRES_DB", "mcp");
    let container = image.start().await.unwrap();
    let port = container.get_host_port_ipv4(5432.tcp()).await.unwrap();
    let url = format!("postgres://mcp:mcp@127.0.0.1:{port}/mcp");
    let pool = PgPool::connect(&url).await.unwrap();
    sqlx::query("CREATE SCHEMA mcp")
        .execute(&pool)
        .await
        .unwrap();
    legacy_migrator().run(&pool).await.unwrap();
    sqlx::query("INSERT INTO mcp.oauth_clients (client_id,display_metadata,created_at_ms,redirect_limit) VALUES ('legacy-client','legacy metadata',1,4)")
        .execute(&pool)
        .await
        .unwrap();
    let (first, reopened) = tokio::join!(
        PostgresOAuthAuthorizationStore::connect_with_config(&url, oauth_store_support::config()),
        PostgresOAuthAuthorizationStore::connect_with_config(&url, oauth_store_support::config())
    );
    let first = Arc::new(first.unwrap());
    let reopened = Arc::new(reopened.unwrap());
    let validator = first.clone();
    oauth_store_support::assert_store_contract(first, reopened).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(1296257103, 1331053396)")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        validator.validate_access_token(missing_validation()),
    )
    .await
    .expect("validation must not wait for the PostgreSQL mutation lock")
    .unwrap();
    lock.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_one(&pool)
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT display_metadata,registration_method,redirect_limit FROM mcp.oauth_clients WHERE client_id = 'legacy-client'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        ("legacy metadata".to_owned(), "preconfigured".to_owned(), 4)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mcp.oauth_grants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT client_secret_digest FROM mcp.oauth_clients WHERE client_id = 'client'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        vec![31; 32]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mcp.oauth_refresh_tokens WHERE octet_length(token_digest) = 32"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mcp.oauth_signing_keys WHERE encrypted_private_material IS NOT NULL AND octet_length(encryption_nonce) = 12"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'mcp' AND table_name = 'oauth_authorization_state'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
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
