use sqlx::{PgPool, SqlitePool, migrate::Migrate as _};

use super::{DatabaseBackend, PersistenceError};

static SQLITE_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/sqlite");
static POSTGRESQL_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/postgresql");

pub(super) async fn sqlite(pool: &SqlitePool) -> Result<(), PersistenceError> {
    sqlite_object_preflight(pool).await?;
    if SQLITE_MIGRATOR.run(pool).await.is_err() {
        sqlite_object_preflight(pool).await?;
        return Err(PersistenceError::Migration(DatabaseBackend::Sqlite));
    }
    Ok(())
}

pub(super) async fn postgresql(pool: &PgPool) -> Result<(), PersistenceError> {
    let mut connection = pool
        .acquire()
        .await
        .map_err(|_| PersistenceError::Connection(DatabaseBackend::PostgreSql))?;
    connection.close_on_drop();

    if connection.lock().await.is_err()
        || sqlx::query("CREATE SCHEMA IF NOT EXISTS waltr")
            .execute(&mut *connection)
            .await
            .is_err()
    {
        return Err(PersistenceError::Migration(DatabaseBackend::PostgreSql));
    }
    if sqlx::query("SET search_path TO waltr")
        .execute(&mut *connection)
        .await
        .is_err()
    {
        return Err(PersistenceError::Migration(DatabaseBackend::PostgreSql));
    }
    let old: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='waltr' AND table_name='script_program_revisions' AND column_name='envelope')")
        .fetch_one(&mut *connection).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::PostgreSql))?;
    if old {
        let populated: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM waltr.script_program_revisions)")
            .fetch_one(&mut *connection).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::PostgreSql))?;
        if populated { return Err(PersistenceError::IncompatibleRoutineState); }
    }
    let legacy: Option<String> = sqlx::query_scalar("SELECT to_regclass('waltr.routine_invocations')::text")
        .fetch_one(&mut *connection).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::PostgreSql))?;
    if legacy.is_some() {
        let populated: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM waltr.routine_invocations)")
            .fetch_one(&mut *connection).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::PostgreSql))?;
        if populated { return Err(PersistenceError::IncompatibleRoutineState); }
    }
    if POSTGRESQL_MIGRATOR.run(&mut *connection).await.is_err() {
        return Err(PersistenceError::Migration(DatabaseBackend::PostgreSql));
    }
    if sqlx::query("RESET search_path")
        .execute(&mut *connection)
        .await
        .is_err()
    {
        return Err(PersistenceError::Migration(DatabaseBackend::PostgreSql));
    }
    if connection.unlock().await.is_err() {
        return Err(PersistenceError::Migration(DatabaseBackend::PostgreSql));
    }

    Ok(())
}

async fn sqlite_object_preflight(pool: &SqlitePool) -> Result<(), PersistenceError> {
    let legacy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='routine_invocations'")
        .fetch_one(pool).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::Sqlite))?;
    if legacy != 0 {
        let populated: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routine_invocations")
            .fetch_one(pool).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::Sqlite))?;
        if populated != 0 { return Err(PersistenceError::IncompatibleRoutineState); }
    }
    let columns: Vec<(i64, String, String, i64, Option<String>, i64)> = sqlx::query_as("PRAGMA table_info(script_program_revisions)")
        .fetch_all(pool).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::Sqlite))?;
    if columns.iter().any(|column| column.1 == "envelope") {
        let populated: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM script_program_revisions")
            .fetch_one(pool).await.map_err(|_| PersistenceError::Migration(DatabaseBackend::Sqlite))?;
        if populated != 0 { return Err(PersistenceError::IncompatibleRoutineState); }
    }
    Ok(())
}
