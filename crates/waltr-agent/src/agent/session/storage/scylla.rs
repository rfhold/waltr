//! Scylla-backed session storage.

use std::sync::Arc;

use scylla::client::session::Session;

use super::{SessionStore, SessionStoreError, SessionStoreResult};
use crate::agent::session::{
    ExternalRequest, RequestRecord, Revision, SessionEvent, SessionId, SessionRecord,
    SessionSnapshot,
};

/// Scylla JSON-row implementation of [`SessionStore`].
#[derive(Debug, Clone)]
pub struct ScyllaSessionStore {
    session: Arc<Session>,
    keyspace: String,
    table: String,
}

impl ScyllaSessionStore {
    /// Create a Scylla-backed store using an existing session and table.
    #[must_use]
    pub fn new(
        session: Arc<Session>,
        keyspace: impl Into<String>,
        table: impl Into<String>,
    ) -> Self {
        Self {
            session,
            keyspace: keyspace.into(),
            table: table.into(),
        }
    }

    /// Create the table used by this store if it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema statement fails.
    pub async fn create_schema(&self) -> SessionStoreResult<()> {
        self.session
            .query_unpaged(
                format!(
                    "CREATE TABLE IF NOT EXISTS {} (session_id text PRIMARY KEY, revision bigint, record_json text)",
                    self.table_name()
                ),
                (),
            )
            .await
            .map_err(backend_error)?;
        Ok(())
    }

    fn table_name(&self) -> String {
        format!(
            "{}.{}",
            quote_identifier(&self.keyspace),
            quote_identifier(&self.table)
        )
    }

    async fn load_record(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        let rows = self
            .session
            .query_unpaged(
                format!(
                    "SELECT record_json, revision FROM {} WHERE session_id = ?",
                    self.table_name()
                ),
                (id.as_str(),),
            )
            .await
            .map_err(backend_error)?
            .into_rows_result()
            .map_err(backend_error)?;
        let Some((json, revision)): Option<(String, i64)> =
            rows.maybe_first_row().map_err(backend_error)?
        else {
            return Err(SessionStoreError::NotFound { id: id.clone() });
        };
        let mut record: SessionRecord = serde_json::from_str(&json).map_err(backend_error)?;
        record.revision = Revision::new(u64::try_from(revision).map_err(backend_error)?);
        Ok(record)
    }

    async fn save_record(&self, record: &SessionRecord) -> SessionStoreResult<()> {
        let json = serde_json::to_string(record).map_err(backend_error)?;
        let revision = i64::try_from(record.revision.get()).map_err(backend_error)?;
        self.session
            .query_unpaged(
                format!(
                    "INSERT INTO {} (session_id, revision, record_json) VALUES (?, ?, ?)",
                    self.table_name()
                ),
                (record.id.as_str(), revision, json),
            )
            .await
            .map_err(backend_error)?;
        Ok(())
    }

    async fn update_record(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl FnOnce(&mut SessionRecord, Revision),
    ) -> SessionStoreResult<Revision> {
        let mut record = self.load_record(id).await?;
        if record.revision != expected_revision {
            return Err(SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: expected_revision,
                actual: record.revision,
            });
        }
        let revision = record.revision.next();
        update(&mut record, revision);
        record.revision = revision;
        self.save_record(&record).await?;
        Ok(revision)
    }
}

impl SessionStore for ScyllaSessionStore {
    fn create(
        &self,
        record: SessionRecord,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move {
            match store.load_record(&record.id).await {
                Ok(_) => Err(SessionStoreError::AlreadyExists { id: record.id }),
                Err(SessionStoreError::NotFound { .. }) => store.save_record(&record).await,
                Err(error) => Err(error),
            }
        }
    }

    fn load(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move { store.load_record(&id).await }
    }

    fn append(
        &self,
        id: &SessionId,
        events: Vec<SessionEvent>,
        expected_revision: Revision,
    ) -> impl std::future::Future<Output = SessionStoreResult<Revision>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_record(&id, expected_revision, |record, _| {
                    record.events.extend(events);
                })
                .await
        }
    }

    fn save_snapshot(
        &self,
        id: &SessionId,
        mut snapshot: SessionSnapshot,
        expected_revision: Revision,
    ) -> impl std::future::Future<Output = SessionStoreResult<Revision>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            if snapshot.id != id {
                return Err(SessionStoreError::SessionIdMismatch {
                    expected: id,
                    actual: snapshot.id,
                });
            }
            store
                .update_record(&id, expected_revision, |record, revision| {
                    snapshot.revision = revision;
                    record.snapshot = snapshot;
                })
                .await
        }
    }

    fn record_request(
        &self,
        id: &SessionId,
        request: RequestRecord,
        expected_revision: Revision,
    ) -> impl std::future::Future<Output = SessionStoreResult<Revision>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_record(&id, expected_revision, |record, _| {
                    record.requests.push(request);
                })
                .await
        }
    }

    fn record_external_request(
        &self,
        id: &SessionId,
        request: ExternalRequest,
        expected_revision: Revision,
    ) -> impl std::future::Future<Output = SessionStoreResult<Revision>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_record(&id, expected_revision, |record, revision| {
                    record.snapshot.external_requests.push(request);
                    record.snapshot.revision = revision;
                })
                .await
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn backend_error(error: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
