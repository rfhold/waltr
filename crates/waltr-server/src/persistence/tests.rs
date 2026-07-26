use std::{ffi::OsStr, fs, path::PathBuf, time::SystemTime};

use sqlx::postgres::PgSslMode;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use uuid::Uuid;

use super::*;

fn missing_mcp_session_request() -> mcp::server::StreamableHttpSessionRequest {
    mcp::server::StreamableHttpSessionRequest {
        key: mcp::server::StreamableHttpSessionKey {
            session_id: "missing".to_owned(),
            owner: mcp::server::StreamableHttpSessionOwner::Anonymous,
        },
        presented_protocol_version: None,
        now: SystemTime::now(),
    }
}

#[test]
fn derives_component_targets_for_supported_backends() {
    let sqlite =
        DatabaseConfig::from_url("sqlite:///var/lib/waltr/waltr.sqlite3").expect("parse SQLite URL");
    assert_eq!(sqlite.backend(), DatabaseBackend::Sqlite);
    assert_eq!(sqlite.mcp_backend(), DatabaseBackend::Sqlite);
    assert_eq!(
        sqlite.sqlite_path(),
        Some(Path::new("/var/lib/waltr/waltr.sqlite3"))
    );
    assert_eq!(
        sqlite.mcp_sqlite_path(),
        Some(Path::new("/var/lib/waltr/mcp.sqlite3"))
    );

    let memory = DatabaseConfig::for_tests().expect("create memory config");
    assert_eq!(memory.sqlite_path(), None);
    assert_eq!(memory.mcp_sqlite_path(), None);

    for url in [
        "postgres://waltr:secret@localhost/waltr",
        "postgresql://waltr:secret@localhost/waltr",
    ] {
        let config = DatabaseConfig::from_url(url).expect("parse PostgreSQL URL");
        assert_eq!(config.backend(), DatabaseBackend::PostgreSql);
        assert_eq!(config.mcp_backend(), DatabaseBackend::PostgreSql);
    }
}

#[test]
fn supports_explicit_independent_targets_and_rejects_sqlite_collisions() {
    let mixed = DatabaseConfig::from_urls(
        "sqlite:///var/lib/waltr/waltr.sqlite3",
        "postgres://mcp:secret@localhost/mcp",
    )
    .expect("parse mixed targets");
    assert_eq!(mixed.backend(), DatabaseBackend::Sqlite);
    assert_eq!(mixed.mcp_backend(), DatabaseBackend::PostgreSql);

    for mcp_url in [
        "sqlite:///var/lib/waltr/waltr.sqlite3",
        "sqlite:///var/lib/waltr/./waltr.sqlite3",
        "sqlite:///var/lib/other/../waltr/waltr.sqlite3",
    ] {
        assert_eq!(
            DatabaseConfig::from_urls("sqlite:///var/lib/waltr/waltr.sqlite3", mcp_url)
                .expect_err("reject same SQLite file"),
            PersistenceError::Configuration(
                "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same SQLite file"
            )
        );
    }
    assert!(
        DatabaseConfig::from_url("sqlite:///var/lib/waltr/mcp.sqlite3").is_err(),
        "derived MCP target must not collide with the Waltr file"
    );
}

#[test]
fn rejects_aliases_for_the_same_named_shared_memory_database() {
    let error = DatabaseConfig::from_urls(
        "sqlite://file:named-memory?mode=memory&cache=shared",
        "sqlite:file%3Anamed-memory?cache=shared&%6Dode=memory",
    )
    .expect_err("reject shared named memory collision");
    assert_eq!(
        error,
        PersistenceError::Configuration(
            "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same named shared in-memory SQLite database"
        )
    );

    DatabaseConfig::from_urls("sqlite::memory:", "sqlite::memory:")
        .expect("private in-memory databases remain independent");
    DatabaseConfig::from_urls(
        "sqlite://file:waltr-memory?mode=memory&cache=shared",
        "sqlite://file:mcp-memory?mode=memory&cache=shared",
    )
    .expect("distinct named shared-memory databases remain independent");
}

#[test]
fn rejects_nested_file_uris_in_both_component_positions() {
    for nested in [
        "sqlite:file:/tmp/waltr.sqlite3",
        "sqlite://file:/tmp/waltr.sqlite3",
        "sqlite:file%3A/tmp/waltr.sqlite3",
        "sqlite://FiLe%3A/tmp/waltr.sqlite3",
    ] {
        assert_eq!(
            DatabaseConfig::from_url(nested).expect_err("reject nested Waltr file URI"),
            PersistenceError::Configuration(
                "WALTR_DATABASE_URL must use a direct SQLite path rather than a nested file: URI"
            ),
            "{nested}"
        );
        assert_eq!(
            DatabaseConfig::from_urls("sqlite:///tmp/waltr.sqlite3", nested)
                .expect_err("reject nested MCP file URI"),
            PersistenceError::Configuration(
                "WALTR_MCP_DATABASE_URL must use a direct SQLite path rather than a nested file: URI"
            ),
            "{nested}"
        );
    }
}

#[test]
fn rejects_relative_absolute_and_cross_position_file_aliases() {
    let relative_root = PathBuf::from(format!("target/waltr-persistence-test-{}", Uuid::new_v4()));
    let database = TestDatabase {
        root: env::current_dir().unwrap().join(&relative_root),
    };
    fs::create_dir_all(&database.root).unwrap();
    let absolute = database.root.join("same.sqlite3");
    let relative = relative_root.join("same.sqlite3");
    for (waltr, mcp) in [(absolute.clone(), relative.clone()), (relative, absolute)] {
        assert!(
            DatabaseConfig::from_urls(
                &format!("sqlite://{}", waltr.display()),
                &format!("sqlite://{}", mcp.display())
            )
            .is_err()
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_nonexistent_file_aliases_beneath_symlinked_parents() {
    use std::os::unix::fs::symlink;

    let database = TestDatabase::new();
    let physical = database.root.join("physical");
    let alias = database.root.join("alias");
    fs::create_dir_all(&physical).expect("create physical parent");
    symlink(&physical, &alias).expect("create parent symlink");

    let waltr_url = format!(
        "sqlite://{}",
        physical.join("nested/database.sqlite3").display()
    );
    let mcp_url = format!(
        "sqlite://{}",
        alias.join("nested/database.sqlite3").display()
    );
    assert_eq!(
        DatabaseConfig::from_urls(&waltr_url, &mcp_url).expect_err("reject physical file alias"),
        PersistenceError::Configuration(
            "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same SQLite file"
        )
    );
}

#[test]
fn independent_nonexistent_sqlite_targets_remain_valid() {
    let database = TestDatabase::new();
    DatabaseConfig::from_urls(&database.url("waltr.sqlite3"), &database.url("mcp.sqlite3"))
        .expect("independent nonexistent files must remain valid");
}

#[tokio::test]
async fn rejects_existing_hard_linked_sqlite_targets_before_migration() {
    let database = TestDatabase::new();
    fs::create_dir_all(&database.root).unwrap();
    let original = database.root.join("original.sqlite3");
    let alias = database.root.join("alias.sqlite3");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&original)
        .unwrap();
    fs::hard_link(&original, &alias).unwrap();

    for (waltr, mcp) in [(&original, &alias), (&alias, &original)] {
        let config = DatabaseConfig::from_urls(
            &format!("sqlite://{}", waltr.display()),
            &format!("sqlite://{}", mcp.display()),
        )
        .expect("distinct paths parse before filesystem identity comparison");
        assert_eq!(
            Persistence::initialize(config)
                .await
                .expect_err("hard links must not share component migrations"),
            PersistenceError::Configuration(
                "WALTR_DATABASE_URL and WALTR_MCP_DATABASE_URL must not use the same SQLite file"
            )
        );
    }
    assert_eq!(fs::metadata(original).unwrap().len(), 0);
}

#[test]
fn derived_sqlite_target_preserves_safe_parsed_options() {
    let config = DatabaseConfig::from_url(
        "sqlite:///var/lib/waltr/waltr.sqlite3?cache=shared&vfs=unix-dotfile",
    )
    .expect("parse SQLite options");
    let DatabaseTarget::Sqlite {
        options: waltr_options,
        ..
    } = &config.waltr
    else {
        panic!("expected Waltr SQLite target");
    };
    let DatabaseTarget::Sqlite {
        options: mcp_options,
        ..
    } = &config.mcp
    else {
        panic!("expected MCP SQLite target");
    };

    assert_eq!(
        mcp_options.get_filename(),
        Path::new("/var/lib/waltr/mcp.sqlite3")
    );
    for evidence in ["shared_cache: true", "vfs: Some(\"unix-dotfile\")"] {
        assert!(format!("{waltr_options:?}").contains(evidence));
        assert!(format!("{mcp_options:?}").contains(evidence));
    }
}

#[test]
fn empty_mcp_environment_value_is_treated_as_unset() {
    assert_eq!(
        optional_env_value(ENV_MCP_DATABASE_URL, Ok(String::new()))
            .expect("read empty environment value"),
        None
    );
}

#[test]
fn rejects_invalid_urls_without_exposing_values() {
    let secret = "highly-secret-password";
    for (waltr, mcp) in [
        ("mysql://localhost/waltr".to_owned(), None),
        (format!("postgres://user:{secret}@[invalid/waltr"), None),
        (
            "sqlite:///tmp/waltr.sqlite3".to_owned(),
            Some(format!("postgres://user:{secret}@[invalid/mcp")),
        ),
    ] {
        let error = mcp
            .map_or_else(
                || DatabaseConfig::from_url(&waltr),
                |mcp| DatabaseConfig::from_urls(&waltr, &mcp),
            )
            .expect_err("reject invalid URL");
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains(secret));
        assert!(!diagnostic.contains(&waltr));
        assert!(diagnostic.contains("DATABASE_URL"));
    }
}

#[test]
fn durable_defaults_use_separate_application_database_files() {
    let config = DatabaseConfig::default_in(
        &default_data_directory(None, Some(OsStr::new("/home/test")))
            .expect("select HOME data directory"),
    )
    .expect("create default config");
    assert_eq!(
        config.sqlite_path(),
        Some(Path::new("/home/test/.local/share/waltr/waltr.sqlite3"))
    );
    assert_eq!(
        config.mcp_sqlite_path(),
        Some(Path::new("/home/test/.local/share/waltr/mcp.sqlite3"))
    );
    assert_eq!(
        default_data_directory(
            Some(OsStr::new("/var/lib/test-data")),
            Some(OsStr::new("/home/test"))
        )
        .expect("select XDG data directory"),
        Path::new("/var/lib/test-data/waltr")
    );
    assert!(default_data_directory(None, None).is_err());
}

#[test]
fn postgres_ssl_requires_encryption_unless_explicitly_configured() {
    let default = DatabaseConfig::from_url("postgres://waltr:secret@database.example/waltr")
        .expect("parse default SSL policy");
    assert!(matches!(
        postgres_ssl_mode(&default.waltr),
        PgSslMode::Require
    ));

    for (mode, expected) in [
        ("disable", PgSslMode::Disable),
        ("require", PgSslMode::Require),
        ("verify-full", PgSslMode::VerifyFull),
    ] {
        let config = DatabaseConfig::from_url(&format!(
            "postgres://waltr:secret@database.example/waltr?sslmode={mode}"
        ))
        .expect("parse explicit SSL policy");
        assert_eq!(
            std::mem::discriminant(&postgres_ssl_mode(&config.waltr)),
            std::mem::discriminant(&expected)
        );
    }

    for encoded_name in ["%73slmode", "ssl%2Dmode"] {
        let config = DatabaseConfig::from_url(&format!(
            "postgres://waltr:secret@database.example/waltr?{encoded_name}=disable"
        ))
        .expect("parse encoded SSL option name");
        assert!(matches!(
            postgres_ssl_mode(&config.waltr),
            PgSslMode::Disable
        ));
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the test verifies one migration lifecycle"
)]
async fn sqlite_creates_both_parents_and_restarts_idempotently() {
    let database = TestDatabase::new();
    let waltr_url = database.url("waltr/nested/waltr.sqlite3");
    let mcp_url = database.url("mcp/nested/mcp.sqlite3");
    let config = DatabaseConfig::from_urls(&waltr_url, &mcp_url).expect("create config");

    let first = Persistence::initialize(config.clone())
        .await
        .expect("initialize fresh databases");
    let second = Persistence::initialize(config)
        .await
        .expect("reinitialize databases");
    assert_eq!(first.backend(), DatabaseBackend::Sqlite);
    assert_eq!(first.mcp_backend(), DatabaseBackend::Sqlite);

    assert_component_histories(&first).await;
    assert_component_histories(&second).await;
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the test verifies one migration lifecycle"
)]
async fn configured_paths_support_wal_drop_reconnect_and_persisted_records() {
    let database = TestDatabase::new();
    let waltr_path = database.root.join("waltr.sqlite3");
    let mcp_path = database.root.join("mcp.sqlite3");
    let config = DatabaseConfig::from_urls(
        &format!("sqlite://{}", waltr_path.display()),
        &format!("sqlite://{}", mcp_path.display()),
    )
    .unwrap();
    let first = Persistence::initialize(config.clone()).await.unwrap();
    let waltr_pool = first.sqlite_pool().unwrap();
    let mcp_pool = first.mcp_sqlite_pool().unwrap();

    assert_eq!(sqlite_main_path(waltr_pool).await, waltr_path);
    assert_eq!(sqlite_main_path(mcp_pool).await, mcp_path);
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode = DELETE")
            .fetch_one(mcp_pool)
            .await
            .unwrap()
            .to_ascii_lowercase(),
        "delete"
    );
    let mut transaction = mcp_pool.begin().await.unwrap();
    sqlx::query("CREATE TABLE rollback_journal_marker (value TEXT NOT NULL)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rollback_journal_marker (value) VALUES ('rolled-back')")
        .execute(&mut *transaction)
        .await
        .unwrap();
    assert!(mcp_path.with_extension("sqlite3-journal").exists());
    transaction.rollback().await.unwrap();

    let journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode = WAL")
        .fetch_one(waltr_pool)
        .await
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    sqlx::query("CREATE TABLE persistence_marker (value TEXT NOT NULL)")
        .execute(waltr_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO persistence_marker (value) VALUES ('survives-restart')")
        .execute(waltr_pool)
        .await
        .unwrap();
    assert!(waltr_path.with_extension("sqlite3-wal").exists());
    for path in [
        sqlite_main_path(waltr_pool).await,
        sqlite_main_path(mcp_pool).await,
    ] {
        let rendered = path.to_string_lossy();
        assert!(!rendered.contains("/proc/self/fd/"));
        assert!(!rendered.contains("/dev/fd/"));
        assert!(!rendered.contains(" (deleted)"));
    }

    waltr_pool.close().await;
    mcp_pool.close().await;
    drop(first);

    let second = Persistence::initialize(config).await.unwrap();
    assert_eq!(
        sqlite_main_path(second.sqlite_pool().unwrap()).await,
        waltr_path
    );
    assert_eq!(
        sqlite_main_path(second.mcp_sqlite_pool().unwrap()).await,
        mcp_path
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM persistence_marker")
            .fetch_one(second.sqlite_pool().unwrap())
            .await
            .unwrap(),
        "survives-restart"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(second.sqlite_pool().unwrap())
            .await
            .unwrap()
            .to_ascii_lowercase(),
        "wal"
    );
}

#[tokio::test]
async fn sqlite_files_keep_component_tables_and_histories_isolated() {
    let database = TestDatabase::new();
    let persistence = Persistence::initialize(
        DatabaseConfig::from_url(&database.url("waltr.sqlite3")).expect("create config"),
    )
    .await
    .expect("initialize persistence");
    let waltr_pool = persistence.sqlite_pool().expect("Waltr SQLite pool");
    let mcp_pool = persistence.mcp_sqlite_pool().expect("MCP SQLite pool");

    let waltr_tables = sqlite_tables(waltr_pool).await;
    let mcp_tables = sqlite_tables(mcp_pool).await;
    assert_eq!(
        waltr_tables,
        [
            "_sqlx_migrations",
            "waltr_browser_sessions",
            "waltr_mcp_oauth_login_continuations",
            "waltr_oidc_login_attempts",
            "outbound_oauth_attempts",
            "outbound_oauth_credentials"
        ]
    );
    assert_eq!(
        mcp_tables,
        [
            "_sqlx_migrations",
            "mcp_oauth_access_tokens",
            "mcp_oauth_authorization_codes",
            "mcp_oauth_authorization_transactions",
            "mcp_oauth_client_metadata_cache",
            "mcp_oauth_client_redirect_uris",
            "mcp_oauth_clients",
            "mcp_oauth_consents",
            "mcp_oauth_dynamic_clients",
            "mcp_oauth_grants",
            "mcp_oauth_refresh_families",
            "mcp_oauth_refresh_tokens",
            "mcp_oauth_registration_rates",
            "mcp_oauth_revoked_clients",
            "mcp_oauth_signing_keys",
            "mcp_streamable_http_events",
            "mcp_streamable_http_sessions"
        ]
    );
    assert!(matches!(
        persistence
            .mcp_session_store()
            .inspect_session(missing_mcp_session_request())
            .await
            .expect("read MCP store"),
        mcp::server::StreamableHttpInspectSession::NotFound
    ));
}

#[tokio::test]
async fn memory_configuration_retains_two_independent_one_connection_databases() {
    let persistence = Persistence::initialize(DatabaseConfig::for_tests().expect("test config"))
        .await
        .expect("initialize memory persistence");
    assert_component_histories(&persistence).await;
    assert_eq!(persistence.sqlite_pool().expect("Waltr pool").size(), 1);
    assert_eq!(persistence.mcp_sqlite_pool().expect("MCP pool").size(), 1);
    assert!(
        !sqlite_tables(persistence.sqlite_pool().expect("Waltr pool"))
            .await
            .iter()
            .any(|table| table.starts_with("mcp_"))
    );
}

#[tokio::test]
async fn standard_sqlx_checksum_mismatch_is_rejected() {
    let database = TestDatabase::new();
    let config = DatabaseConfig::from_url(&database.url("waltr.sqlite3")).expect("create config");
    let persistence = Persistence::initialize(config.clone())
        .await
        .expect("initialize persistence");
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1")
        .execute(persistence.sqlite_pool().expect("Waltr pool"))
        .await
        .expect("change checksum");
    drop(persistence);

    assert_eq!(
        Persistence::initialize(config)
            .await
            .expect_err("reject checksum mismatch"),
        PersistenceError::Migration(DatabaseBackend::Sqlite)
    );
}

#[tokio::test]
async fn sqlite_upgrades_committed_migrations_through_outbound_oauth_and_reopens() {
    let database = TestDatabase::new();
    fs::create_dir_all(&database.root).expect("create test database directory");
    let legacy_migrations = database.root.join("legacy-migrations");
    fs::create_dir_all(&legacy_migrations).expect("create legacy migration directory");
    let migration_source = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/sqlite");
    for migration in [
        "0001_schema_ownership.sql",
        "0002_oidc_access_auth.sql",
        "0003_mcp_oauth_login_continuations.sql",
    ] {
        fs::copy(
            migration_source.join(migration),
            legacy_migrations.join(migration),
        )
        .expect("copy committed migration");
    }
    let url = database.url("waltr.sqlite3");
    let options = SqliteConnectOptions::from_str(&url)
        .expect("parse SQLite URL")
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .expect("connect legacy database");
    sqlx::migrate::Migrator::new(legacy_migrations)
        .await
        .expect("load legacy migrations")
        .run(&pool)
        .await
        .expect("run committed migrations");
    pool.close().await;

    let config = DatabaseConfig::from_url(&url).expect("create config");
    let upgraded = Persistence::initialize(config.clone())
        .await
        .expect("upgrade database");
    assert_component_histories(&upgraded).await;
    assert!(
        sqlite_tables(upgraded.sqlite_pool().expect("Waltr pool"))
            .await
            .contains(&"outbound_oauth_credentials".to_owned())
    );
    drop(upgraded);

    let reopened = Persistence::initialize(config)
        .await
        .expect("reopen upgraded database");
    assert_component_histories(&reopened).await;
}

#[tokio::test]
async fn sqlite_enables_foreign_keys_on_every_component_connection() {
    let database = TestDatabase::new();
    let persistence = Persistence::initialize(
        DatabaseConfig::from_url(&database.url("waltr.sqlite3")).expect("create config"),
    )
    .await
    .expect("initialize persistence");
    for pool in [
        persistence.sqlite_pool().expect("Waltr pool"),
        persistence.mcp_sqlite_pool().expect("MCP pool"),
    ] {
        let mut connection = pool.acquire().await.expect("acquire SQLite connection");
        let enabled = sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await
            .expect("read foreign-key setting");
        assert_eq!(enabled, 1);
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
#[allow(
    clippy::too_many_lines,
    reason = "the test verifies one cross-schema migration lifecycle"
)]
async fn postgres_uses_independent_pools_and_component_schemas() {
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
        .expect("read PostgreSQL port");
    let url = format!("postgres://waltr:waltr@127.0.0.1:{port}/waltr?sslmode=disable");
    let config = DatabaseConfig::from_url(&url).expect("create config");

    let (first, second) = tokio::join!(
        Persistence::initialize(config.clone()),
        Persistence::initialize(config)
    );
    let first = first.expect("initialize PostgreSQL");
    let second = second.expect("initialize PostgreSQL concurrently");
    assert_eq!(first.backend(), DatabaseBackend::PostgreSql);
    assert_eq!(first.mcp_backend(), DatabaseBackend::PostgreSql);
    assert!(!std::ptr::eq(
        first.postgres_pool().expect("Waltr pool"),
        first.mcp_postgres_pool().expect("MCP pool")
    ));

    let pool = first.postgres_pool().expect("Waltr PostgreSQL pool");
    let histories = sqlx::query_scalar::<_, String>(
        "SELECT table_schema || '.' || table_name FROM information_schema.tables WHERE table_schema IN ('waltr', 'mcp') AND table_name = '_sqlx_migrations' ORDER BY table_schema",
    )
    .fetch_all(pool)
    .await
    .expect("inventory migration histories");
    assert_eq!(histories, ["waltr._sqlx_migrations", "mcp._sqlx_migrations"]);
    let waltr_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM waltr._sqlx_migrations")
        .fetch_one(pool)
        .await
        .expect("count Waltr migrations");
    let mcp_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mcp._sqlx_migrations")
        .fetch_one(pool)
        .await
        .expect("count MCP migrations");
    assert_eq!((waltr_count, mcp_count), (4, 3));
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT to_regclass('waltr.outbound_oauth_attempts')::text"
        )
        .fetch_one(pool)
        .await
        .expect("find outbound OAuth attempts")
        .is_some()
    );
    let outbound = first.outbound_oauth_store();
    outbound
        .save_attempt(
            &DurableAttemptRecord {
                attempt_id: "postgres-attempt".to_owned(),
                target_key: "mcp:postgres".to_owned(),
                attempt_json: "{}".to_owned(),
                registration_method: "dynamic".to_owned(),
                completion_mode: "server_callback".to_owned(),
                status: "pending".to_owned(),
                failure_reason: None,
                expires_at_ms: i64::MAX,
            },
            "postgres-state",
        )
        .await
        .expect("save PostgreSQL outbound attempt");
    let other_outbound = second.outbound_oauth_store();
    let (first_claim, second_claim) = tokio::join!(
        outbound.claim_attempt("postgres-attempt", "first", i64::MAX),
        other_outbound.claim_attempt("postgres-attempt", "second", i64::MAX),
    );
    assert_ne!(
        first_claim.expect("first PostgreSQL claim"),
        second_claim.expect("second PostgreSQL claim")
    );
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT to_regclass('waltr.oidc_login_attempts')::text"
        )
        .fetch_one(pool)
        .await
        .expect("find Waltr table")
        .is_some()
    );
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT to_regclass('mcp.streamable_http_sessions')::text"
        )
        .fetch_one(pool)
        .await
        .expect("find MCP table")
        .is_some()
    );
    assert!(matches!(
        second
            .mcp_session_store()
            .inspect_session(missing_mcp_session_request())
            .await
            .expect("read MCP store"),
        mcp::server::StreamableHttpInspectSession::NotFound
    ));

    sqlx::query(
        "UPDATE waltr._sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = 1",
    )
    .execute(pool)
    .await
    .expect("change Waltr checksum");
    assert_eq!(
        Persistence::initialize(DatabaseConfig::from_url(&url).expect("changed config"))
            .await
            .expect_err("reject checksum mismatch"),
        PersistenceError::Migration(DatabaseBackend::PostgreSql)
    );
}

#[test]
fn configuration_and_connection_diagnostics_are_sanitized() {
    let secret = "database-password-must-not-leak";
    let config = DatabaseConfig::from_urls(
        &format!("postgres://waltr:{secret}@database.example/waltr"),
        &format!("postgres://mcp:{secret}@database.example/mcp"),
    )
    .expect("parse URLs");
    assert!(!format!("{config:?}").contains(secret));
    assert!(!format!("{:?}", PersistenceError::Connection(config.backend())).contains(secret));
}

async fn assert_component_histories(persistence: &Persistence) {
    let waltr_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(persistence.sqlite_pool().expect("Waltr pool"))
        .await
        .expect("count Waltr migrations");
    let mcp_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(persistence.mcp_sqlite_pool().expect("MCP pool"))
        .await
        .expect("count MCP migrations");
    assert_eq!((waltr_count, mcp_count), (4, 3));
}

async fn sqlite_tables(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("inventory SQLite tables")
}

async fn sqlite_main_path(pool: &SqlitePool) -> PathBuf {
    sqlx::query_as::<_, (i64, String, String)>("PRAGMA database_list")
        .fetch_all(pool)
        .await
        .expect("read SQLite database paths")
        .into_iter()
        .find_map(|(_, name, path)| (name == "main").then(|| PathBuf::from(path)))
        .expect("SQLite main database path")
}

struct TestDatabase {
    root: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        Self {
            root: env::temp_dir().join(format!("waltr-persistence-test-{}", Uuid::new_v4())),
        }
    }

    fn url(&self, relative_path: &str) -> String {
        format!("sqlite://{}", self.root.join(relative_path).display())
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

fn postgres_ssl_mode(target: &DatabaseTarget) -> PgSslMode {
    match target {
        DatabaseTarget::PostgreSql { options } => options.get_ssl_mode(),
        DatabaseTarget::Sqlite { .. } => panic!("expected PostgreSQL config"),
    }
}
