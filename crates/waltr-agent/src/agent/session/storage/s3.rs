//! S3-backed session storage.

use std::collections::BTreeMap;

use aws_sdk_s3::{Client, error::DisplayErrorContext, primitives::ByteStream};

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    sorted_session_summaries, timeline_item_text_from_record, timeline_query_slice,
};
use crate::{
    agent::session::{
        InferenceSettings, Revision, SessionId, SessionJournalEvent, SessionRecord, SessionSummary,
    },
    definition::AgentDefinitionBundle,
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

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

    async fn load_record_with_etag(
        &self,
        id: &SessionId,
    ) -> SessionStoreResult<(SessionRecord, String)> {
        let key = self.key(id);
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| map_s3_error(id, error))?;
        let etag = output
            .e_tag()
            .ok_or_else(|| SessionStoreError::Backend {
                message: format!("S3 object for {} has no ETag", id.as_str()),
            })?
            .to_owned();
        let bytes = output
            .body
            .collect()
            .await
            .map_err(backend_error)?
            .into_bytes();
        let record = serde_json::from_slice(bytes.as_ref()).map_err(backend_error)?;
        Ok((record, etag))
    }

    async fn load_record(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        self.load_record_with_etag(id)
            .await
            .map(|(record, _)| record)
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

    async fn list_records(&self) -> SessionStoreResult<Vec<SessionSummary>> {
        let mut continuation_token = None;
        let mut records = Vec::new();
        loop {
            let output = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&self.prefix)
                .set_continuation_token(continuation_token)
                .send()
                .await
                .map_err(backend_error)?;
            for object in output.contents() {
                let Some(key) = object.key() else { continue };
                if !key
                    .rsplit_once('.')
                    .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("json"))
                {
                    continue;
                }
                let output = self
                    .client
                    .get_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(backend_error)?;
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(backend_error)?
                    .into_bytes();
                records.push(serde_json::from_slice(bytes.as_ref()).map_err(backend_error)?);
            }
            if output.is_truncated().unwrap_or(false) {
                continuation_token = output.next_continuation_token().map(str::to_owned);
            } else {
                break;
            }
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
            let (mut record, etag) = self.load_record_with_etag(id).await?;
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
            let bytes = serde_json::to_vec_pretty(&record).map_err(backend_error)?;
            let result = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(self.key(id))
                .if_match(etag)
                .body(ByteStream::from(bytes))
                .send()
                .await;
            match result {
                Ok(_) => return Ok((revision, value)),
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|response| response.status().as_u16() == 412) => {}
                Err(error) => return Err(backend_error(error)),
            }
        }
    }

    async fn update_metadata_record(
        &self,
        id: &SessionId,
        update: impl Fn(&mut SessionRecord) + Send,
    ) -> SessionStoreResult<SessionRecord> {
        loop {
            let (mut record, etag) = self.load_record_with_etag(id).await?;
            update(&mut record);
            let bytes = serde_json::to_vec_pretty(&record).map_err(backend_error)?;
            let result = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(self.key(id))
                .if_match(etag)
                .body(ByteStream::from(bytes))
                .send()
                .await;
            match result {
                Ok(_) => return Ok(record),
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|response| response.status().as_u16() == 412) => {}
                Err(error) => return Err(backend_error(error)),
            }
        }
    }

    async fn wipe_records(&self) -> SessionStoreResult<()> {
        let mut continuation_token = None;
        let mut keys = Vec::new();
        loop {
            let output = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&self.prefix)
                .set_continuation_token(continuation_token)
                .send()
                .await
                .map_err(backend_error)?;
            keys.extend(
                output
                    .contents()
                    .iter()
                    .filter_map(|object| object.key().map(str::to_owned)),
            );
            if output.is_truncated().unwrap_or(false) {
                continuation_token = output.next_continuation_token().map(str::to_owned);
            } else {
                break;
            }
        }
        for key in keys {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(backend_error)?;
        }
        Ok(())
    }
}

impl SessionStore for S3SessionStore {
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

    fn create_with_inference_settings(
        &self,
        id: SessionId,
        inference_settings: Option<InferenceSettings>,
        agent_definition: Option<AgentDefinitionBundle>,
        metadata_values: BTreeMap<String, Value>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = SessionRecord::with_inference_settings_and_metadata_values(
                id,
                inference_settings,
                agent_definition,
                metadata_values,
            );
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
                .client
                .delete_object()
                .bucket(&store.bucket)
                .key(store.key(&id))
                .send()
                .await
                .map_err(backend_error)?;
            Ok(())
        }
    }

    fn wipe(&self) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move { store.wipe_records().await }
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
