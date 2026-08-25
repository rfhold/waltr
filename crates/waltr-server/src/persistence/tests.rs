use std::{collections::BTreeMap, ffi::OsStr, fs, path::PathBuf, time::Duration};

use sqlx::postgres::PgSslMode;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use uuid::Uuid;

use super::*;

#[test]
fn routine_migrations_have_matching_diagnostic_indexes() {
    let sqlite = include_str!("../../migrations/sqlite/0006_routine_invocations.sql");
    let postgres = include_str!("../../migrations/postgresql/0006_routine_invocations.sql");
    for (name, columns) in [
        (
            "routine_invocations_routine_admission_idx",
            "routine_name, admission_sequence DESC",
        ),
        (
            "routine_invocations_correlation_admission_idx",
            "correlation_id, admission_sequence DESC",
        ),
    ] {
        for migration in [sqlite, postgres] {
            assert!(migration.contains(&format!("CREATE INDEX {name}")));
            assert!(migration.contains(columns));
        }
    }
    for migration in [sqlite, postgres] {
        assert!(migration.contains("CREATE INDEX routine_invocation_metadata_exact_idx"));
        assert!(migration.contains("metadata_key, metadata_value, invocation_id"));
        assert!(migration.contains("PRIMARY KEY (invocation_id, metadata_key)"));
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
            "outbound_oauth_credentials",
            "routine_invocation_events",
            "routine_invocation_metadata",
            "routine_invocations"
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
            "mcp_oauth_oidc_resource_owner_attempts",
            "mcp_oauth_refresh_families",
            "mcp_oauth_refresh_tokens",
            "mcp_oauth_registration_rates",
            "mcp_oauth_revoked_clients",
            "mcp_oauth_signing_keys",
            "mcp_streamable_http_events",
            "mcp_streamable_http_sessions"
        ]
    );
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
#[allow(
    clippy::too_many_lines,
    reason = "migration contract test stages V4 and verifies every preserved and replaced field"
)]
async fn sqlite_v5_discards_credentials_preserves_attempts_and_reopens() {
    let database = TestDatabase::new();
    fs::create_dir_all(&database.root).expect("create test database directory");
    let legacy_migrations = database.root.join("legacy-migrations");
    fs::create_dir_all(&legacy_migrations).expect("create legacy migration directory");
    let migration_source = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/sqlite");
    for migration in [
        "0001_schema_ownership.sql",
        "0002_oidc_access_auth.sql",
        "0003_mcp_oauth_login_continuations.sql",
        "0004_outbound_oauth.sql",
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
    sqlx::query("INSERT INTO outbound_oauth_credentials (target_key, state_json, refresh_generation, refresh_claim_id, refresh_claim_expires_at_ms, reauthorization_required, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind("mcp:legacy")
        .bind(r#"{"version":1,"token":{"access_token":"legacy-secret"}}"#)
        .bind(7_i64)
        .bind("legacy-claim")
        .bind(12_345_i64)
        .bind(true)
        .bind(1_000_i64)
        .execute(&pool)
        .await
        .expect("insert V4 credential");
    sqlx::query("INSERT INTO outbound_oauth_attempts (attempt_id, state_digest, target_key, attempt_json, registration_method, completion_mode, status, completion_claim_id, completion_claim_expires_at_ms, failure_reason, expires_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind("preserved-attempt")
        .bind(vec![9_u8; 32])
        .bind("mcp:remote")
        .bind(r#"{"verifier":"preserve-me"}"#)
        .bind("dynamic")
        .bind("server_callback")
        .bind("completing")
        .bind("worker")
        .bind(20_000_i64)
        .bind(Option::<String>::None)
        .bind(30_000_i64)
        .bind(2_000_i64)
        .execute(&pool)
        .await
        .expect("insert V4 attempt");
    pool.close().await;

    let config = DatabaseConfig::from_url(&url).expect("create config");
    let upgraded = Persistence::initialize(config.clone())
        .await
        .expect("upgrade database");
    assert_component_histories(&upgraded).await;
    let pool = upgraded.sqlite_pool().expect("Waltr pool");
    let credential_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM outbound_oauth_credentials")
            .fetch_one(pool)
            .await
            .expect("count V5 credentials");
    assert_eq!(credential_count, 0);
    let attempt = sqlx::query_as::<_, (String, Vec<u8>, String, String, String, String, String, Option<String>, Option<i64>, Option<String>, i64, i64)>(
        "SELECT attempt_id, state_digest, target_key, attempt_json, registration_method, completion_mode, status, completion_claim_id, completion_claim_expires_at_ms, failure_reason, expires_at_ms, updated_at_ms FROM outbound_oauth_attempts WHERE attempt_id = ?",
    )
    .bind("preserved-attempt")
    .fetch_one(pool)
    .await
    .expect("read preserved attempt");
    assert_eq!(
        attempt,
        (
            "preserved-attempt".to_owned(),
            vec![9_u8; 32],
            "mcp:remote".to_owned(),
            r#"{"verifier":"preserve-me"}"#.to_owned(),
            "dynamic".to_owned(),
            "server_callback".to_owned(),
            "completing".to_owned(),
            Some("worker".to_owned()),
            Some(20_000),
            None,
            30_000,
            2_000,
        )
    );
    let columns = sqlx::query_scalar::<_, String>(
        "SELECT name FROM pragma_table_info('outbound_oauth_credentials')",
    )
    .fetch_all(pool)
    .await
    .expect("read V5 credential columns");
    assert!(columns.contains(&"credential_json".to_owned()));
    assert!(columns.contains(&"revision".to_owned()));
    assert!(!columns.contains(&"state_json".to_owned()));
    assert!(!columns.contains(&"refresh_generation".to_owned()));
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
    assert_eq!((waltr_count, mcp_count), (6, 4));
    assert!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT to_regclass('waltr.outbound_oauth_attempts')::text"
        )
        .fetch_one(pool)
        .await
        .expect("find outbound OAuth attempts")
        .is_some()
    );
    let routine_tables = sqlx::query_scalar::<_, String>(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'waltr' AND table_name LIKE 'routine_invocation%' ORDER BY table_name",
    )
    .fetch_all(pool)
    .await
    .expect("inventory Routine persistence tables");
    assert_eq!(
        routine_tables,
        [
            "routine_invocation_events",
            "routine_invocation_metadata",
            "routine_invocations"
        ]
    );
    let routine_indexes = sqlx::query_scalar::<_, String>(
        "SELECT indexname FROM pg_indexes WHERE schemaname = 'waltr' AND indexname LIKE 'routine_invocations_%_admission_idx' ORDER BY indexname",
    )
    .fetch_all(pool)
    .await
    .expect("inventory Routine diagnostic indexes");
    assert_eq!(
        routine_indexes,
        [
            "routine_invocations_correlation_admission_idx",
            "routine_invocations_routine_admission_idx",
            "routine_invocations_status_admission_idx",
        ]
    );
    let routine_store = first.routine_invocation_store();
    let (admitted, admitted_event) = routine_store
        .admit(routine_invocation::RoutineAdmission {
            invocation_id: "postgres-routine".to_owned(),
            execution_id: "shared-execution".to_owned(),
            routine_name: "postgres".to_owned(),
            routine_revision: "revision".to_owned(),
            trigger_name: "manual".to_owned(),
            trigger_kind: routine_invocation::RoutineTriggerKind::Manual,
            correlation_id: "shared-correlation".to_owned(),
            metadata: BTreeMap::new(),
            source_cursor: None,
            declared_tasks: Vec::new(),
            admitted_at_ms: 1,
        })
        .await
        .expect("admit PostgreSQL Routine invocation");
    assert_eq!(admitted.latest_sequence, admitted_event.sequence);
    assert_eq!(
        routine_store
            .list(None, 10, &BTreeMap::new())
            .await
            .expect("list PostgreSQL Routine invocations")[0]
            .invocation_id,
        "postgres-routine"
    );
    sqlx::query(
        "SELECT nextval(pg_get_serial_sequence('waltr.routine_invocation_events', 'sequence'))",
    )
    .execute(pool)
    .await
    .expect("create valid PostgreSQL identity gap");
    let started = routine_store
        .start_invocation("postgres-routine", 2)
        .await
        .expect("start PostgreSQL Routine invocation");
    assert!(started.sequence > admitted_event.sequence + 1);
    let postgres_finished = routine_store
        .finish_success(
            "postgres-routine",
            vec![
                routine_invocation::StoredRoutineResult {
                    name: "zeta".to_owned(),
                    value: routine_invocation::StoredRoutineValue::String("last".to_owned()),
                },
                routine_invocation::StoredRoutineResult {
                    name: "alpha".to_owned(),
                    value: routine_invocation::StoredRoutineValue::Boolean(true),
                },
            ],
            3,
        )
        .await
        .expect("finish PostgreSQL Routine invocation");
    assert_eq!(
        postgres_finished
            .values
            .expect("PostgreSQL invocation result values")
            .into_iter()
            .map(|result| result.name)
            .collect::<Vec<_>>(),
        ["zeta", "alpha"]
    );
    assert_eq!(
        routine_store
            .events_after(admitted_event.sequence, 10, &BTreeMap::new())
            .await
            .expect("replay PostgreSQL Routine events")[0]
            .sequence,
        started.sequence
    );
    let (interruption_summary, interruption_admitted) = routine_store
        .admit(routine_invocation::RoutineAdmission {
            invocation_id: "postgres-interruption".to_owned(),
            execution_id: "shared-execution".to_owned(),
            routine_name: "postgres".to_owned(),
            routine_revision: "revision".to_owned(),
            trigger_name: "event".to_owned(),
            trigger_kind: routine_invocation::RoutineTriggerKind::Event,
            correlation_id: "shared-correlation".to_owned(),
            metadata: BTreeMap::from([(
                routine_invocation::SOURCE_SESSION_METADATA_KEY.to_owned(),
                "source".to_owned(),
            )]),
            source_cursor: Some("cursor".to_owned()),
            declared_tasks: ["pending", "started", "terminal"]
                .into_iter()
                .map(|name| routine_invocation::RoutineTaskDescriptor {
                    name: name.to_owned(),
                    task_type: "test".to_owned(),
                })
                .collect(),
            admitted_at_ms: 4,
        })
        .await
        .expect("admit PostgreSQL interrupted Routine invocation");
    assert_ne!(
        interruption_summary.admission_sequence,
        interruption_admitted.sequence
    );
    assert_eq!(
        interruption_admitted
            .admission
            .as_ref()
            .expect("PostgreSQL admission metadata")
            .admission_sequence,
        interruption_summary.admission_sequence
    );
    routine_store
        .start_invocation("postgres-interruption", 5)
        .await
        .expect("start PostgreSQL interrupted Routine invocation");
    routine_store
        .transition_task(
            "postgres-interruption",
            "started",
            routine_invocation::RoutineTaskTransition::Started {
                session_references: BTreeMap::new(),
            },
            6,
        )
        .await
        .expect("start PostgreSQL Routine task");
    routine_store
        .transition_task(
            "postgres-interruption",
            "terminal",
            routine_invocation::RoutineTaskTransition::Skipped,
            7,
        )
        .await
        .expect("terminalize PostgreSQL Routine task");
    let interrupted = routine_store
        .finish_interruption(
            "postgres-interruption",
            routine_invocation::RoutineFailure {
                code: "restart".to_owned(),
                message: "safe".to_owned(),
            },
            8,
        )
        .await
        .expect("interrupt PostgreSQL Routine invocation");
    assert_eq!(
        interrupted
            .iter()
            .map(|event| (event.kind, event.task_name.as_deref()))
            .collect::<Vec<_>>(),
        [
            (
                routine_invocation::RoutineEventKind::TaskInterrupted,
                Some("pending")
            ),
            (
                routine_invocation::RoutineEventKind::TaskInterrupted,
                Some("started")
            ),
            (
                routine_invocation::RoutineEventKind::InvocationInterrupted,
                None
            ),
        ]
    );
    let postgres_detail = routine_store
        .detail("postgres-interruption")
        .await
        .expect("read consistent PostgreSQL Routine detail");
    assert_eq!(
        postgres_detail.events.last().unwrap().sequence,
        postgres_detail.summary.latest_sequence
    );
    assert_eq!(
        postgres_detail.events[0]
            .admission
            .as_ref()
            .unwrap()
            .admission_sequence,
        postgres_detail.summary.admission_sequence
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

#[tokio::test]
#[ignore = "requires Docker"]
async fn postgres_routine_event_sequences_follow_commit_order() {
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
    let persistence = Persistence::initialize(
        DatabaseConfig::from_url(&format!(
            "postgres://waltr:waltr@127.0.0.1:{port}/waltr?sslmode=disable"
        ))
        .expect("create config"),
    )
    .await
    .expect("initialize PostgreSQL");
    let pool = persistence.postgres_pool().expect("Waltr PostgreSQL pool");
    let store = persistence.routine_invocation_store();
    let admission = |invocation_id: &str, admitted_at_ms| routine_invocation::RoutineAdmission {
        invocation_id: invocation_id.to_owned(),
        execution_id: format!("execution-{invocation_id}"),
        routine_name: "commit-order".to_owned(),
        routine_revision: "revision".to_owned(),
        trigger_name: "manual".to_owned(),
        trigger_kind: routine_invocation::RoutineTriggerKind::Manual,
        correlation_id: format!("correlation-{invocation_id}"),
        metadata: BTreeMap::new(),
        source_cursor: None,
        declared_tasks: Vec::new(),
        admitted_at_ms,
    };

    store
        .admit(admission("allocator-a", 1))
        .await
        .expect("admit allocator A");
    let mut allocator_a = pool.begin().await.expect("begin allocator A");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(routine_invocation::ROUTINE_EVENT_SEQUENCE_LOCK_KEY)
        .execute(&mut *allocator_a)
        .await
        .expect("lock Routine event allocation");
    let sequence_a = sqlx::query_scalar::<_, i64>(
        "INSERT INTO waltr.routine_invocation_events (invocation_id, event_kind, occurred_at_ms) VALUES ('allocator-a', 'invocation_started', 2) RETURNING sequence",
    )
    .fetch_one(&mut *allocator_a)
    .await
    .expect("allocate A sequence");
    sqlx::query("UPDATE waltr.routine_invocations SET status = 'running', started_at_ms = 2, updated_at_ms = 2, latest_sequence = $1 WHERE invocation_id = 'allocator-a'")
        .bind(sequence_a)
        .execute(&mut *allocator_a)
        .await
        .expect("update allocator A aggregate");

    let blocked_store = store.clone();
    let mut allocator_b = tokio::spawn(async move {
        blocked_store
            .admit(admission("allocator-b", 3))
            .await
            .expect("admit allocator B")
            .1
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut allocator_b)
            .await
            .is_err(),
        "allocator B must wait for allocator A to commit"
    );
    let sequence_while_blocked = sqlx::query_scalar::<_, i64>(
        "SELECT last_value FROM waltr.routine_invocation_events_sequence_seq",
    )
    .fetch_one(pool)
    .await
    .expect("read event identity while B is blocked");
    assert_eq!(sequence_while_blocked, sequence_a);

    allocator_a.commit().await.expect("commit allocator A");
    let event_b = allocator_b.await.expect("join allocator B");
    assert!(event_b.sequence > sequence_a);
    let replay = store
        .events_after(sequence_a - 1, 10, &BTreeMap::new())
        .await
        .expect("replay commit-ordered events");
    assert_eq!(
        replay
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [sequence_a, event_b.sequence]
    );

    let mut lock_holder = pool.begin().await.expect("begin lifecycle lock holder");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(routine_invocation::ROUTINE_EVENT_SEQUENCE_LOCK_KEY)
        .execute(&mut *lock_holder)
        .await
        .expect("hold Routine event allocation lock");
    let sequence_before_lifecycle = sqlx::query_scalar::<_, i64>(
        "SELECT last_value FROM waltr.routine_invocation_events_sequence_seq",
    )
    .fetch_one(pool)
    .await
    .expect("read identity before lifecycle allocation");
    let lifecycle_store = store.clone();
    let mut lifecycle = tokio::spawn(async move {
        lifecycle_store
            .start_invocation("allocator-b", 4)
            .await
            .expect("start allocator B invocation")
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut lifecycle)
            .await
            .is_err(),
        "lifecycle allocation must wait for the transaction lock"
    );
    let sequence_while_lifecycle_blocked = sqlx::query_scalar::<_, i64>(
        "SELECT last_value FROM waltr.routine_invocation_events_sequence_seq",
    )
    .fetch_one(pool)
    .await
    .expect("read identity while lifecycle is blocked");
    assert_eq!(sequence_while_lifecycle_blocked, sequence_before_lifecycle);
    lock_holder.commit().await.expect("release lifecycle lock");
    let lifecycle_event = lifecycle.await.expect("join lifecycle allocator");
    assert!(lifecycle_event.sequence > sequence_before_lifecycle);
}

#[tokio::test]
#[ignore = "requires Docker"]
#[allow(
    clippy::too_many_lines,
    reason = "migration contract test stages V4 and verifies preserved attempts and replaced columns"
)]
async fn postgres_v5_discards_credentials_and_preserves_attempts() {
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
    let root = crate::test_support::temp_root();
    let legacy_migrations = root.join("postgres-v4-migrations");
    fs::create_dir_all(&legacy_migrations).expect("create migration directory");
    let migration_source = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgresql");
    for migration in [
        "0001_schema_ownership.sql",
        "0002_oidc_access_auth.sql",
        "0003_mcp_oauth_login_continuations.sql",
        "0004_outbound_oauth.sql",
    ] {
        fs::copy(
            migration_source.join(migration),
            legacy_migrations.join(migration),
        )
        .expect("copy committed migration");
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .expect("connect PostgreSQL");
    sqlx::query("CREATE SCHEMA IF NOT EXISTS waltr")
        .execute(&pool)
        .await
        .expect("create legacy Waltr schema");
    let mut migration_connection = pool.acquire().await.expect("acquire migration connection");
    sqlx::query("SET search_path TO waltr")
        .execute(&mut *migration_connection)
        .await
        .expect("select legacy Waltr schema");
    sqlx::migrate::Migrator::new(legacy_migrations)
        .await
        .expect("load V4 migrations")
        .run(&mut *migration_connection)
        .await
        .expect("run V4 migrations");
    drop(migration_connection);
    sqlx::query("INSERT INTO waltr.outbound_oauth_credentials (target_key, state_json, refresh_generation, updated_at_ms) VALUES ($1, $2, $3, $4)")
        .bind("provider:legacy")
        .bind(r#"{"access":"legacy-secret"}"#)
        .bind(4_i64)
        .bind(1_000_i64)
        .execute(&pool)
        .await
        .expect("insert V4 credential");
    sqlx::query("INSERT INTO waltr.outbound_oauth_attempts (attempt_id, state_digest, target_key, attempt_json, registration_method, completion_mode, status, expires_at_ms, updated_at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
        .bind("preserved-attempt")
        .bind(vec![7_u8; 32])
        .bind("mcp:remote")
        .bind(r#"{"verifier":"preserve-me"}"#)
        .bind("dynamic")
        .bind("server_callback")
        .bind("pending")
        .bind(30_000_i64)
        .bind(2_000_i64)
        .execute(&pool)
        .await
        .expect("insert V4 attempt");
    pool.close().await;

    let persistence =
        Persistence::initialize(DatabaseConfig::from_url(&url).expect("create PostgreSQL config"))
            .await
            .expect("apply V5 migration");
    let pool = persistence.postgres_pool().expect("Waltr PostgreSQL pool");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM waltr.outbound_oauth_credentials")
            .fetch_one(pool)
            .await
            .expect("count credentials"),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT attempt_json FROM waltr.outbound_oauth_attempts WHERE attempt_id = $1"
        )
        .bind("preserved-attempt")
        .fetch_one(pool)
        .await
        .expect("read preserved attempt"),
        r#"{"verifier":"preserve-me"}"#
    );
    let columns = sqlx::query_scalar::<_, String>("SELECT column_name FROM information_schema.columns WHERE table_schema = 'waltr' AND table_name = 'outbound_oauth_credentials'")
        .fetch_all(pool)
        .await
        .expect("read V5 columns");
    assert!(columns.contains(&"credential_json".to_owned()));
    assert!(columns.contains(&"revision".to_owned()));
    assert!(!columns.contains(&"state_json".to_owned()));
    assert!(!columns.contains(&"refresh_generation".to_owned()));
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
    assert_eq!((waltr_count, mcp_count), (6, 4));
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
