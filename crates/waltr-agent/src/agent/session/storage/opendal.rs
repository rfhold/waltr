//! OpenDAL-backed session storage.

use std::collections::BTreeMap;

use opendal::Operator;

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    sorted_session_summaries, timeline_item_text_from_record, timeline_query_slice,
};
use crate::{
    agent::session::{Revision, SessionId, SessionJournalEvent, SessionRecord, SessionSummary},
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

/// Generic `OpenDAL` implementation of [`SessionStore`].
#[derive(Debug, Clone)]
pub struct OpenDalSessionStore {
    operator: Operator,
    prefix: String,
}

impl OpenDalSessionStore {
    /// Create an OpenDAL-backed store using `operator` and key `prefix`.
    #[must_use]
    pub fn new(operator: Operator, prefix: impl Into<String>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Self { operator, prefix }
    }

    fn key(&self, id: &SessionId) -> String {
        format!("{}{}.json", self.prefix, id.as_str())
    }

    async fn load_record(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        let key = self.key(id);
        let bytes = self.operator.read(&key).await.map_err(|error| {
            if error.kind() == opendal::ErrorKind::NotFound {
                SessionStoreError::NotFound { id: id.clone() }
            } else {
                backend_error(error)
            }
        })?;
        serde_json::from_slice(bytes.to_bytes().as_ref()).map_err(backend_error)
    }

    async fn save_record(&self, record: &SessionRecord) -> SessionStoreResult<()> {
        let key = self.key(&record.id);
        let bytes = serde_json::to_vec_pretty(record).map_err(backend_error)?;
        self.operator
            .write(&key, bytes)
            .await
            .map_err(backend_error)?;
        Ok(())
    }

    async fn list_records(&self) -> SessionStoreResult<Vec<SessionSummary>> {
        let entries = self
            .operator
            .list(&self.prefix)
            .await
            .map_err(backend_error)?;
        let mut records = Vec::new();
        for entry in entries {
            let path = entry.path();
            if !path
                .rsplit_once('.')
                .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("json"))
            {
                continue;
            }
            let bytes = self.operator.read(path).await.map_err(backend_error)?;
            records.push(serde_json::from_slice(bytes.to_bytes().as_ref()).map_err(backend_error)?);
        }
        Ok(sorted_session_summaries(records))
    }

    async fn update_record<T: Send>(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl FnOnce(&mut SessionRecord, Revision) -> SessionStoreResult<T> + Send,
    ) -> SessionStoreResult<(Revision, T)> {
        let mut record = self.load_record(id).await?;
        if record.revision != expected_revision {
            return Err(SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: expected_revision,
                actual: record.revision,
            });
        }
        let revision = record.revision.next();
        record.revision = revision;
        let value = update(&mut record, revision)?;
        self.save_record(&record).await?;
        Ok((revision, value))
    }

    async fn update_metadata_record(
        &self,
        id: &SessionId,
        update: impl FnOnce(&mut SessionRecord) + Send,
    ) -> SessionStoreResult<SessionRecord> {
        let mut record = self.load_record(id).await?;
        update(&mut record);
        self.save_record(&record).await?;
        Ok(record)
    }
}

impl SessionStore for OpenDalSessionStore {
    fn create(
        &self,
        id: SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = SessionRecord::new(id);
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

    fn create_with_metadata_values(
        &self,
        id: SessionId,
        metadata_values: BTreeMap<String, Value>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = SessionRecord::with_metadata_values(id, metadata_values);
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
            match store.operator.delete(&store.key(&id)).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == opendal::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(backend_error(error)),
            }
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
                    append_timeline_to_record(record, &id, events)
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
                    record.append_journal_events(events);
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
                .update_metadata_record(&id, |record| record.set_metadata_value(key, value))
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

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
