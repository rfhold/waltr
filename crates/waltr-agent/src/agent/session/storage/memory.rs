//! Process-memory session store.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    sorted_session_summaries, timeline_item_text_from_record, timeline_query_slice,
};
use crate::{
    agent::session::{
        InferenceSettings, MetadataSetCondition, MetadataSetOutcome, Revision,
        SessionCreateRequest, SessionId, SessionJournalEvent, SessionRecord, SessionSummary,
    },
    definition::AgentDefinitionBundle,
    timeline::{
        AgentTimelineEvent, PendingTimelineEvent, TimelineItemId, TimelineItemText, TimelineQuery,
    },
};
use serde_json::Value;

/// Process-memory implementation of [`SessionStore`].
#[derive(Debug, Clone, Default)]
pub struct MemorySessionStore {
    records: Arc<Mutex<HashMap<SessionId, SessionRecord>>>,
}

impl MemorySessionStore {
    /// Create an empty process-memory session store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with_record_mut<T>(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl FnOnce(&mut SessionRecord, Revision) -> SessionStoreResult<T>,
    ) -> SessionStoreResult<(Revision, T)> {
        let mut records = self
            .records
            .lock()
            .map_err(|_| SessionStoreError::LockPoisoned)?;
        let record = records
            .get(id)
            .ok_or_else(|| SessionStoreError::NotFound { id: id.clone() })?;

        if record.revision != expected_revision {
            return Err(SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: expected_revision,
                actual: record.revision,
            });
        }

        let next_revision = record.revision.next();
        let mut updated = record.clone();
        updated.revision = next_revision;
        let value = update(&mut updated, next_revision)?;
        records.insert(id.clone(), updated);
        drop(records);

        Ok((next_revision, value))
    }
}

impl SessionStore for MemorySessionStore {
    fn create(
        &self,
        request: SessionCreateRequest,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = request.materialize();
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;

            if records.contains_key(&record.id) {
                return Err(SessionStoreError::AlreadyExists { id: record.id });
            }

            records.insert(record.id.clone(), record.clone());
            drop(records);
            Ok(record)
        }
    }

    fn load(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;

            records
                .get(&id)
                .cloned()
                .ok_or(SessionStoreError::NotFound { id })
        }
    }

    fn delete(
        &self,
        id: &SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        let id = id.clone();
        async move {
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;
            records.remove(&id);
            drop(records);
            Ok(())
        }
    }

    fn wipe(&self) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move {
            store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?
                .clear();
            Ok(())
        }
    }

    fn list(
        &self,
    ) -> impl std::future::Future<Output = SessionStoreResult<Vec<SessionSummary>>> + Send {
        let store = self.clone();
        async move {
            let records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;
            Ok(sorted_session_summaries(records.values().cloned()))
        }
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
            let (_, assigned) = store.with_record_mut(&id, expected_revision, |record, _| {
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
            let (revision, ()) = store.with_record_mut(&id, expected_revision, |record, _| {
                record.append_journal_events(events);
                record.touch_activity();
                Ok(())
            })?;

            Ok(revision)
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
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;
            let record = records
                .get_mut(&id)
                .ok_or_else(|| SessionStoreError::NotFound { id: id.clone() })?;
            let changed = condition == MetadataSetCondition::Unconditional
                || !record.metadata.values.contains_key(&key);
            if changed {
                record.set_metadata_value(key, value);
            }
            let record = record.clone();
            drop(records);
            Ok(MetadataSetOutcome { record, changed })
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
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;
            let record = records
                .get_mut(&id)
                .ok_or_else(|| SessionStoreError::NotFound { id: id.clone() })?;
            record.remove_metadata_value(&key);
            let record = record.clone();
            drop(records);
            Ok(record)
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
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;
            let record = records
                .get_mut(&id)
                .ok_or_else(|| SessionStoreError::NotFound { id: id.clone() })?;
            record.set_inference_settings(inference_settings);
            let record = record.clone();
            drop(records);
            Ok(record)
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
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;
            let record = records
                .get_mut(&id)
                .ok_or_else(|| SessionStoreError::NotFound { id: id.clone() })?;
            record.set_agent_definition(agent_definition);
            let record = record.clone();
            drop(records);
            Ok(record)
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
            let record = store.load(&id).await?;
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
            let record = store.load(&id).await?;
            timeline_item_text_from_record(&record, &item_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::PendingTimelineItemId;
    use crate::agent::session::{
        CancelReason, ExternalRequest, ExternalRequestKind, ExternalRequestStatus, RequestId,
        RequestRecord, SessionStatus,
    };
    use crate::{
        InferenceProviderSettings, ModelApi, ModelRef, ProviderId, ToolCall, ToolResult,
        timeline::{
            CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CHANNEL_THINKING, CHANNEL_TOOL_CALL,
            TimelineChannel, TimelineContent, TimelineCursor, TimelineEventKind, TimelineItemId,
            TimelineQuery, TimelineStatus,
        },
    };

    fn session_id() -> SessionId {
        SessionId::new("session-1")
    }

    fn request_record(id: &str) -> RequestRecord {
        RequestRecord {
            id: RequestId::new(id),
            turn: 1,
            model: ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
            canonical_request_hash: format!("canonical-{id}"),
            canonical_request: json!({ "id": id }),
            projected_request: json!({ "projected_id": id }),
        }
    }

    fn external_request(id: &str, status: ExternalRequestStatus) -> ExternalRequest {
        ExternalRequest {
            id: id.into(),
            kind: ExternalRequestKind::Permission,
            tool_call_id: Some(format!("tool-{id}")),
            prompt: format!("Allow {id}?"),
            payload: json!({ "id": id }),
            status,
        }
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall::new(id, "test-tool", json!({ "id": id }))
    }

    fn tool_result(id: &str) -> ToolResult {
        ToolResult::new(id, "test-tool", format!("result-{id}"))
    }

    fn pending_delta(id: &SessionId, index: usize) -> PendingTimelineEvent {
        PendingTimelineEvent::assistant_text_delta(
            id.clone(),
            0,
            PendingTimelineItemId::New,
            format!("delta-{index}"),
        )
    }

    #[tokio::test]
    async fn metadata_mutations_preserve_concurrent_journal_and_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let settings = InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: "updated".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        };
        let bundle = AgentDefinitionBundle::new("selected", vec!["kind: Agent".to_owned()]);

        let (append, inference, definition) = tokio::join!(
            store.append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "hello".to_owned(),
                }],
            ),
            store.set_inference_settings(&id, Some(settings.clone())),
            store.set_agent_definition(&id, Some(bundle.clone())),
        );

        assert_eq!(append.unwrap(), Revision::new(1));
        inference.unwrap();
        definition.unwrap();
        let record = store.load(&id).await.unwrap();
        assert_eq!(record.revision, Revision::new(1));
        assert_eq!(record.inference_settings, Some(settings));
        assert_eq!(record.agent_definition, Some(bundle));
        assert_eq!(record.journal.len(), 1);
    }

    fn pending_thinking_delta(id: &SessionId, index: usize) -> PendingTimelineEvent {
        PendingTimelineEvent::thinking_delta(
            id.clone(),
            0,
            PendingTimelineItemId::New,
            format!("thought-{index}"),
        )
    }

    fn pending_status(id: &SessionId, _index: usize) -> PendingTimelineEvent {
        let mut event = PendingTimelineEvent::new(
            id.clone(),
            0,
            PendingTimelineItemId::New,
            TimelineEventKind::SessionStatusChanged,
            None,
            TimelineContent::Empty,
        );
        event.status = Some(TimelineStatus::Idle);
        event
    }

    fn external_request_event(
        session_id: &SessionId,
        request: ExternalRequest,
    ) -> PendingTimelineEvent {
        PendingTimelineEvent::new(
            session_id.clone(),
            1,
            PendingTimelineItemId::New,
            TimelineEventKind::ExternalRequestOpened,
            Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
            TimelineContent::ExternalRequest { request },
        )
    }

    #[tokio::test]
    async fn create_and_load_clones_records() {
        let store = MemorySessionStore::new();
        let id = session_id();

        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let mut loaded = store.load(&id).await.unwrap();
        loaded.journal.push(SessionJournalEvent::PromptAppended {
            prompt: "mutated outside store".to_owned(),
        });

        let loaded_again = store.load(&id).await.unwrap();
        assert_eq!(loaded_again.id, id);
        assert_eq!(loaded_again.revision, Revision::ZERO);
        assert!(loaded_again.journal.is_empty());
    }

    #[tokio::test]
    async fn inference_creation_atomically_stores_settings_and_agent_definition_bundle() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let bundle = AgentDefinitionBundle::new(
            "selected",
            vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: selected\nspec: {}\n".to_owned()],
        );
        let inference_settings = InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: "https://provider.example/v1".to_owned(),
                model_id: "test-model".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        };

        let mut request = SessionCreateRequest::new(id.clone());
        request.inference_settings = Some(inference_settings.clone());
        request.agent_definition = Some(bundle.clone());
        let created = store.create(request).await.unwrap();
        let loaded = store.load(&id).await.unwrap();

        assert_eq!(created.agent_definition, Some(bundle.clone()));
        assert_eq!(loaded.agent_definition, Some(bundle));
        assert_eq!(created.inference_settings, Some(inference_settings.clone()));
        assert_eq!(loaded.inference_settings, Some(inference_settings));
        assert!(loaded.journal.is_empty());
    }

    #[tokio::test]
    async fn duplicate_inference_creation_does_not_overwrite_existing_record() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let original = AgentDefinitionBundle::new("original", Vec::new());
        let replacement = AgentDefinitionBundle::new("replacement", Vec::new());
        let mut request = SessionCreateRequest::new(id.clone());
        request.agent_definition = Some(original.clone());
        store.create(request).await.unwrap();

        let mut request = SessionCreateRequest::new(id.clone());
        request.agent_definition = Some(replacement);
        let error = store.create(request).await.unwrap_err();

        assert_eq!(error, SessionStoreError::AlreadyExists { id: id.clone() });
        assert_eq!(
            store.load(&id).await.unwrap().agent_definition,
            Some(original)
        );
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let store = MemorySessionStore::new();
        let id = session_id();

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
    async fn wipe_is_idempotent_and_removes_all_records() {
        let store = MemorySessionStore::new();

        store.wipe().await.unwrap();
        store
            .create(SessionCreateRequest::new("session-one"))
            .await
            .unwrap();
        store
            .create(SessionCreateRequest::new("session-two"))
            .await
            .unwrap();
        store.wipe().await.unwrap();
        store.wipe().await.unwrap();

        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_returns_session_summaries_newest_activity_first() {
        let store = MemorySessionStore::new();
        let first = SessionId::new("session-first");
        let second = SessionId::new("session-second");
        store
            .create(SessionCreateRequest::new(first.clone()))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        store
            .create(SessionCreateRequest::new(second.clone()))
            .await
            .unwrap();

        let summaries = store.list().await.unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, second);
        assert_eq!(summaries[1].id, first);
        assert_eq!(summaries[0].revision, Revision::ZERO);
        assert_eq!(summaries[0].status, SessionStatus::Idle);
    }

    #[tokio::test]
    async fn list_summary_reflects_latest_activity_and_status() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let created = store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "hello".to_owned(),
                }],
            )
            .await
            .unwrap();

        let summaries = store.list().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id);
        assert_eq!(summaries[0].revision, Revision::new(1));
        assert_eq!(summaries[0].status, SessionStatus::BuildingRequest);
        assert_eq!(summaries[0].current_prompt.as_deref(), Some("hello"));
        assert_eq!(summaries[0].created_at, created.metadata.created_at);
        assert!(summaries[0].last_activity_at > created.metadata.last_activity_at);
    }

    #[tokio::test]
    async fn metadata_values_are_stored_without_activity_bump() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let created = store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let updated = store
            .set_metadata_value(&id, "host.key".to_owned(), json!({ "value": true }))
            .await
            .unwrap();

        assert_eq!(updated.revision, Revision::ZERO);
        assert_eq!(
            updated.metadata_value("host.key"),
            Some(&json!({ "value": true }))
        );
        assert!(updated.metadata.updated_at > created.metadata.updated_at);
        assert_eq!(
            updated.metadata.last_activity_at,
            created.metadata.last_activity_at
        );

        let summary = updated.summary();
        assert_eq!(
            summary.metadata_values.get("host.key"),
            Some(&json!({ "value": true }))
        );
    }

    #[tokio::test]
    async fn absent_metadata_condition_treats_json_null_as_present_without_mutation() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let mut request = SessionCreateRequest::new(id.clone());
        request
            .metadata_values
            .insert("key".to_owned(), Value::Null);
        store.create(request).await.unwrap();
        let before = store.load(&id).await.unwrap();

        let outcome = store
            .set_metadata_value_conditionally(
                &id,
                "key".to_owned(),
                json!("replacement"),
                MetadataSetCondition::Absent,
            )
            .await
            .unwrap();

        assert!(!outcome.changed);
        assert_eq!(outcome.record, before);
        assert_eq!(outcome.record.metadata_value("key"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn successful_absent_metadata_set_preserves_journal_revision_and_other_values() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let mut request = SessionCreateRequest::new(id.clone());
        request.metadata_values.insert("other".to_owned(), json!(true));
        store.create(request).await.unwrap();
        store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "preserved".to_owned(),
                }],
            )
            .await
            .unwrap();

        let outcome = store
            .set_metadata_value_conditionally(
                &id,
                "new".to_owned(),
                json!({ "opaque": [null, 1] }),
                MetadataSetCondition::Absent,
            )
            .await
            .unwrap();

        assert!(outcome.changed);
        assert_eq!(outcome.record.revision, Revision::new(1));
        assert_eq!(outcome.record.journal.len(), 1);
        assert_eq!(outcome.record.metadata_value("other"), Some(&json!(true)));
        assert_eq!(
            outcome.record.metadata_value("new"),
            Some(&json!({ "opaque": [null, 1] }))
        );
    }

    #[tokio::test]
    async fn metadata_patch_does_not_stale_timeline_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        store
            .set_metadata_value(&id, "runtime.thinking".to_owned(), json!("high"))
            .await
            .unwrap();

        let assigned = store
            .append_timeline(&id, Revision::ZERO, vec![pending_delta(&id, 1)])
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        assert_eq!(assigned.len(), 1);
        assert_eq!(loaded.revision, Revision::new(1));
        assert_eq!(
            loaded.metadata_value("runtime.thinking"),
            Some(&json!("high"))
        );
    }

    #[tokio::test]
    async fn metadata_remove_does_not_stale_timeline_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        store
            .set_metadata_value(&id, "runtime.thinking".to_owned(), json!("high"))
            .await
            .unwrap();
        let removed = store
            .remove_metadata_value(&id, "runtime.thinking".to_owned())
            .await
            .unwrap();

        assert_eq!(removed.revision, Revision::ZERO);
        assert_eq!(removed.metadata_value("runtime.thinking"), None);

        store
            .append_timeline(&id, Revision::ZERO, vec![pending_delta(&id, 1)])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn append_journal_succeeds_and_increments_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let revision = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "hello".to_owned(),
                }],
            )
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        assert_eq!(revision, Revision::new(1));
        assert_eq!(loaded.revision, Revision::new(1));
        assert_eq!(loaded.journal.len(), 1);
    }

    #[tokio::test]
    async fn append_journal_fails_for_stale_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "first".to_owned(),
                }],
            )
            .await
            .unwrap();

        let error = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "stale".to_owned(),
                }],
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: Revision::ZERO,
                actual: Revision::new(1),
            }
        );
        assert_eq!(store.load(&id).await.unwrap().journal.len(), 1);
    }

    #[tokio::test]
    async fn append_timeline_fails_for_stale_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_timeline(&id, Revision::ZERO, vec![pending_delta(&id, 1)])
            .await
            .unwrap();

        let error = store
            .append_timeline(&id, Revision::ZERO, vec![pending_delta(&id, 2)])
            .await
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::StaleRevision { .. }));
        assert_eq!(store.load(&id).await.unwrap().timeline_events().count(), 1);
        let assigned = store
            .append_timeline(&id, Revision::new(1), vec![pending_delta(&id, 3)])
            .await
            .unwrap();
        assert_eq!(assigned[0].cursor, TimelineCursor::new(2));
        assert_eq!(assigned[0].item_id.as_str(), "i2");
    }

    #[tokio::test]
    async fn invalid_item_reference_does_not_mutate_record() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let missing = TimelineItemId::from_first_cursor(TimelineCursor::new(99));
        let error = store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![PendingTimelineEvent::assistant_text_delta(
                    id.clone(),
                    0,
                    missing.clone(),
                    "orphaned",
                )],
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SessionStoreError::TimelineItemNotFound {
                id: id.clone(),
                item_id: missing,
            }
        );
        let record = store.load(&id).await.unwrap();
        assert_eq!(record.revision, Revision::ZERO);
        assert_eq!(record.timeline_events().count(), 0);
    }

    #[tokio::test]
    async fn snapshot_is_derived_from_journal() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let call = tool_call("call-1");
        let result = tool_result("call-1");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let append_revision = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "hello".to_owned(),
                }],
            )
            .await
            .unwrap();
        let _assigned = store
            .append_timeline(
                &id,
                append_revision,
                vec![
                    PendingTimelineEvent::tool_call(
                        id.clone(),
                        1,
                        PendingTimelineItemId::New,
                        call.clone(),
                    ),
                    PendingTimelineEvent::tool_result(
                        id.clone(),
                        1,
                        PendingTimelineItemId::New,
                        TimelineItemId::from_first_cursor(TimelineCursor::new(1)),
                        result.clone(),
                    ),
                ],
            )
            .await
            .unwrap();
        let turn_revision = store
            .append_journal(
                &id,
                Revision::new(2),
                vec![
                    SessionJournalEvent::TurnFinished { turn: 1 },
                    SessionJournalEvent::Finished,
                ],
            )
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        let snapshot = loaded.snapshot();
        assert_eq!(turn_revision, Revision::new(3));
        assert_eq!(loaded.revision, Revision::new(3));
        assert_eq!(snapshot.revision, Revision::new(3));
        assert_eq!(snapshot.status, SessionStatus::Finished);
        assert_eq!(snapshot.turn, 2);
        assert_eq!(loaded.journal.len(), 5);
        assert_eq!(snapshot.tool_calls().collect::<Vec<_>>(), [&call]);
        assert_eq!(snapshot.tool_results().collect::<Vec<_>>(), [&result]);
    }

    #[tokio::test]
    async fn optimistic_revisions_advance_across_appends() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let append_revision = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "first".to_owned(),
                }],
            )
            .await
            .unwrap();
        let stale_append = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "stale".to_owned(),
                }],
            )
            .await
            .unwrap_err();

        let timeline_revision = store
            .append_timeline(&id, append_revision, vec![pending_delta(&id, 1)])
            .await
            .unwrap();
        let stale_timeline = store
            .append_timeline(&id, append_revision, vec![pending_delta(&id, 2)])
            .await
            .unwrap_err();

        assert_eq!(append_revision, Revision::new(1));
        assert_eq!(timeline_revision[0].cursor, TimelineCursor::new(1));
        assert_eq!(store.load(&id).await.unwrap().revision, Revision::new(2));
        assert_eq!(
            stale_append,
            SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: Revision::ZERO,
                actual: Revision::new(1),
            }
        );
        assert_eq!(
            stale_timeline,
            SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: Revision::new(1),
                actual: Revision::new(2),
            }
        );
    }

    #[tokio::test]
    async fn derived_snapshot_revision_matches_record_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let revision = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "hello".to_owned(),
                }],
            )
            .await
            .unwrap();
        let loaded = store.load(&id).await.unwrap();

        assert_eq!(revision, Revision::new(1));
        assert_eq!(loaded.snapshot().revision, loaded.revision);
    }

    #[tokio::test]
    async fn request_records_preserve_insertion_order() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let revision = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::ProviderRequestRecorded {
                    request: request_record("first"),
                }],
            )
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                revision,
                vec![SessionJournalEvent::ProviderRequestRecorded {
                    request: request_record("second"),
                }],
            )
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        let request_ids: Vec<&str> = loaded
            .provider_requests()
            .map(|request| request.id.as_str())
            .collect();
        assert_eq!(request_ids, ["first", "second"]);
    }

    #[tokio::test]
    async fn external_requests_project_insertion_order_and_cancellation_markers() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let _assigned = store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![
                    external_request_event(
                        &id,
                        external_request("first", ExternalRequestStatus::Pending),
                    ),
                    external_request_event(
                        &id,
                        external_request(
                            "second",
                            ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt),
                        ),
                    ),
                ],
            )
            .await
            .unwrap();
        let loaded = store.load(&id).await.unwrap();
        let requests = loaded.snapshot().external_requests;
        assert_eq!(requests[0].id.as_str(), "first");
        assert_eq!(requests[0].status, ExternalRequestStatus::Pending);
        assert_eq!(requests[1].id.as_str(), "second");
        assert_eq!(
            requests[1].status,
            ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt)
        );
    }

    #[tokio::test]
    async fn append_timeline_assigns_stable_monotonic_cursors() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let assigned = store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![pending_delta(&id, 1), pending_delta(&id, 2)],
            )
            .await
            .unwrap();
        let next = store
            .append_timeline(&id, Revision::new(1), vec![pending_delta(&id, 3)])
            .await
            .unwrap();

        assert_eq!(assigned[0].cursor, TimelineCursor::new(1));
        assert_eq!(assigned[1].cursor, TimelineCursor::new(2));
        assert_eq!(next[0].cursor, TimelineCursor::new(3));
        assert_eq!(assigned[0].item_id.as_str(), "i1");
        assert_eq!(assigned[1].item_id.as_str(), "i2");
        assert_eq!(next[0].item_id.as_str(), "i3");
        let loaded = store.load(&id).await.unwrap();
        assert_eq!(loaded.revision, Revision::new(2));
        assert_eq!(loaded.timeline_events().count(), 3);
    }

    #[tokio::test]
    async fn existing_item_updates_create_gaps_in_item_ids() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();

        let first = store
            .append_timeline(&id, Revision::ZERO, vec![pending_delta(&id, 1)])
            .await
            .unwrap();
        let first_id = first[0].item_id.clone();
        let assigned = store
            .append_timeline(
                &id,
                Revision::new(1),
                vec![
                    PendingTimelineEvent::assistant_text_delta(
                        id.clone(),
                        0,
                        first_id.clone(),
                        "continued",
                    ),
                    pending_thinking_delta(&id, 2),
                ],
            )
            .await
            .unwrap();

        assert_eq!(first_id.as_str(), "i1");
        assert_eq!(assigned[0].item_id, first_id);
        assert_eq!(assigned[1].item_id.as_str(), "i3");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn timeline_query_slices_by_direction_cursor_limit_and_channel() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![
                    pending_delta(&id, 1),
                    pending_thinking_delta(&id, 2),
                    pending_status(&id, 3),
                    pending_delta(&id, 4),
                    PendingTimelineEvent::tool_call(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        tool_call("5"),
                    ),
                ],
            )
            .await
            .unwrap();

        let after_start = store
            .timeline(&id, TimelineQuery::forward(None, 2))
            .await
            .unwrap();
        let after_two = store
            .timeline(&id, TimelineQuery::forward(Some(TimelineCursor::new(2)), 2))
            .await
            .unwrap();
        let before_end = store
            .timeline(&id, TimelineQuery::backward(None, 2))
            .await
            .unwrap();
        let before_four = store
            .timeline(
                &id,
                TimelineQuery::backward(Some(TimelineCursor::new(4)), 2),
            )
            .await
            .unwrap();
        let assistant_only = store
            .timeline(
                &id,
                TimelineQuery::forward(None, 10)
                    .with_channels([TimelineChannel::from(CHANNEL_ASSISTANT_TEXT)]),
            )
            .await
            .unwrap();
        let assistant_or_tool = store
            .timeline(
                &id,
                TimelineQuery::forward(None, 10).with_channels([
                    TimelineChannel::from(CHANNEL_ASSISTANT_TEXT),
                    TimelineChannel::from(CHANNEL_TOOL_CALL),
                ]),
            )
            .await
            .unwrap();
        let zero_limit = store
            .timeline(&id, TimelineQuery::forward(None, 0))
            .await
            .unwrap();

        assert_eq!(
            after_start
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(1), TimelineCursor::new(2)]
        );
        assert_eq!(
            after_two
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(3), TimelineCursor::new(4)]
        );
        assert_eq!(
            before_end
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(4), TimelineCursor::new(5)]
        );
        assert_eq!(
            before_four
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(2), TimelineCursor::new(3)]
        );
        assert_eq!(
            assistant_only
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [TimelineCursor::new(1), TimelineCursor::new(4)]
        );
        assert_eq!(
            assistant_or_tool
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            [
                TimelineCursor::new(1),
                TimelineCursor::new(4),
                TimelineCursor::new(5)
            ]
        );
        assert!(zero_limit.is_empty());
    }

    #[tokio::test]
    async fn timeline_item_text_reconstructs_text_like_content() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let item_id = TimelineItemId::from_first_cursor(TimelineCursor::new(1));
        let mut snapshot = PendingTimelineEvent::new(
            id.clone(),
            0,
            item_id.clone(),
            TimelineEventKind::ItemUpdated,
            Some(TimelineChannel::from(CHANNEL_ASSISTANT_TEXT)),
            TimelineContent::TextSnapshot {
                text: "reset".to_owned(),
            },
        );
        snapshot.status = Some(TimelineStatus::StreamingProvider);
        store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![
                    PendingTimelineEvent::assistant_text_delta(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        "hello ",
                    ),
                    pending_thinking_delta(&id, 1),
                    snapshot,
                    PendingTimelineEvent::assistant_text_delta(
                        id.clone(),
                        0,
                        item_id.clone(),
                        " world",
                    ),
                ],
            )
            .await
            .unwrap();

        let text = store.timeline_item_text(&id, &item_id).await.unwrap();

        assert_eq!(text.item_id, item_id);
        assert_eq!(text.text, "reset world");
        assert_eq!(text.bytes, "reset world".len() as u64);
        assert_eq!(text.last_cursor, Some(TimelineCursor::new(4)));
    }

    #[tokio::test]
    async fn timeline_item_text_distinguishes_missing_non_text_and_refs() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let tool_item = TimelineItemId::from_first_cursor(TimelineCursor::new(1));
        let ref_item = TimelineItemId::from_first_cursor(TimelineCursor::new(2));
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![
                    PendingTimelineEvent::tool_call(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        tool_call("non-text"),
                    ),
                    PendingTimelineEvent::new(
                        id.clone(),
                        0,
                        PendingTimelineItemId::New,
                        TimelineEventKind::ItemUpdated,
                        Some(TimelineChannel::from(CHANNEL_THINKING)),
                        TimelineContent::content_ref("content-1", 100),
                    ),
                ],
            )
            .await
            .unwrap();

        let tool_text = store.timeline_item_text(&id, &tool_item).await.unwrap();
        let missing = store
            .timeline_item_text(
                &id,
                &TimelineItemId::from_first_cursor(TimelineCursor::new(999)),
            )
            .await
            .unwrap_err();
        let unavailable = store.timeline_item_text(&id, &ref_item).await.unwrap_err();

        assert_eq!(tool_text.text, "");
        assert_eq!(tool_text.bytes, 0);
        assert!(matches!(
            missing,
            SessionStoreError::TimelineItemNotFound { .. }
        ));
        assert!(matches!(
            unavailable,
            SessionStoreError::TimelineContentUnavailable { .. }
        ));
    }
}
