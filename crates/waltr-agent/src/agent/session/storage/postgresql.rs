//! PostgreSQL setup for OpenDAL-backed session storage.

use opendal::{Operator, services};

use super::{OpenDalSessionStore, SessionStoreError, SessionStoreResult};

const SESSION_TABLE: &str = "waltr_sessions";

mod embedded {
    refinery::embed_migrations!("migrations/postgresql");
}

/// Run forward-only PostgreSQL session storage migrations.
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

/// Create an OpenDAL PostgreSQL session store after running required migrations.
pub async fn opendal_postgresql_session_store(
    connection_string: &str,
    prefix: impl Into<String>,
) -> SessionStoreResult<OpenDalSessionStore> {
    run_postgresql_migrations(connection_string).await?;
    let builder = services::Postgresql::default()
        .connection_string(connection_string)
        .table(SESSION_TABLE)
        .root("/");
    let operator = Operator::new(builder).map_err(backend_error)?.finish();
    Ok(OpenDalSessionStore::new(operator, prefix))
}

fn backend_error(error: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
