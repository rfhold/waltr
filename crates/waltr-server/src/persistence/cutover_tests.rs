use super::*;
use crate::auth::{
    McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration, OAuthCredential,
    OAuthCredentialStore, OAuthTokenSet,
};
use mcp::oauth::{
    OAuthAuthorizationStore, OAuthAuthorizationStoreConfig, OAuthClient,
    OAuthClientRegistrationMethod,
};
use std::fs;
use waltr_agent::{
    FilesystemSessionStore, PendingTimelineEvent, PendingTimelineItemId, ReplacementUserContent,
    SessionCoordinator, SessionCreateRequest, SessionId, SessionJournalEvent, SessionMaskAddition,
    SessionMaskMutation, SessionStore, TimelineCursor, TimelineItemId,
};

const LEGACY_ROWS: &str = "INSERT INTO routine_invocations(invocation_id,execution_id,routine_name,routine_revision,trigger_name,trigger_kind,correlation_id,declared_tasks_json,status,admitted_at_ms,updated_at_ms) VALUES('retired','execution','title','old-revision','manual','manual','correlation','[]','admitted',1,1); INSERT INTO routine_invocation_metadata VALUES('retired','source-session','preserved-session'); INSERT INTO routine_invocation_events(invocation_id,event_kind,occurred_at_ms) VALUES('retired','invocation_admitted',1);";
const BROWSER_ROW: &str = "INSERT INTO waltr_browser_sessions(token_digest,issuer,client_id,subject,created_at_ms,expires_at_ms,last_seen_at_ms) VALUES('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','https://issuer.invalid','waltr','existing-subject',1,9999999999999,2)";

async fn execute(pool: &PersistencePool, sql: &str) {
    match pool {
        PersistencePool::Sqlite(pool) => {
            sqlx::raw_sql(sql).execute(pool).await.unwrap();
        }
        PersistencePool::PostgreSql(pool) => {
            sqlx::raw_sql(sql).execute(pool).await.unwrap();
        }
    }
}

async fn count(pool: &PersistencePool, sql: &str) -> i64 {
    match pool {
        PersistencePool::Sqlite(pool) => sqlx::query_scalar(sql).fetch_one(pool).await.unwrap(),
        PersistencePool::PostgreSql(pool) => sqlx::query_scalar(sql).fetch_one(pool).await.unwrap(),
    }
}

async fn preserved_files(
    root: &Path,
) -> (FilesystemSessionStore, SessionId, Vec<u8>, PathBuf, Vec<u8>) {
    let config = root.join("config/.waltr/resources/agents/coding.yaml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let source = include_bytes!("../../resources/agents/coding.yaml").to_vec();
    fs::write(&config, &source).unwrap();
    crate::resource_catalog::AgentDefinitionState::load(root.join("config"), &[], "coding")
        .unwrap();
    let store = FilesystemSessionStore::new(root.join("sessions"));
    let id = SessionId::new("preserved-session");
    let created = store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let timeline = store
        .append_timeline(
            &id,
            created.execution_token().unwrap(),
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "Original user content",
                ),
                PendingTimelineEvent::assistant_text_snapshot(
                    id.clone(),
                    0,
                    PendingTimelineItemId::New,
                    "Original assistant content",
                ),
            ],
        )
        .await
        .unwrap();
    store
        .append_journal(
            &id,
            timeline.commit.execution_token,
            vec![SessionJournalEvent::TurnFinished { turn: 0 }],
        )
        .await
        .unwrap();
    let sessions = SessionCoordinator::new(store.clone());
    let mut mutation = SessionMaskMutation::new(Some(TimelineCursor::new(2)));
    mutation.add.push(SessionMaskAddition::items(
        "migration-category",
        TimelineCursor::new(2),
        vec![TimelineItemId::from_first_cursor(TimelineCursor::new(1))],
        ReplacementUserContent::new("Preserved effective summary"),
    ));
    sessions.mutate_context_masks(&id, mutation).await.unwrap();
    let record = serde_json::to_vec(&store.load(&id).await.unwrap()).unwrap();
    assert!(String::from_utf8_lossy(&record).contains("Preserved effective summary"));
    sessions.shutdown().await;
    (store, id, record, config, source)
}

async fn prove(pool: PersistencePool, mcp: impl OAuthAuthorizationStore, root: &Path) {
    execute(&pool, LEGACY_ROWS).await;
    let browser_table = match &pool {
        PersistencePool::Sqlite(_) => "waltr_browser_sessions",
        PersistencePool::PostgreSql(_) => "waltr.browser_sessions",
    };
    execute(
        &pool,
        &BROWSER_ROW.replace("waltr_browser_sessions", browser_table),
    )
    .await;
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM routine_invocation_events").await,
        1
    );
    let outbound = OutboundOAuthStore::new(pool.clone());
    outbound
        .write(
            "mcp:retained".into(),
            OAuthCredential::mcp(
                OAuthTokenSet::bearer("existing-access", Some("existing-refresh".into()), None),
                McpOAuthContext {
                    resource: "https://mcp.invalid/resource".into(),
                    issuer: "https://issuer.invalid".into(),
                    registration: McpOAuthRegistration {
                        client_id: "retained-client".into(),
                        redirect_uri: "https://waltr.invalid/callback".into(),
                        token_endpoint_auth_method: "none".into(),
                        ..Default::default()
                    },
                    endpoints: McpOAuthEndpoints {
                        token: "https://issuer.invalid/token".into(),
                        ..Default::default()
                    },
                },
            ),
        )
        .await
        .unwrap();
    let credential_before = outbound.read("mcp:retained".into()).await.unwrap();
    let client = OAuthClient {
        client_id: "retained-client".into(),
        registration_method: OAuthClientRegistrationMethod::Preconfigured,
        display_name: Some("Existing MCP client".into()),
        client_secret_digest: None,
        redirect_uris: vec!["https://client.invalid/callback".into()],
        created_at_ms: 1,
        expires_at_ms: None,
        disabled_at_ms: None,
    };
    assert!(mcp.register_client(client.clone()).await.unwrap());
    let (filesystem, id, session_before, config, config_before) = preserved_files(root).await;
    match &pool {
        PersistencePool::Sqlite(pool) => assert_eq!(
            migrations::sqlite(pool).await,
            Err(PersistenceError::IncompatibleRoutineState)
        ),
        PersistencePool::PostgreSql(pool) => assert_eq!(
            migrations::postgresql(pool).await,
            Err(PersistenceError::IncompatibleRoutineState)
        ),
    }
    let legacy_tables = match &pool {
        PersistencePool::Sqlite(_) => {
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('routine_invocations','routine_invocation_metadata','routine_invocation_events')"
        }
        PersistencePool::PostgreSql(_) => {
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema='waltr' AND table_name IN ('routine_invocations','routine_invocation_metadata','routine_invocation_events')"
        }
    };
    assert_eq!(count(&pool, legacy_tables).await, 3);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM _sqlx_migrations").await,
        12
    );
    assert_eq!(count(&pool, &format!("SELECT COUNT(*) FROM {browser_table} WHERE issuer='https://issuer.invalid' AND subject='existing-subject' AND expires_at_ms=9999999999999")).await, 1);
    assert_eq!(
        credential_before,
        outbound.read("mcp:retained".into()).await.unwrap()
    );
    assert_eq!(
        mcp.client(client.client_id.clone(), 2).await.unwrap(),
        Some(client)
    );
    assert_eq!(
        session_before,
        serde_json::to_vec(&filesystem.load(&id).await.unwrap()).unwrap()
    );
    assert_eq!(config_before, fs::read(config).unwrap());
    crate::resource_catalog::AgentDefinitionState::load(root.join("config"), &[], "coding")
        .unwrap();
}

#[tokio::test]
async fn populated_legacy_routine_preflight_preserves_all_stores_without_retirement() {
    let root = crate::test_support::temp_root();
    let options = SqliteConnectOptions::new()
        .filename(root.join("waltr.sqlite3"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let mut prior = sqlx::migrate!("migrations/sqlite");
    prior.migrations = std::borrow::Cow::Owned(
        prior
            .iter()
            .filter(|migration| migration.version < 14)
            .cloned()
            .collect(),
    );
    prior.run(&pool).await.unwrap();
    let mcp_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(root.join("mcp.sqlite3"))
                .create_if_missing(true),
        )
        .await
        .unwrap();
    let mcp = mcp::oauth::SqliteOAuthAuthorizationStore::from_pool_with_config(
        mcp_pool.clone(),
        OAuthAuthorizationStoreConfig::default(),
    )
    .await
    .unwrap();
    prove(PersistencePool::Sqlite(pool.clone()), mcp, &root).await;
    pool.close().await;
    mcp_pool.close().await;
    fs::remove_dir_all(root).unwrap();
    if let Ok(raw) = std::env::var("ROUTINE_RETIREMENT_POSTGRES_URL") {
        fourteen_postgres_preservation(raw).await;
    }
}

async fn fourteen_postgres_preservation(raw: String) {
    let mut url = url::Url::parse(&raw).unwrap();
    assert!(matches!(url.scheme(), "postgres" | "postgresql"));
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert!(url.port().is_some());
    assert!(url.username().starts_with("routine_retirement_"));
    assert_eq!(url.path(), format!("/{}", url.username()));
    assert!(url.query().is_none() && url.fragment().is_none());
    url.query_pairs_mut().append_pair("sslmode", "disable");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO waltr")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(url.as_str())
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA waltr")
        .execute(&pool)
        .await
        .expect("fresh fixture required; never reset an existing schema");
    let mut prior = sqlx::migrate!("migrations/postgresql");
    prior.migrations = std::borrow::Cow::Owned(
        prior
            .iter()
            .filter(|migration| migration.version < 14)
            .cloned()
            .collect(),
    );
    prior.run(&pool).await.unwrap();
    let mcp = mcp::oauth::PostgresOAuthAuthorizationStore::from_pool_with_config(
        pool.clone(),
        OAuthAuthorizationStoreConfig::default(),
    )
    .await
    .unwrap();
    let root = crate::test_support::temp_root();
    prove(PersistencePool::PostgreSql(pool.clone()), mcp, &root).await;
    pool.close().await;
    fs::remove_dir_all(root).unwrap();
    println!(
        "PostgreSQL 13-to-14 populated-store preservation verified on the explicitly guarded fixture"
    );
}
