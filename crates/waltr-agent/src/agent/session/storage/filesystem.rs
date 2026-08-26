//! Filesystem-backed session storage.

use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    mutate_masks_on_record, sorted_session_summaries, timeline_item_text_from_record,
    timeline_query_slice,
};
use crate::{
    agent::session::{
        InferenceSettings, MetadataSetCondition, MetadataSetOutcome, Revision,
        SessionCreateRequest, SessionId, SessionJournalEvent, SessionMaskMutation,
        SessionMaskMutationOutcome, SessionRecord, SessionSummary,
    },
    definition::AgentDefinitionBundle,
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

const LOCK_DIRECTORY: &str = ".waltr-session-locks";
const STORE_LOCK_FILE: &str = "store.lock";

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

    fn lock_file(&self, name: &str) -> SessionStoreResult<File> {
        let directory = self.root.join(LOCK_DIRECTORY);
        fs::create_dir_all(&directory).map_err(backend_error)?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(name))
            .map_err(backend_error)
    }

    fn with_session_lock<T>(
        &self,
        id: &SessionId,
        operation: impl FnOnce() -> SessionStoreResult<T>,
    ) -> SessionStoreResult<T> {
        let store_lock = self.lock_file(STORE_LOCK_FILE)?;
        store_lock.lock_shared().map_err(backend_error)?;
        let session_lock = self.lock_file(&format!("{}.lock", id.as_str()))?;
        session_lock.lock().map_err(backend_error)?;
        operation()
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(Self) -> SessionStoreResult<T> + Send + 'static,
    ) -> SessionStoreResult<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || operation(store))
            .await
            .map_err(backend_error)?
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
        let temp_path = self.root.join(format!(
            ".{}.{}.tmp",
            record.id.as_str(),
            uuid::Uuid::now_v7()
        ));
        let bytes = serde_json::to_vec_pretty(record).map_err(backend_error)?;
        let result = fs::write(&temp_path, bytes)
            .and_then(|()| fs::rename(&temp_path, path))
            .map_err(backend_error);
        if result.is_err() {
            drop(fs::remove_file(temp_path));
        }
        result
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
        self.with_session_lock(id, || {
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
        })
    }

    fn update_metadata_sync(
        &self,
        id: &SessionId,
        update: impl FnOnce(&mut SessionRecord),
    ) -> SessionStoreResult<SessionRecord> {
        self.with_session_lock(id, || {
            let mut record = self.load_sync(id)?;
            update(&mut record);
            self.save_sync(&record)?;
            Ok(record)
        })
    }

    fn set_metadata_conditionally_sync(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
        condition: MetadataSetCondition,
    ) -> SessionStoreResult<MetadataSetOutcome> {
        self.with_session_lock(id, || {
            let mut record = self.load_sync(id)?;
            let changed = condition == MetadataSetCondition::Unconditional
                || !record.metadata.values.contains_key(&key);
            if changed {
                record.set_metadata_value(key, value);
                self.save_sync(&record)?;
            }
            Ok(MetadataSetOutcome { record, changed })
        })
    }

    fn wipe_sync(&self) -> SessionStoreResult<()> {
        fs::create_dir_all(&self.root).map_err(backend_error)?;
        let store_lock = self.lock_file(STORE_LOCK_FILE)?;
        store_lock.lock().map_err(backend_error)?;
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(backend_error(error)),
        };
        for entry in entries {
            let path = entry.map_err(backend_error)?.path();
            if path == self.root.join(LOCK_DIRECTORY) {
                continue;
            }
            if path.is_dir() {
                fs::remove_dir_all(path).map_err(backend_error)?;
            } else {
                fs::remove_file(path).map_err(backend_error)?;
            }
        }
        Ok(())
    }
}

impl SessionStore for FilesystemSessionStore {
    fn create(
        &self,
        request: SessionCreateRequest,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = request.materialize();
            store
                .blocking(move |store| {
                    let id = record.id.clone();
                    store.with_session_lock(&id, || {
                        if store.path(&id).exists() {
                            return Err(SessionStoreError::AlreadyExists { id: id.clone() });
                        }
                        store.save_sync(&record)?;
                        Ok(record)
                    })
                })
                .await
        }
    }

    fn load(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move { store.blocking(move |store| store.load_sync(&id)).await }
    }

    fn delete(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .blocking(move |store| {
                    store.with_session_lock(&id, || {
                        fs::remove_file(store.path(&id)).or_else(|error| {
                            if error.kind() == std::io::ErrorKind::NotFound {
                                Ok(())
                            } else {
                                Err(backend_error(error))
                            }
                        })
                    })
                })
                .await
        }
    }

    fn wipe(&self) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move { store.blocking(|store| store.wipe_sync()).await }
    }

    fn list(
        &self,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<SessionSummary>>> + Send {
        let store = self.clone();
        async move { store.blocking(|store| store.list_sync()).await }
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
            store
                .blocking(move |store| {
                    let (_, assigned) =
                        store.update_sync(&id, expected_revision, |record, _| {
                            append_timeline_to_record(record, &id, events)
                        })?;
                    Ok(assigned)
                })
                .await
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
            store
                .blocking(move |store| {
                    let (revision, ()) =
                        store.update_sync(&id, expected_revision, |record, _| {
                            record.append_journal_events(events);
                            record.touch_activity();
                            Ok(())
                        })?;
                    Ok(revision)
                })
                .await
        }
    }

    fn mutate_masks(
        &self,
        id: &SessionId,
        mutation: SessionMaskMutation,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionMaskMutationOutcome>> + Send
    {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .blocking(move |store| {
                    store.with_session_lock(&id, || {
                        let mut record = store.load_sync(&id)?;
                        let revision = record.revision.next();
                        record.revision = revision;
                        let outcome = mutate_masks_on_record(&mut record, revision, &mutation)?;
                        store.save_sync(&record)?;
                        Ok(outcome)
                    })
                })
                .await
        }
    }

    fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
        condition: MetadataSetCondition,
    ) -> impl std::future::Future<Output = SessionStoreResult<MetadataSetOutcome>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .blocking(move |store| {
                    store.set_metadata_conditionally_sync(&id, key, value, condition)
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
                .blocking(move |store| {
                    store.update_metadata_sync(&id, |record| {
                        record.remove_metadata_value(&key);
                    })
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
                .blocking(move |store| {
                    store.update_metadata_sync(&id, |record| {
                        record.set_inference_settings(inference_settings);
                    })
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
                .blocking(move |store| {
                    store.update_metadata_sync(&id, |record| {
                        record.set_agent_definition(agent_definition);
                    })
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
            store
                .blocking(move |store| {
                    let record = store.load_sync(&id)?;
                    Ok(timeline_query_slice(&record, &query))
                })
                .await
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
            store
                .blocking(move |store| {
                    let record = store.load_sync(&id)?;
                    timeline_item_text_from_record(&record, &item_id)
                })
                .await
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
    async fn replacement_masks_follow_store_contract() {
        let temp = tempfile::tempdir().unwrap();
        super::super::assert_mask_store_contract(
            FilesystemSessionStore::new(temp.path()),
            "filesystem",
        )
        .await;
    }

    #[tokio::test]
    async fn list_missing_root_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path().join("missing"));

        let summaries = store.list().await.unwrap();

        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn independently_constructed_stores_serialize_append_and_metadata_update() {
        let temp = tempfile::tempdir().unwrap();
        let first = FilesystemSessionStore::new(temp.path());
        let second = FilesystemSessionStore::new(temp.path());
        let id = SessionId::new("race");
        first
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let (append, metadata) = tokio::join!(
            first.append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "preserved".to_owned(),
                }],
            ),
            second.set_metadata_value(&id, "key".to_owned(), serde_json::json!("value")),
        );

        assert_eq!(append.unwrap(), Revision::new(1));
        metadata.unwrap();
        let record = first.load(&id).await.unwrap();
        assert_eq!(record.revision, Revision::new(1));
        assert_eq!(record.journal.len(), 1);
        assert_eq!(
            record.metadata_value("key"),
            Some(&serde_json::json!("value"))
        );
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .and_then(|value| value.to_str())
                != Some("tmp")
        }));
    }

    #[tokio::test]
    async fn os_lock_serializes_conditional_writers_and_preserves_timeline_high_water() {
        let temp = tempfile::tempdir().unwrap();
        let first = FilesystemSessionStore::new(temp.path());
        let second = FilesystemSessionStore::new(temp.path());
        let id = SessionId::new("conditional-race");
        first
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        first
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![PendingTimelineEvent::assistant_text_delta(
                    id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "preserved",
                )],
            )
            .await
            .unwrap();

        let held = first.lock_file(&format!("{}.lock", id.as_str())).unwrap();
        held.lock().unwrap();
        let blocked_store = second.clone();
        let blocked_id = id.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .set_metadata_value_conditionally(
                    &blocked_id,
                    "winner".to_owned(),
                    serde_json::json!("blocked"),
                    MetadataSetCondition::Absent,
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        drop(held);
        assert!(blocked.await.unwrap().unwrap().changed);

        let (left, right) = tokio::join!(
            first.set_metadata_value_conditionally(
                &id,
                "single".to_owned(),
                serde_json::json!("left"),
                MetadataSetCondition::Absent,
            ),
            second.set_metadata_value_conditionally(
                &id,
                "single".to_owned(),
                serde_json::json!("right"),
                MetadataSetCondition::Absent,
            ),
        );
        assert_eq!(
            usize::from(left.unwrap().changed) + usize::from(right.unwrap().changed),
            1
        );
        let record = first.load(&id).await.unwrap();
        assert_eq!(
            record.timeline_cursor_high_water(),
            crate::TimelineCursor::new(1)
        );
    }

    #[tokio::test]
    async fn create_waits_for_the_store_lock_before_publishing() {
        let temp = tempfile::tempdir().unwrap();
        let first = FilesystemSessionStore::new(temp.path());
        let second = FilesystemSessionStore::new(temp.path());
        let id = SessionId::new("create-store-lock");
        let held = first.lock_file(STORE_LOCK_FILE).unwrap();
        held.lock().unwrap();

        let blocked_id = id.clone();
        let blocked =
            tokio::spawn(async move { second.create(SessionCreateRequest::new(blocked_id)).await });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        assert!(!first.path(&id).exists());

        drop(held);
        blocked.await.unwrap().unwrap();
        assert!(first.path(&id).exists());
    }

    #[test]
    fn failed_replacement_cleans_unique_same_directory_temporary_file() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path());
        let record = SessionRecord::new("cleanup");
        fs::create_dir_all(store.path(&record.id)).unwrap();

        store.save_sync(&record).unwrap_err();

        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path());
        let id = SessionId::new("session-delete");

        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
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
    async fn wipe_removes_valid_and_invalid_raw_records_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path());
        store
            .create(SessionCreateRequest::new("valid"))
            .await
            .unwrap();
        fs::write(temp.path().join("legacy.json"), b"not a session record").unwrap();

        store.wipe().await.unwrap();
        store.wipe().await.unwrap();

        assert!(!store.path(&SessionId::new("valid")).exists());
        assert!(!temp.path().join("legacy.json").exists());
        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [std::ffi::OsString::from(LOCK_DIRECTORY)]);
    }

    #[tokio::test]
    async fn list_returns_persisted_summaries_newest_activity_first() {
        let temp = tempfile::tempdir().unwrap();
        let store = FilesystemSessionStore::new(temp.path());
        let first = SessionId::new("session-first");
        let second = SessionId::new("session-second");
        let first_record = store
            .create(SessionCreateRequest::new(first.clone()))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        store
            .create(SessionCreateRequest::new(second.clone()))
            .await
            .unwrap();
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
