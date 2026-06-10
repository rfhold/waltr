//! S3-backed session storage.

use aws_sdk_s3::{Client, error::DisplayErrorContext, primitives::ByteStream};

use super::{SessionStore, SessionStoreError, SessionStoreResult};
use crate::agent::session::{
    ExternalRequest, RequestRecord, Revision, SessionEvent, SessionId, SessionRecord,
    SessionSnapshot,
};

/// S3 JSON-object implementation of [`SessionStore`].
#[derive(Debug, Clone)]
pub struct S3SessionStore {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3SessionStore {
    /// Create an S3-backed store.
    #[must_use]
    pub fn new(client: Client, bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Self {
            client,
            bucket: bucket.into(),
            prefix,
        }
    }

    fn key(&self, id: &SessionId) -> String {
        format!("{}{}.json", self.prefix, id.as_str())
    }

    async fn load_record(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        let key = self.key(id);
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| map_s3_error(id, error))?;
        let bytes = output
            .body
            .collect()
            .await
            .map_err(backend_error)?
            .into_bytes();
        serde_json::from_slice(bytes.as_ref()).map_err(backend_error)
    }

    async fn save_record(&self, record: &SessionRecord) -> SessionStoreResult<()> {
        let key = self.key(&record.id);
        let bytes = serde_json::to_vec_pretty(record).map_err(backend_error)?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
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

impl SessionStore for S3SessionStore {
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

fn map_s3_error(id: &SessionId, error: impl std::error::Error) -> SessionStoreError {
    let message = DisplayErrorContext(&error).to_string();
    if message.contains("NoSuchKey") || message.contains("NotFound") || message.contains("404") {
        SessionStoreError::NotFound { id: id.clone() }
    } else {
        SessionStoreError::Backend { message }
    }
}

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: DisplayErrorContext(&error).to_string(),
    }
}
