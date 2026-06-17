//! Process-memory session store.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use super::{
    SessionStore, SessionStoreError, SessionStoreResult, append_timeline_to_record,
    timeline_after_slice, timeline_before_slice,
};
use crate::{
    agent::session::{Revision, SessionId, SessionJournalEvent, SessionRecord},
    timeline::{AgentTimelineEvent, PendingTimelineEvent, TimelineCursor},
};

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
        let value = update(record, next_revision)?;
        record.revision = next_revision;
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
            let record = store.load(&id).await?;
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
            let record = store.load(&id).await?;
            Ok(timeline_before_slice(&record, cursor, limit))
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
        timeline::{CHANNEL_EXTERNAL_REQUEST, TimelineChannel, TimelineContent, TimelineEventKind},
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
    async fn timeline_after_and_before_slice_by_cursor() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(id.clone()).await.unwrap();
        store
            .append_timeline(
                &id,
                Revision::ZERO,
                (1..=5).map(|index| pending_delta(&id, index)).collect(),
            )
            .await
            .unwrap();

        let after_start = store.timeline_after(&id, None, 2).await.unwrap();
        let after_two = store
            .timeline_after(&id, Some(TimelineCursor::new(2)), 2)
            .await
            .unwrap();
        let before_end = store.timeline_before(&id, None, 2).await.unwrap();
        let before_four = store
            .timeline_before(&id, Some(TimelineCursor::new(4)), 2)
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
    }
}
