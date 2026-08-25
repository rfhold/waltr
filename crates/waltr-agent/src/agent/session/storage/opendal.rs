//! OpenDAL-backed session storage.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex as StdMutex, OnceLock},
};
use tokio::sync::{Mutex, OnceCell};

use opendal::Operator;

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

/// Generic `OpenDAL` implementation of [`SessionStore`].
#[derive(Debug, Clone)]
pub struct OpenDalSessionStore {
    operator: Operator,
    prefix: String,
    mutation_namespace: Arc<OnceCell<String>>,
    namespace_locator: String,
}

const MUTATION_NAMESPACE_OBJECT: &str = ".waltr-session-mutation-namespace";
type MutationKey = (String, String, SessionId);
type MutationGuards = OnceLock<StdMutex<HashMap<MutationKey, Arc<Mutex<()>>>>>;
type InitializationGuards = OnceLock<StdMutex<HashMap<String, Arc<Mutex<()>>>>>;

impl OpenDalSessionStore {
    /// Create an OpenDAL-backed store using `operator` and key `prefix`.
    #[must_use]
    pub fn new(operator: Operator, prefix: impl Into<String>) -> Self {
        let info = operator.info();
        let namespace_locator = format!("{}\0{}\0{}", info.scheme(), info.name(), info.root());
        Self::build(operator, prefix, namespace_locator, OnceCell::new())
    }

    /// Create a store with an explicit same-process mutation-lock namespace.
    ///
    /// Independently constructed stores addressing the same logical backend must use the same
    /// namespace. Use this constructor when operator metadata does not uniquely identify the
    /// configured endpoint.
    #[must_use]
    pub fn new_with_namespace(
        operator: Operator,
        prefix: impl Into<String>,
        mutation_namespace: impl Into<String>,
    ) -> Self {
        let mutation_namespace = mutation_namespace.into();
        Self::build(
            operator,
            prefix,
            mutation_namespace.clone(),
            OnceCell::from(mutation_namespace),
        )
    }

    fn build(
        operator: Operator,
        prefix: impl Into<String>,
        namespace_locator: String,
        mutation_namespace: OnceCell<String>,
    ) -> Self {
        let mut prefix = prefix.into();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Self {
            operator,
            prefix,
            mutation_namespace: Arc::new(mutation_namespace),
            namespace_locator,
        }
    }

    fn key(&self, id: &SessionId) -> String {
        format!("{}{}.json", self.prefix, id.as_str())
    }

    async fn load_record_with_etag(
        &self,
        id: &SessionId,
    ) -> SessionStoreResult<(SessionRecord, Option<String>)> {
        let key = self.key(id);
        loop {
            let metadata = self.operator.stat(&key).await.map_err(|error| {
                if error.kind() == opendal::ErrorKind::NotFound {
                    SessionStoreError::NotFound { id: id.clone() }
                } else {
                    backend_error(error)
                }
            })?;
            let etag = metadata.etag().map(str::to_owned);
            let read = match etag.as_deref() {
                Some(etag) => self.operator.read_with(&key).if_match(etag).await,
                None => self.operator.read(&key).await,
            };
            match read {
                Ok(bytes) => {
                    let record =
                        serde_json::from_slice(bytes.to_bytes().as_ref()).map_err(backend_error)?;
                    return Ok((record, etag));
                }
                Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => {}
                Err(error) => return Err(backend_error(error)),
            }
        }
    }

    async fn load_record(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        self.load_record_with_etag(id)
            .await
            .map(|(record, _)| record)
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
        update: impl Fn(&mut SessionRecord, Revision) -> SessionStoreResult<T> + Send,
    ) -> SessionStoreResult<(Revision, T)> {
        let guard = self.mutation_guard(id).await?;
        let _mutation = guard.lock().await;
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
            let write = match etag.as_deref() {
                Some(etag) => {
                    self.operator
                        .write_with(&self.key(id), bytes)
                        .if_match(etag)
                        .await
                }
                None => self.operator.write(&self.key(id), bytes).await,
            };
            match write {
                Ok(_) => return Ok((revision, value)),
                Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => {}
                Err(error) => return Err(backend_error(error)),
            }
        }
    }

    async fn update_metadata_record(
        &self,
        id: &SessionId,
        update: impl Fn(&mut SessionRecord) + Send,
    ) -> SessionStoreResult<SessionRecord> {
        let guard = self.mutation_guard(id).await?;
        let _mutation = guard.lock().await;
        loop {
            let (mut record, etag) = self.load_record_with_etag(id).await?;
            update(&mut record);
            let bytes = serde_json::to_vec_pretty(&record).map_err(backend_error)?;
            let write = match etag.as_deref() {
                Some(etag) => {
                    self.operator
                        .write_with(&self.key(id), bytes)
                        .if_match(etag)
                        .await
                }
                None => self.operator.write(&self.key(id), bytes).await,
            };
            match write {
                Ok(_) => return Ok(record),
                Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => {}
                Err(error) => return Err(backend_error(error)),
            }
        }
    }

    async fn mutation_guard(&self, id: &SessionId) -> SessionStoreResult<Arc<Mutex<()>>> {
        static MUTATION_GUARDS: MutationGuards = OnceLock::new();
        let mutation_namespace = self.mutation_namespace().await?;
        Ok(MUTATION_GUARDS
            .get_or_init(|| StdMutex::new(HashMap::new()))
            .lock()
            .map_err(|_| SessionStoreError::LockPoisoned)?
            .entry((mutation_namespace, self.prefix.clone(), id.clone()))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    async fn mutation_namespace(&self) -> SessionStoreResult<String> {
        let namespace = self
            .mutation_namespace
            .get_or_try_init(|| async {
                static INITIALIZATION_GUARDS: InitializationGuards = OnceLock::new();
                let guard = INITIALIZATION_GUARDS
                    .get_or_init(|| StdMutex::new(HashMap::new()))
                    .lock()
                    .map_err(|_| SessionStoreError::LockPoisoned)?
                    .entry(format!("{}\0{}", self.namespace_locator, self.prefix))
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                let _initialization = guard.lock().await;
                let key = format!("{}{}", self.prefix, MUTATION_NAMESPACE_OBJECT);
                match self.operator.read(&key).await {
                    Ok(bytes) => {
                        String::from_utf8(bytes.to_bytes().to_vec()).map_err(backend_error)
                    }
                    Err(error) if error.kind() == opendal::ErrorKind::NotFound => {
                        let namespace = uuid::Uuid::now_v7().to_string();
                        self.operator
                            .write(&key, namespace.clone())
                            .await
                            .map_err(backend_error)?;
                        Ok(namespace)
                    }
                    Err(error) => Err(backend_error(error)),
                }
            })
            .await?;
        Ok(namespace.clone())
    }

    async fn wipe_records(&self) -> SessionStoreResult<()> {
        let mut paths = self
            .operator
            .list_with(&self.prefix)
            .recursive(true)
            .await
            .map_err(backend_error)?
            .into_iter()
            .map(|entry| entry.path().to_owned())
            .filter(|path| {
                path != &self.prefix
                    && path != &format!("{}{}", self.prefix, MUTATION_NAMESPACE_OBJECT)
            })
            .collect::<Vec<_>>();
        paths.sort_by(|left, right| right.cmp(left));
        for path in paths {
            self.operator.delete(&path).await.map_err(backend_error)?;
        }
        Ok(())
    }
}

impl SessionStore for OpenDalSessionStore {
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
            match store.operator.delete(&store.key(&id)).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == opendal::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(backend_error(error)),
            }
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

fn backend_error(error: impl std::error::Error) -> SessionStoreError {
    SessionStoreError::Backend {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use opendal::services;

    use super::*;

    #[tokio::test]
    async fn wipe_removes_valid_and_invalid_raw_objects_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let builder = services::Fs::default().root(temp.path().to_str().unwrap());
        let operator = Operator::new(builder).unwrap().finish();
        let store = OpenDalSessionStore::new(operator.clone(), "sessions");
        store
            .create(SessionCreateRequest::new("valid"))
            .await
            .unwrap();
        operator
            .write("sessions/legacy-record", "not a session record")
            .await
            .unwrap();

        store.wipe().await.unwrap();
        store.wipe().await.unwrap();

        assert!(
            operator
                .list_with("sessions/")
                .recursive(true)
                .await
                .unwrap()
                .into_iter()
                .all(|entry| {
                    entry.path() == "sessions/"
                        || entry.path() == format!("sessions/{MUTATION_NAMESPACE_OBJECT}")
                })
        );
    }

    #[tokio::test]
    async fn independently_constructed_stores_serialize_append_and_field_mutations() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let first = OpenDalSessionStore::new(operator.clone(), "sessions");
        let second = OpenDalSessionStore::new(operator, "sessions");
        let id = SessionId::new("race");
        first
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let gate = first.mutation_guard(&id).await.unwrap();
        assert!(Arc::ptr_eq(
            &gate,
            &second.mutation_guard(&id).await.unwrap()
        ));
        let held = gate.lock().await;

        let append_store = first.clone();
        let append_id = id.clone();
        let append = tokio::spawn(async move {
            append_store
                .append_journal(
                    &append_id,
                    Revision::ZERO,
                    vec![SessionJournalEvent::PromptAppended {
                        prompt: "preserved".to_owned(),
                    }],
                )
                .await
        });
        tokio::task::yield_now().await;

        let inference_store = second.clone();
        let inference_id = id.clone();
        let inference = tokio::spawn(async move {
            inference_store
                .set_inference_settings(&inference_id, Some(inference_settings("updated")))
                .await
        });
        let definition_store = second.clone();
        let definition_id = id.clone();
        let definition = tokio::spawn(async move {
            definition_store
                .set_agent_definition(
                    &definition_id,
                    Some(AgentDefinitionBundle::new(
                        "selected",
                        vec!["kind: Agent".to_owned()],
                    )),
                )
                .await
        });
        tokio::task::yield_now().await;

        assert!(!append.is_finished());
        assert!(!inference.is_finished());
        assert!(!definition.is_finished());
        drop(held);

        assert_eq!(append.await.unwrap().unwrap(), Revision::new(1));
        inference.await.unwrap().unwrap();
        definition.await.unwrap().unwrap();
        let record = first.load(&id).await.unwrap();
        assert_eq!(record.revision, Revision::new(1));
        assert_eq!(record.journal.len(), 1);
        assert_eq!(
            record.inference_settings,
            Some(inference_settings("updated"))
        );
        assert_eq!(record.agent_definition.unwrap().agent_name, "selected");
    }

    #[tokio::test]
    async fn roots_and_explicit_namespaces_isolate_guards_and_records() {
        let first = OpenDalSessionStore::new(
            Operator::new(services::Memory::default()).unwrap().finish(),
            "sessions",
        );
        let second = OpenDalSessionStore::new(
            Operator::new(services::Memory::default()).unwrap().finish(),
            "sessions",
        );
        let id = SessionId::new("isolated");

        assert!(!Arc::ptr_eq(
            &first.mutation_guard(&id).await.unwrap(),
            &second.mutation_guard(&id).await.unwrap()
        ));
        first
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        assert!(matches!(
            second.load(&id).await,
            Err(SessionStoreError::NotFound { .. })
        ));
    }

    fn inference_settings(model_id: &str) -> InferenceSettings {
        InferenceSettings {
            provider: crate::InferenceProviderSettings {
                key: "test".to_owned(),
                api: crate::ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: model_id.to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        }
    }
}
