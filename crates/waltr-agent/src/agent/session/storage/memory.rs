//! Process-memory session store.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use super::{SessionStore, SessionStoreError, SessionStoreResult};
use crate::agent::session::{
    ExternalRequest, RequestRecord, Revision, SessionEvent, SessionId, SessionRecord,
    SessionSnapshot,
};

/// Process-memory implementation of [`SessionStore`].
#[derive(Debug, Clone, Default)]
pub struct MemorySessionStore {
    records: Arc<Mutex<HashMap<SessionId, SessionRecord>>>,
}

impl MemorySessionStore {
    /// Create an empty process-memory session store.
    pub fn new() -> Self {
        Self::default()
    }

    fn with_record_mut<T>(
        &self,
        id: &SessionId,
        expected_revision: Revision,
        update: impl FnOnce(&mut SessionRecord, Revision) -> T,
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
        let value = update(record, next_revision);
        record.revision = next_revision;

        Ok((next_revision, value))
    }
}

impl SessionStore for MemorySessionStore {
    fn create(
        &self,
        record: SessionRecord,
    ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
        let store = self.clone();
        async move {
            let mut records = store
                .records
                .lock()
                .map_err(|_| SessionStoreError::LockPoisoned)?;

            if records.contains_key(&record.id) {
                return Err(SessionStoreError::AlreadyExists { id: record.id });
            }

            records.insert(record.id.clone(), record);
            Ok(())
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
                .ok_or_else(|| SessionStoreError::NotFound { id })
        }
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
            let (revision, ()) = store.with_record_mut(&id, expected_revision, |record, _| {
                record.events.extend(events);
            })?;

            Ok(revision)
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

            let (revision, ()) =
                store.with_record_mut(&id, expected_revision, |record, revision| {
                    snapshot.revision = revision;
                    record.snapshot = snapshot;
                })?;

            Ok(revision)
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
            let (revision, ()) = store.with_record_mut(&id, expected_revision, |record, _| {
                record.requests.push(request);
            })?;

            Ok(revision)
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
            let (revision, ()) =
                store.with_record_mut(&id, expected_revision, |record, revision| {
                    record.snapshot.external_requests.push(request);
                    record.snapshot.revision = revision;
                })?;

            Ok(revision)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::AgentTranscriptEvent;
    use crate::agent::session::{
        CancelReason, ExternalRequestKind, ExternalRequestStatus, PendingWork, PendingWorkStatus,
        SessionStatus, Suspension,
    };
    use crate::{ModelApi, ModelRef, ProviderId, ToolCall, ToolResult};

    fn session_id() -> SessionId {
        SessionId::new("session-1")
    }

    fn request_record(id: &str) -> RequestRecord {
        RequestRecord {
            id: id.into(),
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

    fn pending_work(id: &str) -> PendingWork {
        PendingWork {
            id: id.to_owned(),
            turn: 1,
            tool_call: Some(tool_call(id)),
            status: PendingWorkStatus::Pending,
        }
    }

    fn snapshot_status(status: SessionStatus) -> SessionSnapshot {
        let mut snapshot = SessionSnapshot::new(session_id());
        snapshot.status = status;
        snapshot
    }

    fn rebuild_snapshot_from_events(record: &SessionRecord) -> SessionSnapshot {
        let mut snapshot = SessionSnapshot::new(record.id.clone());
        snapshot.apply_events(&record.events);
        snapshot
    }

    #[tokio::test]
    async fn create_and_load_clones_records() {
        let store = MemorySessionStore::new();
        let id = session_id();

        store.create(SessionRecord::new(id.clone())).await.unwrap();

        let mut loaded = store.load(&id).await.unwrap();
        loaded.events.push(SessionEvent::PromptAppended {
            prompt: "mutated outside store".to_owned(),
        });

        let loaded_again = store.load(&id).await.unwrap();
        assert_eq!(loaded_again.id, id);
        assert_eq!(loaded_again.revision, Revision::ZERO);
        assert!(loaded_again.events.is_empty());
    }

    #[tokio::test]
    async fn append_succeeds_and_increments_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(SessionRecord::new(id.clone())).await.unwrap();

        let revision = store
            .append(
                &id,
                vec![SessionEvent::PromptAppended {
                    prompt: "hello".to_owned(),
                }],
                Revision::ZERO,
            )
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        assert_eq!(revision, Revision::new(1));
        assert_eq!(loaded.revision, Revision::new(1));
        assert_eq!(loaded.events.len(), 1);
    }

    #[tokio::test]
    async fn append_fails_for_stale_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(SessionRecord::new(id.clone())).await.unwrap();
        store
            .append(
                &id,
                vec![SessionEvent::PromptAppended {
                    prompt: "first".to_owned(),
                }],
                Revision::ZERO,
            )
            .await
            .unwrap();

        let error = store
            .append(
                &id,
                vec![SessionEvent::PromptAppended {
                    prompt: "stale".to_owned(),
                }],
                Revision::ZERO,
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
        assert_eq!(store.load(&id).await.unwrap().events.len(), 1);
    }

    #[tokio::test]
    async fn save_snapshot_fails_for_stale_revision() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(SessionRecord::new(id.clone())).await.unwrap();
        let revision = store
            .append(
                &id,
                vec![SessionEvent::PromptAppended {
                    prompt: "first".to_owned(),
                }],
                Revision::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(revision, Revision::new(1));

        let mut snapshot = SessionSnapshot::new(id.clone());
        snapshot.status = SessionStatus::Finished;

        let error = store
            .save_snapshot(&id, snapshot, Revision::ZERO)
            .await
            .unwrap_err();

        assert!(matches!(error, SessionStoreError::StaleRevision { .. }));
        assert_eq!(
            store.load(&id).await.unwrap().snapshot.status,
            SessionStatus::Idle
        );
    }

    #[tokio::test]
    async fn appended_events_can_be_rebuilt_saved_and_loaded_as_snapshot() {
        let store = MemorySessionStore::new();
        let id = session_id();
        let call = tool_call("call-1");
        let result = tool_result("call-1");
        store.create(SessionRecord::new(id.clone())).await.unwrap();

        let append_revision = store
            .append(
                &id,
                vec![
                    SessionEvent::PromptAppended {
                        prompt: "hello".to_owned(),
                    },
                    SessionEvent::ProviderTextDelta {
                        turn: 1,
                        delta: "answer".to_owned(),
                    },
                    SessionEvent::ProviderThinkingDelta {
                        turn: 1,
                        delta: "reasoning".to_owned(),
                    },
                    SessionEvent::ToolCallRecorded {
                        turn: 1,
                        call: call.clone(),
                    },
                    SessionEvent::ToolResultRecorded {
                        turn: 1,
                        result: result.clone(),
                    },
                    SessionEvent::TurnFinished { turn: 1 },
                    SessionEvent::Finished,
                ],
                Revision::ZERO,
            )
            .await
            .unwrap();

        let rebuilt = rebuild_snapshot_from_events(&store.load(&id).await.unwrap());
        let snapshot_revision = store
            .save_snapshot(&id, rebuilt, append_revision)
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        assert_eq!(append_revision, Revision::new(1));
        assert_eq!(snapshot_revision, Revision::new(2));
        assert_eq!(loaded.revision, Revision::new(2));
        assert_eq!(loaded.snapshot.revision, Revision::new(2));
        assert_eq!(loaded.snapshot.status, SessionStatus::Finished);
        assert_eq!(loaded.snapshot.turn, 2);
        assert_eq!(loaded.events.len(), 7);
        assert_eq!(
            loaded.snapshot.state.events,
            vec![
                AgentTranscriptEvent::Text {
                    turn: 1,
                    text: "answer".to_owned(),
                },
                AgentTranscriptEvent::Thinking {
                    turn: 1,
                    text: "reasoning".to_owned(),
                },
                AgentTranscriptEvent::ToolCall { turn: 1, call },
                AgentTranscriptEvent::ToolResult { turn: 1, result },
            ]
        );
    }

    #[tokio::test]
    async fn optimistic_revisions_advance_across_append_and_snapshot_save() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(SessionRecord::new(id.clone())).await.unwrap();

        let append_revision = store
            .append(
                &id,
                vec![SessionEvent::TextDelta {
                    turn: 0,
                    delta: "first".to_owned(),
                }],
                Revision::ZERO,
            )
            .await
            .unwrap();
        let stale_append = store
            .append(
                &id,
                vec![SessionEvent::TextDelta {
                    turn: 0,
                    delta: "stale".to_owned(),
                }],
                Revision::ZERO,
            )
            .await
            .unwrap_err();

        let rebuilt = rebuild_snapshot_from_events(&store.load(&id).await.unwrap());
        let snapshot_revision = store
            .save_snapshot(&id, rebuilt, append_revision)
            .await
            .unwrap();
        let stale_snapshot = store
            .save_snapshot(&id, SessionSnapshot::new(id.clone()), append_revision)
            .await
            .unwrap_err();

        assert_eq!(append_revision, Revision::new(1));
        assert_eq!(snapshot_revision, Revision::new(2));
        assert_eq!(
            stale_append,
            SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: Revision::ZERO,
                actual: Revision::new(1),
            }
        );
        assert_eq!(
            stale_snapshot,
            SessionStoreError::StaleRevision {
                id: id.clone(),
                expected: Revision::new(1),
                actual: Revision::new(2),
            }
        );
    }

    #[tokio::test]
    async fn save_snapshot_persists_status_transitions() {
        let statuses = vec![
            SessionStatus::BuildingRequest,
            SessionStatus::StreamingProvider,
            SessionStatus::EvaluatingToolCall,
            SessionStatus::RunningTool,
            SessionStatus::Suspended(Suspension::AwaitingToolResults {
                work: vec![pending_work("call-1")],
            }),
            SessionStatus::Finished,
            SessionStatus::Failed {
                message: "provider failed".to_owned(),
            },
        ];

        for (index, status) in statuses.into_iter().enumerate() {
            let store = MemorySessionStore::new();
            let id = session_id();
            store.create(SessionRecord::new(id.clone())).await.unwrap();

            let revision = store
                .save_snapshot(&id, snapshot_status(status.clone()), Revision::ZERO)
                .await
                .unwrap();
            let loaded = store.load(&id).await.unwrap();

            assert_eq!(revision, Revision::new(1), "status index {index}");
            assert_eq!(loaded.revision, Revision::new(1), "status index {index}");
            assert_eq!(
                loaded.snapshot.revision,
                Revision::new(1),
                "status index {index}"
            );
            assert_eq!(loaded.snapshot.status, status, "status index {index}");
        }
    }

    #[tokio::test]
    async fn request_records_preserve_insertion_order() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(SessionRecord::new(id.clone())).await.unwrap();

        let revision = store
            .record_request(&id, request_record("first"), Revision::ZERO)
            .await
            .unwrap();
        store
            .record_request(&id, request_record("second"), revision)
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        let request_ids: Vec<&str> = loaded
            .requests
            .iter()
            .map(|request| request.id.as_str())
            .collect();
        assert_eq!(request_ids, ["first", "second"]);
    }

    #[tokio::test]
    async fn external_requests_preserve_insertion_order_and_cancellation_markers() {
        let store = MemorySessionStore::new();
        let id = session_id();
        store.create(SessionRecord::new(id.clone())).await.unwrap();

        let revision = store
            .record_external_request(
                &id,
                external_request("first", ExternalRequestStatus::Pending),
                Revision::ZERO,
            )
            .await
            .unwrap();
        store
            .record_external_request(
                &id,
                external_request(
                    "second",
                    ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt),
                ),
                revision,
            )
            .await
            .unwrap();

        let loaded = store.load(&id).await.unwrap();
        let requests = loaded.snapshot.external_requests;
        assert_eq!(requests[0].id.as_str(), "first");
        assert_eq!(requests[0].status, ExternalRequestStatus::Pending);
        assert_eq!(requests[1].id.as_str(), "second");
        assert_eq!(
            requests[1].status,
            ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt)
        );
    }
}
