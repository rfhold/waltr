//! Filesystem-backed session storage.

use std::{fs, path::PathBuf};

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    timeline_after_slice, timeline_before_slice,
};
use crate::{
    agent::session::{Revision, SessionId, SessionJournalEvent, SessionRecord},
    timeline::{AgentTimelineEvent, PendingTimelineEvent, TimelineCursor},
};

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
        let value = update(&mut record, revision)?;
        record.revision = revision;
        self.save_sync(&record)?;
        Ok((revision, value))
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

    fn load(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move { store.load_sync(&id) }
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
                Ok(())
            })?;
            Ok(revision)
        }
    }

    fn timeline_after(
        &self,
        id: &SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let record = store.load_sync(&id)?;
            Ok(timeline_after_slice(&record, cursor, limit))
        }
    }

    fn timeline_before(
        &self,
        id: &SessionId,
        cursor: Option<TimelineCursor>,
        limit: usize,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let record = store.load_sync(&id)?;
            Ok(timeline_before_slice(&record, cursor, limit))
        }
    }
}

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
