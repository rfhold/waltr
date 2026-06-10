//! OpenDAL-backed session storage.

use opendal::Operator;

use super::{SessionStore, SessionStoreError, SessionStoreResult};
use crate::agent::session::{
    ExternalRequest, RequestRecord, Revision, SessionEvent, SessionId, SessionRecord,
    SessionSnapshot,
};

/// Generic OpenDAL implementation of [`SessionStore`].
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

impl SessionStore for OpenDalSessionStore {
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

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}
