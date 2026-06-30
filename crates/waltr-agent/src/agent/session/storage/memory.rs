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
    agent::session::{Revision, SessionId, SessionJournalEvent, SessionRecord, SessionSummary},
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
            .get_mut(id)
            .ok_or_else(|| SessionStoreError::NotFound { id: id.clone() })?;

        if record.revision != expected_revision {
            return Err(SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: expected_revision,
                actual: record.revision,
            });
        }

        let next_revision = record.revision.next();
        record.revision = next_revision;
        let value = update(record, next_revision)?;
        drop(records);

        Ok((next_revision, value))
    }
}

impl SessionStore for MemorySessionStore {
    fn create(
        &self,
        id: SessionId,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = SessionRecord::new(id);
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

    fn create_with_metadata_values(
        &self,
        id: SessionId,
        metadata_values: BTreeMap<String, Value>,
    ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
        let store = self.clone();
        async move {
            let record = SessionRecord::with_metadata_values(id, metadata_values);
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

    fn set_metadata_value(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
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
            record.set_metadata_value(key, value);
            let record = record.clone();
            drop(records);
            Ok(record)
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
    use crate::agent::session::{
        CancelReason, ExternalRequest, ExternalRequestKind, ExternalRequestStatus, RequestId,
        RequestRecord, SessionStatus,
    };
    use crate::{
        ModelApi, ModelRef, ProviderId, ToolCall, ToolResult,
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
            format!("assistant-{index}"),
            format!("delta-{index}"),
        )
    }

    fn pending_thinking_delta(id: &SessionId, index: usize) -> PendingTimelineEvent {
        PendingTimelineEvent::thinking_delta(
            id.clone(),
            0,
            format!("thinking-{index}"),
            format!("thought-{index}"),
        )
    }

    fn pending_status(id: &SessionId, index: usize) -> PendingTimelineEvent {
        let mut event = PendingTimelineEvent::new(
            id.clone(),
            0,
            format!("status-{index}"),
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
            format!("external-request:{}", request.id.as_str()),
            TimelineEventKind::ExternalRequestOpened,
            Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
            TimelineContent::ExternalRequest { request },
        )
    }

    #[tokio::test]
    async fn create_and_load_clones_records() {
        let store = MemorySessionStore::new();
        let id = session_id();

        store.create(id.clone()).await.unwrap();

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
    async fn delete_is_idempotent() {
        let store = MemorySessionStore::new();
        let id = session_id();

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
    async fn list_returns_session_summaries_newest_activity_first() {
        let store = MemorySessionStore::new();
        let first = SessionId::new("session-first");
        let second = SessionId::new("session-second");
        store.create(first.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        store.create(second.clone()).await.unwrap();

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
        let created = store.create(id.clone()).await.unwrap();
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
        let created = store.create(id.clone()).await.unwrap();
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
    async fn metadata_patch_does_not_stale_timeline_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();
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
        store.create(id.clone()).await.unwrap();
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
    }

    #[tokio::test]
    async fn snapshot_is_derived_from_journal() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let call = tool_call("call-1");
        let result = tool_result("call-1");
        store.create(id.clone()).await.unwrap();

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
                        "tool-call:call-1",
                        call.clone(),
                    ),
                    PendingTimelineEvent::tool_result(
                        id.clone(),
                        1,
                        "tool-result:call-1",
                        "tool-call:call-1",
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
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();

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
        store.create(id.clone()).await.unwrap();

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
        let loaded = store.load(&id).await.unwrap();
        assert_eq!(loaded.revision, Revision::new(2));
        assert_eq!(loaded.timeline_events().count(), 3);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn timeline_query_slices_by_direction_cursor_limit_and_channel() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(id.clone()).await.unwrap();
        store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![
                    pending_delta(&id, 1),
                    pending_thinking_delta(&id, 2),
                    pending_status(&id, 3),
                    pending_delta(&id, 4),
                    PendingTimelineEvent::tool_call(id.clone(), 0, "tool-call:5", tool_call("5")),
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
        store.create(id.clone()).await.unwrap();
        let item_id = TimelineItemId::new("assistant-shared");
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
                        item_id.clone(),
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
        let tool_item = TimelineItemId::new("tool-call:non-text");
        let ref_item = TimelineItemId::new("content-ref");
        store.create(id.clone()).await.unwrap();
        store
            .append_timeline(
                &id,
                Revision::ZERO,
                vec![
                    PendingTimelineEvent::tool_call(
                        id.clone(),
                        0,
                        tool_item.clone(),
                        tool_call("non-text"),
                    ),
                    PendingTimelineEvent::new(
                        id.clone(),
                        0,
                        ref_item.clone(),
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
            .timeline_item_text(&id, &TimelineItemId::new("missing"))
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
