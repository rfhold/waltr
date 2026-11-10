//! OpenDAL-backed session storage.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, OnceLock},
};
use tokio::sync::{Mutex, OnceCell};

use opendal::Operator;

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    mutate_on_record, sorted_session_summaries, timeline_item_text_from_record,
    timeline_query_slice,
};
use crate::{
    agent::session::{
        InferenceSettings, MetadataSetCondition, MetadataSetOutcome, Revision,
        SessionCreateRequest, SessionId, SessionJournalEvent, SessionRecord, SessionSummary,
    },
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

/// Generic `OpenDAL` implementation of [`SessionStore`].
#[derive(Clone)]
pub struct OpenDalSessionStore {
    operator: Operator,
    prefix: String,
    mutation_namespace: Arc<OnceCell<String>>,
    namespace_locator: String,
    compare_and_swap: Option<Arc<dyn OpenDalCompareAndSwap>>,
}

pub(super) trait OpenDalCompareAndSwap: Send + Sync {
    fn compare_exchange<'a>(
        &'a self,
        key: &'a str,
        expected: &'a [u8],
        replacement: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = SessionStoreResult<bool>> + Send + 'a>>;
}

impl std::fmt::Debug for OpenDalSessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenDalSessionStore")
            .field("operator", &self.operator)
            .field("prefix", &self.prefix)
            .field("namespace_locator", &self.namespace_locator)
            .field("compare_and_swap", &self.compare_and_swap.is_some())
            .finish_non_exhaustive()
    }
}

const MUTATION_NAMESPACE_OBJECT: &str = ".waltr-session-mutation-namespace";
const OPENDAL_CAS_ATTEMPTS: usize = 8;
type MutationKey = (String, String, SessionId);
type MutationGuards = OnceLock<StdMutex<HashMap<MutationKey, Arc<Mutex<()>>>>>;
type InitializationGuards = OnceLock<StdMutex<HashMap<String, Arc<Mutex<()>>>>>;

struct UpdatedRecord<'a> {
    etag: Option<&'a str>,
    original: &'a [u8],
    record: &'a SessionRecord,
    require_atomic: bool,
}

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
            compare_and_swap: None,
        }
    }

    pub(super) fn with_compare_and_swap(
        mut self,
        compare_and_swap: Arc<dyn OpenDalCompareAndSwap>,
    ) -> Self {
        self.compare_and_swap = Some(compare_and_swap);
        self
    }

    fn key(&self, id: &SessionId) -> String {
        format!("{}{}.json", self.prefix, id.as_str())
    }

    async fn load_record_with_etag(
        &self,
        id: &SessionId,
    ) -> SessionStoreResult<(SessionRecord, Option<String>, Vec<u8>)> {
        let key = self.key(id);
        for attempt in 1..=OPENDAL_CAS_ATTEMPTS {
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
                    let bytes = bytes.to_bytes().to_vec();
                    let record = super::decode_record(&bytes)?;
                    return Ok((record, etag, bytes));
                }
                Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => {
                    if attempt == OPENDAL_CAS_ATTEMPTS {
                        return Err(cas_contention(id, attempt));
                    }
                }
                Err(error) => return Err(backend_error(error)),
            }
        }
        unreachable!("OpenDAL CAS attempts are positive")
    }

    async fn load_record(&self, id: &SessionId) -> SessionStoreResult<SessionRecord> {
        self.load_record_with_etag(id)
            .await
            .map(|(record, _, _)| record)
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
            records.push(super::decode_record(bytes.to_bytes().as_ref())?);
        }
        Ok(sorted_session_summaries(records))
    }

    async fn update_record<T: Send>(
        &self,
        id: &SessionId,
        expected_token: super::super::ExecutionToken,
        update: impl Fn(&mut SessionRecord, Revision) -> SessionStoreResult<T> + Send,
    ) -> SessionStoreResult<(super::super::SessionCommitReceipt, T)> {
        let guard = self.mutation_guard(id).await?;
        let _mutation = guard.lock().await;
        for attempt in 1..=OPENDAL_CAS_ATTEMPTS {
            let (mut record, etag, original) = self.load_record_with_etag(id).await?;
            record.check_execution_token(&expected_token)?;
            let revision = record.revision.next();
            record.advance_revision(true);
            let value = update(&mut record, revision)?;
            let receipt = record.commit_receipt()?;
            if self
                .write_updated_record(
                    id,
                    UpdatedRecord {
                        etag: etag.as_deref(),
                        original: &original,
                        record: &record,
                        require_atomic: false,
                    },
                )
                .await?
            {
                return Ok((receipt, value));
            }
            if attempt == OPENDAL_CAS_ATTEMPTS {
                return Err(cas_contention(id, attempt));
            }
        }
        unreachable!("OpenDAL CAS attempts are positive")
    }

    async fn update_metadata_record(
        &self,
        id: &SessionId,
        expected: Option<&super::super::ExecutionToken>,
        update: impl Fn(&mut SessionRecord) + Send,
    ) -> SessionStoreResult<SessionRecord> {
        let guard = self.mutation_guard(id).await?;
        let _mutation = guard.lock().await;
        for attempt in 1..=OPENDAL_CAS_ATTEMPTS {
            let (mut record, etag, original) = self.load_record_with_etag(id).await?;
            if let Some(expected) = expected {
                record.check_execution_token(expected)?;
            }
            let revision = record.revision;
            update(&mut record);
            if record.revision == revision {
                return Ok(record);
            }
            if self
                .write_updated_record(
                    id,
                    UpdatedRecord {
                        etag: etag.as_deref(),
                        original: &original,
                        record: &record,
                        require_atomic: false,
                    },
                )
                .await?
            {
                return Ok(record);
            }
            if attempt == OPENDAL_CAS_ATTEMPTS {
                return Err(cas_contention(id, attempt));
            }
        }
        unreachable!("OpenDAL CAS attempts are positive")
    }

    async fn write_updated_record(
        &self,
        id: &SessionId,
        updated: UpdatedRecord<'_>,
    ) -> SessionStoreResult<bool> {
        let key = self.key(id);
        let replacement = serde_json::to_vec_pretty(updated.record).map_err(backend_error)?;
        if let Some(etag) = updated.etag {
            return match self
                .operator
                .write_with(&key, replacement)
                .if_match(etag)
                .await
            {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == opendal::ErrorKind::ConditionNotMatch => Ok(false),
                Err(error) => Err(backend_error(error)),
            };
        }
        if let Some(compare_and_swap) = &self.compare_and_swap {
            return compare_and_swap
                .compare_exchange(&key, updated.original, &replacement)
                .await;
        }
        if updated.require_atomic {
            return Err(SessionStoreError::Backend {
                message: format!(
                    "atomic session writes are unsupported for OpenDAL session {} because the backend provides neither ETags nor compare-and-swap",
                    id.as_str()
                ),
            });
        }
        self.operator
            .write(&key, replacement)
            .await
            .map_err(backend_error)?;
        Ok(true)
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
            super::super::metadata::validate_record(&record)?;
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
        expected_token: super::super::ExecutionToken,
        events: Vec<PendingTimelineEvent>,
    ) -> impl std::future::Future<Output = SessionStoreResult<super::super::TimelineAppendReceipt>> + Send
    {
        let store = self.clone();
        let id = id.clone();
        async move {
            let (commit, events) = store
                .update_record(&id, expected_token, |record, _| {
                    append_timeline_to_record(record, &id, events.clone())
                })
                .await?;
            Ok(super::super::TimelineAppendReceipt { commit, events })
        }
    }

    fn append_journal(
        &self,
        id: &SessionId,
        expected_token: super::super::ExecutionToken,
        events: Vec<SessionJournalEvent>,
    ) -> impl std::future::Future<Output = SessionStoreResult<super::super::SessionCommitReceipt>> + Send
    {
        let store = self.clone();
        let id = id.clone();
        async move {
            let (receipt, ()) = store
                .update_record(&id, expected_token, |record, _| {
                    record.append_journal_events(events.clone());
                    record.touch_activity();
                    Ok(())
                })
                .await?;
            Ok(receipt)
        }
    }

    fn mutate(
        &self,
        id: &SessionId,
        mutation: crate::SessionMutation,
    ) -> impl std::future::Future<Output = SessionStoreResult<crate::SessionMutationOutcome>> + Send
    {
        let store = self.clone();
        let id = id.clone();
        async move {
            let guard = store.mutation_guard(&id).await?;
            let _mutation_guard = guard.lock().await;
            for attempt in 1..=OPENDAL_CAS_ATTEMPTS {
                let (mut record, etag, original) = store.load_record_with_etag(&id).await?;
                let revision = record.revision.next();
                let outcome = mutate_on_record(&mut record, revision, &mutation)?;
                if store
                    .write_updated_record(
                        &id,
                        UpdatedRecord {
                            etag: etag.as_deref(),
                            original: &original,
                            record: &record,
                            require_atomic: true,
                        },
                    )
                    .await?
                {
                    return Ok(outcome);
                }
                if attempt == OPENDAL_CAS_ATTEMPTS {
                    return Err(cas_contention(&id, attempt));
                }
            }
            unreachable!("OpenDAL CAS attempts are positive")
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
            super::super::metadata::validate_set(&key, &value)?;
            let guard = store.mutation_guard(&id).await?;
            let _mutation = guard.lock().await;
            for attempt in 1..=OPENDAL_CAS_ATTEMPTS {
                let (mut record, etag, original) = store.load_record_with_etag(&id).await?;
                let changed = condition.matches(&record, &key)
                    && record.metadata.values.get(&key) != Some(&value);
                if !changed {
                    return Ok(MetadataSetOutcome { record, changed });
                }
                super::super::metadata::set_value(&mut record, key.clone(), value.clone())?;
                if store
                    .write_updated_record(
                        &id,
                        UpdatedRecord {
                            etag: etag.as_deref(),
                            original: &original,
                            record: &record,
                            require_atomic: true,
                        },
                    )
                    .await?
                {
                    return Ok(MetadataSetOutcome { record, changed });
                }
                if attempt == OPENDAL_CAS_ATTEMPTS {
                    return Err(cas_contention(&id, attempt));
                }
            }
            unreachable!("OpenDAL CAS attempts are positive")
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
            super::super::metadata::validate_key(&key)?;
            store
                .update_metadata_record(&id, None, |record| {
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
                .update_metadata_record(&id, None, |record| {
                    record.set_inference_settings(inference_settings.clone());
                })
                .await
        }
    }

    fn set_agent_definition(
        &self,
        id: &SessionId,
        agent_definition: Option<serde_json::Value>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            store
                .update_metadata_record(&id, None, |record| {
                    record.set_agent_definition(agent_definition.clone());
                })
                .await
        }
    }

    fn set_agent_definition_if_execution(
        &self,
        id: &SessionId,
        expected: super::super::ExecutionToken,
        agent_definition: Option<serde_json::Value>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        async move {
            self.update_metadata_record(id, Some(&expected), |record| {
                record.set_agent_definition(agent_definition.clone())
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

fn cas_contention(id: &SessionId, attempts: usize) -> SessionStoreError {
    SessionStoreError::Backend {
        message: format!(
            "OpenDAL compare-and-swap contention for session {} after {attempts} attempts",
            id.as_str()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use opendal::services;

    use super::*;

    struct TestCompareAndSwap {
        operator: Operator,
        rejections: Arc<AtomicUsize>,
        attempts: Arc<AtomicUsize>,
    }

    enum Interleave {
        Metadata,
        Execution,
        Uncertain,
    }

    struct InterleavingCompareAndSwap {
        operator: Operator,
        attempts: Arc<AtomicUsize>,
        interleave: Interleave,
    }

    impl OpenDalCompareAndSwap for InterleavingCompareAndSwap {
        fn compare_exchange<'a>(
            &'a self,
            key: &'a str,
            expected: &'a [u8],
            replacement: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = SessionStoreResult<bool>> + Send + 'a>> {
            Box::pin(async move {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                let bytes = self
                    .operator
                    .read(key)
                    .await
                    .map_err(backend_error)?
                    .to_bytes();
                if bytes.as_ref() != expected {
                    return Ok(false);
                }
                if attempt == 0 {
                    if matches!(self.interleave, Interleave::Uncertain) {
                        self.operator
                            .write(key, replacement.to_vec())
                            .await
                            .map_err(backend_error)?;
                        return Err(SessionStoreError::Backend {
                            message: "uncertain write acknowledgment".into(),
                        });
                    }
                    let mut record = super::super::decode_record(&bytes)?;
                    if matches!(self.interleave, Interleave::Metadata) {
                        super::super::super::metadata::set_value(
                            &mut record,
                            "title".into(),
                            serde_json::json!("concurrent"),
                        )?;
                    } else {
                        record.advance_revision(true);
                        record.append_journal_event(SessionJournalEvent::Interrupted);
                    }
                    self.operator
                        .write(
                            key,
                            serde_json::to_vec_pretty(&record).map_err(backend_error)?,
                        )
                        .await
                        .map_err(backend_error)?;
                    return Ok(false);
                }
                self.operator
                    .write(key, replacement.to_vec())
                    .await
                    .map_err(backend_error)?;
                Ok(true)
            })
        }
    }

    #[tokio::test]
    async fn conditional_configuration_rechecks_cas_and_never_retries_uncertain_ack() {
        for interleave in [
            Interleave::Metadata,
            Interleave::Execution,
            Interleave::Uncertain,
        ] {
            let metadata = matches!(interleave, Interleave::Metadata);
            let execution = matches!(interleave, Interleave::Execution);
            let operator = Operator::new(services::Memory::default()).unwrap().finish();
            let attempts = Arc::new(AtomicUsize::new(0));
            let store = OpenDalSessionStore::new(operator.clone(), "sessions")
                .with_compare_and_swap(Arc::new(InterleavingCompareAndSwap {
                    operator,
                    attempts: attempts.clone(),
                    interleave,
                }));
            let id = SessionId::new("conditional-configuration-cas");
            let initial = store
                .create(SessionCreateRequest::new(id.clone()))
                .await
                .unwrap();
            let result = store
                .set_agent_definition_if_execution(
                    &id,
                    initial.execution_token().unwrap(),
                    Some(serde_json::json!({"cycle":true})),
                )
                .await;
            let record = store.load(&id).await.unwrap();
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                if metadata { 2 } else { 1 }
            );
            if metadata {
                assert_eq!(result.unwrap(), record);
                assert_eq!(
                    record.metadata_value("title"),
                    Some(&serde_json::json!("concurrent"))
                );
                assert_eq!(record.revision.get(), 2);
                assert_eq!(record.execution_sequence.get(), 1);
            } else if execution {
                assert!(matches!(
                    result,
                    Err(SessionStoreError::StaleExecution { .. })
                ));
                assert!(record.agent_definition.is_none());
            } else {
                assert!(
                    matches!(result, Err(SessionStoreError::Backend { message }) if message == "uncertain write acknowledgment")
                );
                assert_eq!(
                    record.agent_definition,
                    Some(serde_json::json!({"cycle":true}))
                );
                assert_eq!(record.execution_sequence.get(), 1);
            }
        }
    }

    #[tokio::test]
    async fn tokenless_conditional_configuration_preserves_metadata_and_checks_execution_noop() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let store = OpenDalSessionStore::new(operator.clone(), "sessions");
        let other = OpenDalSessionStore::new(operator, "sessions/");
        let id = SessionId::new("conditional-configuration-process-owned");
        let mut request = SessionCreateRequest::new(id.clone());
        request
            .metadata_values
            .insert("title".into(), serde_json::json!("latest"));
        request
            .metadata_values
            .insert("removed".into(), serde_json::json!(true));
        let initial = store.create(request).await.unwrap();
        let token = initial.execution_token().unwrap();
        assert_eq!(
            store
                .set_agent_definition_if_execution(&id, token.clone(), None)
                .await
                .unwrap(),
            initial
        );
        other
            .remove_metadata_value(&id, "removed".into())
            .await
            .unwrap();
        let configured = store
            .set_agent_definition_if_execution(
                &id,
                token.clone(),
                Some(serde_json::json!({"cycle":true})),
            )
            .await
            .unwrap();
        assert_eq!(
            configured.metadata_value("title"),
            Some(&serde_json::json!("latest"))
        );
        assert!(configured.metadata_value("removed").is_none());
        assert_eq!(configured.revision.get(), 2);
        assert_eq!(configured.execution_sequence.get(), 1);
        assert!(matches!(
            other
                .set_agent_definition_if_execution(&id, token, configured.agent_definition.clone())
                .await,
            Err(SessionStoreError::StaleExecution { .. })
        ));
        let token = configured.execution_token().unwrap();
        assert_eq!(
            other
                .set_agent_definition_if_execution(
                    &id,
                    token.clone(),
                    configured.agent_definition.clone()
                )
                .await
                .unwrap(),
            configured
        );
        other
            .append_journal(&id, token.clone(), vec![SessionJournalEvent::Finished])
            .await
            .unwrap();
        let committed = store.load(&id).await.unwrap();
        assert!(matches!(
            store
                .set_agent_definition_if_execution(&id, token, configured.agent_definition.clone())
                .await,
            Err(SessionStoreError::StaleExecution { .. })
        ));
        assert_eq!(store.load(&id).await.unwrap(), committed);
    }

    #[tokio::test]
    async fn append_rechecks_token_after_definite_cas_rejection_and_preserves_title() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(InterleavingCompareAndSwap {
                operator,
                attempts: attempts.clone(),
                interleave: Interleave::Metadata,
            }),
        );
        let id = SessionId::new("metadata-cas-conflict");
        let record = store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let receipt = store
            .append_journal(
                &id,
                record.execution_token().unwrap(),
                vec![SessionJournalEvent::Finished],
            )
            .await
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(receipt.revision, Revision::new(2));
        assert_eq!(receipt.execution_token.sequence.get(), 1);
        assert_eq!(
            receipt.metadata.values["title"],
            serde_json::json!("concurrent")
        );
        assert_eq!(
            store.load(&id).await.unwrap().journal,
            vec![SessionJournalEvent::Finished]
        );
    }

    #[tokio::test]
    async fn append_rejects_execution_conflict_after_definite_cas_rejection() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(InterleavingCompareAndSwap {
                operator,
                attempts: attempts.clone(),
                interleave: Interleave::Execution,
            }),
        );
        let id = SessionId::new("execution-cas-conflict");
        let record = store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let error = store
            .append_journal(
                &id,
                record.execution_token().unwrap(),
                vec![SessionJournalEvent::Finished],
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SessionStoreError::StaleExecution { .. }));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.load(&id).await.unwrap().journal,
            vec![SessionJournalEvent::Interrupted]
        );
    }

    #[tokio::test]
    async fn uncertain_cas_ack_is_not_retried_even_when_replacement_committed() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(InterleavingCompareAndSwap {
                operator,
                attempts: attempts.clone(),
                interleave: Interleave::Uncertain,
            }),
        );
        let id = SessionId::new("uncertain-cas");
        let record = store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let error = store
            .append_journal(
                &id,
                record.execution_token().unwrap(),
                vec![SessionJournalEvent::Finished],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionStoreError::Backend { message } if message == "uncertain write acknowledgment")
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.load(&id).await.unwrap().journal,
            vec![SessionJournalEvent::Finished]
        );
    }

    #[tokio::test]
    async fn bytewise_cas_uses_exact_legacy_bytes_before_persisting_execution_sequence() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(TestCompareAndSwap {
                operator: operator.clone(),
                rejections: Arc::new(AtomicUsize::new(0)),
                attempts: attempts.clone(),
            }),
        );
        let mut record = SessionRecord::new("legacy-bytewise-cas");
        record.revision = Revision::new(17);
        record.journal.push(SessionJournalEvent::Interrupted);
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("execution_sequence");
        let original = serde_json::to_vec_pretty(&legacy).unwrap();
        operator
            .write(&store.key(&record.id), original.clone())
            .await
            .unwrap();
        let loaded = store.load(&record.id).await.unwrap();
        assert_eq!(loaded.execution_sequence.get(), 17);
        assert_eq!(
            operator
                .read(&store.key(&record.id))
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            original.as_slice()
        );
        let receipt = store
            .append_journal(
                &record.id,
                loaded.execution_token().unwrap(),
                vec![SessionJournalEvent::Finished],
            )
            .await
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(receipt.execution_token.sequence.get(), 18);
        assert_eq!(
            store.load(&record.id).await.unwrap().journal[0],
            record.journal[0]
        );
    }

    impl OpenDalCompareAndSwap for TestCompareAndSwap {
        fn compare_exchange<'a>(
            &'a self,
            key: &'a str,
            expected: &'a [u8],
            replacement: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = SessionStoreResult<bool>> + Send + 'a>> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                if self
                    .rejections
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                        value.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Ok(false);
                }
                let current = self
                    .operator
                    .read(key)
                    .await
                    .map_err(backend_error)?
                    .to_bytes();
                if current.as_ref() != expected {
                    return Ok(false);
                }
                self.operator
                    .write(key, replacement.to_vec())
                    .await
                    .map_err(backend_error)?;
                Ok(true)
            })
        }
    }

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
                    append_store
                        .load(&append_id)
                        .await
                        .unwrap()
                        .execution_token()
                        .unwrap(),
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
                    Some(crate::agent::session::opaque_test_configuration(
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

        assert_eq!(append.await.unwrap().unwrap().revision, Revision::new(1));
        inference.await.unwrap().unwrap();
        definition.await.unwrap().unwrap();
        let record = first.load(&id).await.unwrap();
        assert_eq!(record.revision, Revision::new(3));
        assert_eq!(record.journal.len(), 1);
        assert_eq!(
            record.inference_settings,
            Some(inference_settings("updated"))
        );
        assert_eq!(record.agent_definition.unwrap()["name"], "selected");
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

    #[tokio::test]
    async fn tokenless_backend_rejects_atomic_metadata_write_instead_of_falling_back() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let store = OpenDalSessionStore::new(operator, "sessions");
        let id = SessionId::new("tokenless");
        let record = store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let original = serde_json::to_vec_pretty(&record).unwrap();
        let mut replacement = record;
        replacement.set_metadata_value("key", serde_json::json!("value"));

        let error = store
            .write_updated_record(
                &id,
                UpdatedRecord {
                    etag: None,
                    original: &original,
                    record: &replacement,
                    require_atomic: true,
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SessionStoreError::Backend { message }
                if message.contains("neither ETags nor compare-and-swap")
        ));
        assert_eq!(store.load(&id).await.unwrap().metadata_value("key"), None);
    }

    #[tokio::test]
    async fn tokenless_backend_rejects_atomic_metadata_and_masks_without_partial_persistence() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let store = OpenDalSessionStore::new(operator, "sessions");
        let id = SessionId::new("tokenless-mask");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![crate::PendingTimelineEvent::assistant_text_snapshot(
                    id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "answer",
                )],
            )
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![SessionJournalEvent::TurnFinished { turn: 0 }],
            )
            .await
            .unwrap();
        let mut mutation = crate::SessionMaskMutation::new(Some(crate::TimelineCursor::new(1)));
        mutation.add.push(crate::SessionMaskAddition::new(
            "summary",
            crate::ThroughCursor::new(crate::TimelineCursor::new(1)),
            crate::ReplacementUserContent::new("compacted"),
        ));

        let error = store.mutate(&id, crate::SessionMutation {
            masks: Some(mutation),
            updates: vec![crate::MetadataUpdate { target: crate::MetadataTarget::Session,
                key: "opaque".into(), value: Some(serde_json::json!({"free": true})) }],
            ..Default::default()
        }).await.unwrap_err();

        assert!(matches!(
            error,
            SessionStoreError::Backend { message }
                if message.contains("neither ETags nor compare-and-swap")
        ));
        let record = store.load(&id).await.unwrap();
        assert_eq!(record.revision, Revision::new(2));
        assert!(record.active_masks().is_empty());
        assert!(record.metadata_value("opaque").is_none());
    }

    #[tokio::test]
    async fn replacement_masks_follow_successful_compare_and_swap_contract() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(TestCompareAndSwap {
                operator,
                rejections: Arc::new(AtomicUsize::new(0)),
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
        );

        super::super::assert_mask_store_contract(store, "opendal-cas").await;
    }

    #[tokio::test]
    async fn repeated_compare_and_swap_rejection_is_bounded() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let rejections = Arc::new(AtomicUsize::new(usize::MAX));
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(TestCompareAndSwap {
                operator,
                rejections,
                attempts: Arc::clone(&attempts),
            }),
        );
        let id = SessionId::new("bounded-cas");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let error = store
            .mutate(&id, crate::SessionMutation {
                updates: vec![crate::MetadataUpdate { target: crate::MetadataTarget::Session,
                    key: "key".into(), value: Some(serde_json::json!("value")) }],
                ..Default::default()
            })
            .await
            .unwrap_err();

        assert_eq!(attempts.load(Ordering::SeqCst), OPENDAL_CAS_ATTEMPTS);
        assert!(store.load(&id).await.unwrap().metadata_value("key").is_none());
        assert!(matches!(
            error,
            SessionStoreError::Backend { message }
                if message.contains("compare-and-swap contention")
                    && message.contains("8 attempts")
        ));
    }

    #[tokio::test]
    async fn compare_and_swap_retry_preserves_timeline_cursor_high_water() {
        let operator = Operator::new(services::Memory::default()).unwrap().finish();
        let rejections = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let store = OpenDalSessionStore::new(operator.clone(), "sessions").with_compare_and_swap(
            Arc::new(TestCompareAndSwap {
                operator,
                rejections: Arc::clone(&rejections),
                attempts,
            }),
        );
        let id = SessionId::new("cursor-cas");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![PendingTimelineEvent::assistant_text_delta(
                    id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "preserved",
                )],
            )
            .await
            .unwrap();
        rejections.store(1, Ordering::SeqCst);

        let outcome = store
            .mutate(&id, crate::SessionMutation {
                updates: vec![crate::MetadataUpdate { target: crate::MetadataTarget::Session,
                    key: "key".into(), value: Some(serde_json::json!("value")) }],
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(outcome.record.metadata_value("key"), Some(&serde_json::json!("value")));
        assert_eq!(
            outcome.record.timeline_cursor_high_water(),
            crate::TimelineCursor::new(1)
        );
        assert_eq!(
            store.load(&id).await.unwrap().timeline_cursor_high_water(),
            crate::TimelineCursor::new(1)
        );
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
