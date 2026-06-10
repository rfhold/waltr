//! Filesystem-backed session storage.

use std::{fs, path::PathBuf};

use super::{SessionStore, SessionStoreError, SessionStoreResult};
use crate::agent::session::{
    ExternalRequest, RequestRecord, Revision, SessionEvent, SessionId, SessionRecord,
    SessionSnapshot,
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

    fn update_sync(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl FnOnce(&mut SessionRecord, Revision),
    ) -> SessionStoreResult<Revision> {
        let mut record = self.load_sync(id)?;
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
        self.save_sync(&record)?;
        Ok(revision)
    }
}

impl SessionStore for FilesystemSessionStore {
    fn create(
        &self,
        record: SessionRecord,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move {
            if store.path(&record.id).exists() {
                return Err(SessionStoreError::AlreadyExists { id: record.id });
            }
            store.save_sync(&record)
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

    fn append(
        &self,
        id: &SessionId,
        events: Vec<SessionEvent>,
        expected_revision: Revision,
    ) -> impl std::future::Future<Output = SessionStoreResult<Revision>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store.update_sync(&id, expected_revision, |record, _| {
                record.events.extend(events);
            })
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
            store.update_sync(&id, expected_revision, |record, revision| {
                snapshot.revision = revision;
                record.snapshot = snapshot;
            })
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
            store.update_sync(&id, expected_revision, |record, _| {
                record.requests.push(request);
            })
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
            store.update_sync(&id, expected_revision, |record, revision| {
                record.snapshot.external_requests.push(request);
                record.snapshot.revision = revision;
            })
        }
    }
}

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
