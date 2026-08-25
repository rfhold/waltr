//! Scylla-backed session storage.

use std::{collections::BTreeMap, sync::Arc};

use scylla::client::session::Session;
use scylla::value::CqlValue;

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    sorted_session_summaries, timeline_item_text_from_record, timeline_query_slice,
};
use crate::{
    agent::session::{
        InferenceSettings, Revision, SessionCreateRequest, SessionId, SessionJournalEvent,
        SessionRecord, SessionSummary,
    },
    definition::AgentDefinitionBundle,
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

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

    async fn list_records(&self) -> SessionStoreResult<Vec<SessionSummary>> {
        let rows = self
            .session
            .query_unpaged(
                format!("SELECT record_json, revision FROM {}", self.table_name()),
                (),
            )
            .await
            .map_err(backend_error)?
            .into_rows_result()
            .map_err(backend_error)?;
        let mut records = Vec::new();
        for row in rows.rows::<(String, i64)>().map_err(backend_error)? {
            let (json, revision) = row.map_err(backend_error)?;
            let mut record: SessionRecord = serde_json::from_str(&json).map_err(backend_error)?;
            record.revision = Revision::new(u64::try_from(revision).map_err(backend_error)?);
            records.push(record);
        }
        Ok(sorted_session_summaries(records))
    }

    async fn update_record<T: Send>(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl Fn(&mut SessionRecord, Revision) -> SessionStoreResult<T> + Send,
    ) -> SessionStoreResult<(Revision, T)> {
        loop {
            let mut record = self.load_record(id).await?;
            if record.revision != expected_revision {
                return Err(SessionStoreError::StaleRevision {
                    id: id.clone(),
                    expected: expected_revision,
                    actual: record.revision,
                });
            }
            let original_json = serde_json::to_string(&record).map_err(backend_error)?;
            let revision = record.revision.next();
            record.revision = revision;
            let value = update(&mut record, revision)?;
            let json = serde_json::to_string(&record).map_err(backend_error)?;
            let expected = i64::try_from(expected_revision.get()).map_err(backend_error)?;
            let next = i64::try_from(revision.get()).map_err(backend_error)?;
            let applied: (bool, Option<CqlValue>, Option<CqlValue>) = self.session.query_unpaged(
                format!("UPDATE {} SET revision = ?, record_json = ? WHERE session_id = ? IF revision = ? AND record_json = ?", self.table_name()),
                (next, json, id.as_str(), expected, original_json),
            ).await.map_err(backend_error)?.into_rows_result().map_err(backend_error)?
                .single_row().map_err(backend_error)?;
            if applied.0 {
                return Ok((revision, value));
            }
        }
    }

    async fn update_metadata_record(
        &self,
        id: &SessionId,
        update: impl Fn(&mut SessionRecord) + Send,
    ) -> SessionStoreResult<SessionRecord> {
        loop {
            let mut record = self.load_record(id).await?;
            let revision = record.revision;
            let original_json = serde_json::to_string(&record).map_err(backend_error)?;
            update(&mut record);
            let json = serde_json::to_string(&record).map_err(backend_error)?;
            let revision_i64 = i64::try_from(revision.get()).map_err(backend_error)?;
            let applied: (bool, Option<CqlValue>, Option<CqlValue>) = self.session.query_unpaged(
                format!("UPDATE {} SET record_json = ? WHERE session_id = ? IF revision = ? AND record_json = ?", self.table_name()),
                (json, id.as_str(), revision_i64, original_json),
            ).await.map_err(backend_error)?.into_rows_result().map_err(backend_error)?
                .single_row().map_err(backend_error)?;
            if applied.0 {
                return Ok(record);
            }
        }
    }
}

impl SessionStore for ScyllaSessionStore {
    fn create(
        &self,
        request: SessionCreateRequest,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = request.materialize();
            match store.load_record(&record.id).await {
                Ok(_) => Err(SessionStoreError::AlreadyExists { id: record.id }),
                Err(SessionStoreError::NotFound { .. }) => {
                    store.save_record(&record).await?;
                    Ok(record)
                }
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

    fn delete(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .session
                .query_unpaged(
                    format!("DELETE FROM {} WHERE session_id = ?", store.table_name()),
                    (id.as_str(),),
                )
                .await
                .map_err(backend_error)?;
            Ok(())
        }
    }

    fn wipe(&self) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move {
            store
                .session
                .query_unpaged(format!("TRUNCATE {}", store.table_name()), ())
                .await
                .map_err(backend_error)?;
            Ok(())
        }
    }

    fn list(
        &self,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<SessionSummary>>> + Send {
        let store = self.clone();
        async move { store.list_records().await }
    }

    fn append_timeline(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        events: Vec<PendingTimelineEvent>,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let (_, assigned) = store
                .update_record(&id, expected_revision, |record, _| {
                    append_timeline_to_record(record, &id, events.clone())
                })
                .await?;
            Ok(assigned)
        }
    }

    fn append_journal(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        events: Vec<SessionJournalEvent>,
    ) -> impl std::future::Future<Output = SessionStoreResult<Revision>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let (revision, ()) = store
                .update_record(&id, expected_revision, |record, _| {
                    record.append_journal_events(events.clone());
                    record.touch_activity();
                    Ok(())
                })
                .await?;
            Ok(revision)
        }
    }

    fn set_metadata_value(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_metadata_record(&id, |record| {
                    record.set_metadata_value(key.clone(), value.clone());
                })
                .await
        }
    }

    fn remove_metadata_value(
        &self,
        id: &SessionId,
        key: String,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_metadata_record(&id, |record| {
                    record.remove_metadata_value(&key);
                })
                .await
        }
    }

    fn set_inference_settings(
        &self,
        id: &SessionId,
        inference_settings: Option<InferenceSettings>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_metadata_record(&id, |record| {
                    record.set_inference_settings(inference_settings.clone());
                })
                .await
        }
    }

    fn set_agent_definition(
        &self,
        id: &SessionId,
        agent_definition: Option<AgentDefinitionBundle>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_metadata_record(&id, |record| {
                    record.set_agent_definition(agent_definition.clone());
                })
                .await
        }
    }

    fn timeline(
        &self,
        id: &SessionId,
        query: TimelineQuery,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let record = store.load_record(&id).await?;
            Ok(timeline_query_slice(&record, &query))
        }
    }

    fn timeline_item_text(
        &self,
        id: &SessionId,
        item_id: &TimelineItemId,
    ) -> impl std::future::Future<Output = SessionStoreResult<TimelineItemText>> + Send {
        let store = self.clone();
        let id = id.clone();
        let item_id = item_id.clone();
        async move {
            let record = store.load_record(&id).await?;
            timeline_item_text_from_record(&record, &item_id)
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
