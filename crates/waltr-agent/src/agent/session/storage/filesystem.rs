//! Filesystem-backed session storage.

use std::{collections::BTreeMap, fs, path::PathBuf};

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

/// Local JSON-file implementation of [`SessionStore`].
#[derive(Debug, Clone)]
pub struct FilesystemSessionStore {
    root: PathBuf,
}

impl FilesystemSessionStore {
    /// Create a filesystem store rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{}.json", id.as_str()))
    }

    fn load_sync(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        let path = self.path(id);
        let bytes = fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SessionStoreError::NotFound { id: id.clone() }
            } else {
                backend_error(error)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(backend_error)
    }

    fn save_sync(&self, record: &SessionRecord) -> SessionStoreResult<()> {
        fs::create_dir_all(&self.root).map_err(backend_error)?;
        let path = self.path(&record.id);
        let temp_path = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(record).map_err(backend_error)?;
        fs::write(&temp_path, bytes).map_err(backend_error)?;
        fs::rename(&temp_path, path).map_err(backend_error)?;
        Ok(())
    }

    fn list_sync(&self) -> SessionStoreResult<Vec<SessionSummary>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(backend_error(error)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(backend_error)?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path).map_err(backend_error)?;
            records.push(serde_json::from_slice::<SessionRecord>(&bytes).map_err(backend_error)?);
        }
        Ok(sorted_session_summaries(records))
    }

    fn update_sync<T>(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl FnOnce(&mut SessionRecord, Revision) -> SessionStoreResult<T>,
    ) -> SessionStoreResult<(Revision, T)> {
        let mut record = self.load_sync(id)?;
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
        self.save_sync(&record)?;
        Ok((revision, value))
    }

    fn update_metadata_sync(
        &self,
        id: &SessionId,
        update: impl FnOnce(&mut SessionRecord),
    ) -> SessionStoreResult<SessionRecord> {
        let mut record = self.load_sync(id)?;
        update(&mut record);
        self.save_sync(&record)?;
        Ok(record)
    }
}

impl SessionStore for FilesystemSessionStore {
    fn create(
        &self,
        id: SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = SessionRecord::new(id);
            if store.path(&record.id).exists() {
                return Err(SessionStoreError::AlreadyExists { id: record.id });
            }
            store.save_sync(&record)?;
            Ok(record)
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
            if store.path(&record.id).exists() {
                return Err(SessionStoreError::AlreadyExists { id: record.id });
            }
            store.save_sync(&record)?;
            Ok(record)
        }
    }

    fn load(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move { store.load_sync(&id) }
    }

    fn delete(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            fs::remove_file(store.path(&id)).or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(backend_error(error))
                }
            })
        }
    }

    fn list(
        &self,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<SessionSummary>>> + Send {
        let store = self.clone();
        async move { store.list_sync() }
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
            let (_, assigned) = store.update_sync(&id, expected_revision, |record, _| {
                append_timeline_to_record(record, &id, events)
            })?;
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
            let (revision, ()) = store.update_sync(&id, expected_revision, |record, _| {
                record.append_journal_events(events);
                record.touch_activity();
                Ok(())
            })?;
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
        async move { store.update_metadata_sync(&id, |record| record.set_metadata_value(key, value)) }
    }

    fn remove_metadata_value(
        &self,
        id: &SessionId,
        key: String,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store.update_metadata_sync(&id, |record| {
                record.remove_metadata_value(&key);
            })
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
            let record = store.load_sync(&id)?;
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
            let record = store.load_sync(&id)?;
            timeline_item_text_from_record(&record, &item_id)
        }
    }
}

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_missing_root_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path().join("missing"));

        let summaries = store.list().await.unwrap();

        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path());
        let id = SessionId::new("session-delete");

        store.create(id.clone()).await.unwrap();
        store.delete(&id).await.unwrap();
        store.delete(&id).await.unwrap();
        store
            .delete(&SessionId::new("never-created"))
            .await
            .unwrap();

        let error = store.load(&id).await.unwrap_err();
        assert_eq!(error, SessionStoreError::NotFound { id });
    }

    #[tokio::test]
    async fn list_returns_persisted_summaries_newest_activity_first() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path());
        let first = SessionId::new("session-first");
        let second = SessionId::new("session-second");
        let first_record = store.create(first.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        store.create(second.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        store
            .append_journal(
                &first,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "again".to_owned(),
                }],
            )
            .await
            .unwrap();

        let reloaded_store = FilesystemSessionStore::new(temp.path());
        let summaries = reloaded_store.list().await.unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, first);
        assert_eq!(summaries[0].revision, Revision::new(1));
        assert_eq!(
            summaries[0].status,
            crate::agent::session::SessionStatus::BuildingRequest
        );
        assert_eq!(summaries[0].current_prompt.as_deref(), Some("again"));
        assert_eq!(summaries[0].created_at, first_record.metadata.created_at);
        assert!(summaries[0].last_activity_at > first_record.metadata.last_activity_at);
        assert_eq!(summaries[1].id, second);
    }
}
