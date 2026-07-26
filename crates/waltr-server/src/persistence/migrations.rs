use sqlx::{PgPool, SqlitePool, migrate::Migrate as _};

use super::{DatabaseBackend, PersistenceError};

static SQLITE_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/sqlite");
static POSTGRESQL_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/postgresql");

pub(super) async fn sqlite(pool: &SqlitePool) -> Result<(), PersistenceError> {
    SQLITE_MIGRATOR
        .run(pool)
        .await
        .map_err(|_| PersistenceError::Migration(DatabaseBackend::Sqlite))
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
