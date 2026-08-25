//! `PostgreSQL` setup for `OpenDAL`-backed session storage.

use std::sync::Arc;

use opendal::{Operator, services};

use super::{
    OpenDalSessionStore, SessionStoreError, SessionStoreResult, opendal::OpenDalCompareAndSwap,
};

const SESSION_TABLE: &str = "waltr_sessions";

struct PostgresqlCompareAndSwap {
    client: tokio_postgres::Client,
}

impl OpenDalCompareAndSwap for PostgresqlCompareAndSwap {
    fn compare_exchange<'a>(
        &'a self,
        key: &'a str,
        expected: &'a [u8],
        replacement: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SessionStoreResult<bool>> + Send + 'a>>
    {
        Box::pin(async move {
            let updated = self
                .client
                .execute(
                    "UPDATE waltr_sessions SET value = $1 WHERE key = $2 AND value = $3",
                    &[&replacement, &key, &expected],
                )
                .await
                .map_err(backend_error)?;
            Ok(updated == 1)
        })
    }
}

mod embedded {
    refinery::embed_migrations!("migrations/postgresql");
}

/// Run forward-only `PostgreSQL` session storage migrations.
///
/// # Errors
///
/// Returns an error when the connection or a migration fails.
pub async fn run_postgresql_migrations(connection_string: &str) -> SessionStoreResult<()> {
    let (mut client, connection) =
        tokio_postgres::connect(connection_string, tokio_postgres::NoTls)
            .await
            .map_err(backend_error)?;
    tokio::spawn(async move {
        drop(connection.await);
    });

    embedded::migrations::runner()
        .run_async(&mut client)
        .await
        .map_err(backend_error)?;
    Ok(())
}

/// Create an `OpenDAL` `PostgreSQL` session store after running required migrations.
///
/// # Errors
///
/// Returns an error when migrations fail or the backend cannot be constructed.
pub async fn opendal_postgresql_session_store(
    connection_string: &str,
    prefix: impl Into<String>,
) -> SessionStoreResult<OpenDalSessionStore> {
    run_postgresql_migrations(connection_string).await?;
    let (client, connection) = tokio_postgres::connect(connection_string, tokio_postgres::NoTls)
        .await
        .map_err(backend_error)?;
    tokio::spawn(async move {
        drop(connection.await);
    });
    let builder = services::Postgresql::default()
        .connection_string(connection_string)
        .table(SESSION_TABLE)
        .root("/");
    let operator = Operator::new(builder).map_err(backend_error)?.finish();
    Ok(OpenDalSessionStore::new(operator, prefix)
        .with_compare_and_swap(Arc::new(PostgresqlCompareAndSwap { client })))
}

fn backend_error(error: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
