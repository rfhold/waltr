//! Durable live semantic notifications at the agent consumer boundary.

use crate::scripted_provider::ScriptedProvider;
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::sync::{Arc, Mutex};
use waltr_agent::*;

#[derive(Default)]
struct Sink(Mutex<Vec<SessionSemanticEvent>>);
impl SessionEventSink for Sink {
    fn send(&self, _event: SessionEvent) -> SessionEventSinkFuture {
        Box::pin(async { Ok(()) })
    }
    fn semantic(&self, event: SessionSemanticEvent) {
        self.0.lock().unwrap().push(event);
    }
}
fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses)
}

#[tokio::test]
async fn legacy_context_validation_never_initializes_an_incarnation_implicitly() {
    let directory = tempfile::tempdir().unwrap();
    let store = FilesystemSessionStore::new(directory.path());
    let id = SessionId::new("legacy");
    let mut request = SessionCreateRequest::new(id.clone());
    request.inference_settings = Some(InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".into(),
            api: ModelApi::OpenAiResponses,
            url: "https://unused.invalid".into(),
            model_id: "test".into(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
    });
    request.agent_definition = Some(json!({"host": "opaque-test-definition"}));
    store.create(request).await.unwrap();
    store
        .append_timeline(
            &id,
            store.load(&id).await.unwrap().execution_token().unwrap(),
            vec![PendingTimelineEvent::assistant_text_snapshot(
                id.clone(),
                0,
                PendingTimelineItemId::New,
                "completed",
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
    let path = directory.path().join("legacy.json");
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    legacy["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("incarnation");
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let sessions = SessionCoordinator::new(store.clone());
    let before = std::fs::read(&path).unwrap();
    assert!(
        sessions
            .capture_context(&id, "principal", SessionCaptureLimits::default())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(
        !store
            .load(&id)
            .await
            .unwrap()
            .metadata
            .values
            .contains_key("_contextIncarnation")
    );
    legacy["metadata"]["values"]["_contextIncarnation"] = json!("forged");
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert!(sessions.capture_context(&id, "principal", SessionCaptureLimits::default()).await.is_err());
    let before = std::fs::read(&path).unwrap();
    assert!(
        sessions
            .capture_context(&id, "", SessionCaptureLimits::default())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn committed_turns_do_not_treat_result_text_or_forged_task_metadata_as_task_completion() {
    let store = MemorySessionStore::new();
    let id = SessionId::new("task-child");
    let mut request = SessionCreateRequest::new(id.clone());
    request
        .metadata_values
        .insert("host.parent".into(), json!("parent"));
    request
        .metadata_values
        .insert("host.turn".into(), json!(4));
    request
        .metadata_values
        .insert("host.call".into(), json!("parent-call"));
    store.create(request).await.unwrap();
    let sessions = SessionCoordinator::new(store.clone());
    let sink = Arc::new(Sink::default());
    let provider = Arc::new(ScriptedProvider::from_turns(
        "semantic",
        vec![
            vec![
                CompletionEvent::ToolCall(ToolCall::new("accepted", "task", json!({}))),
                CompletionEvent::Finished,
            ],
            vec![
                CompletionEvent::Delta("done".into()),
                CompletionEvent::Finished,
            ],
        ],
    ));
    // An accepted async task result is deliberately not a completed-task fact.
    let tool = AgentTool::new(
        ToolDefinition::new("task", "test", json!({"type":"object"})),
        |ctx| Ok(ctx.result("{\"status\":\"running\"}")),
    );
    let runtime = AgentRuntime::builder(model(), provider)
        .with_tool(tool)
        .build();
    let result = sessions
        .prompt(&runtime, id.clone(), "run")
        .with_session_event_sink(sink.clone())
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(result.iter().all(Result::is_ok));
    let facts = sink.0.lock().unwrap().clone();
    assert_eq!(
        facts
            .iter()
            .filter(|event| matches!(event.kind, SessionSemanticKind::TurnCompleted { .. }))
            .count(),
        2
    );
    assert_eq!(
        facts
            .iter()
            .filter(|event| matches!(event.kind, SessionSemanticKind::ToolResultCommitted { .. }))
            .count(),
        1
    );
    let finished = facts
        .iter()
        .filter(|event| matches!(event.kind, SessionSemanticKind::RunFinished))
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert!(matches!(facts.last().unwrap().kind, SessionSemanticKind::RunFinished));
    let record = store.load(&id).await.unwrap();
    assert!(facts.iter().all(|event| event.revision <= record.revision
        && event.incarnation == record.metadata.incarnation));
    assert_eq!(
        facts
            .iter()
            .map(|event| &event.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        facts.len()
    );
    // Loading, copying, and materializing durable history do not redeliver semantic facts.
    let reloaded: SessionRecord =
        serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap();
    drop(reloaded.materialize_context(SessionContextMode::Effective));
    assert_eq!(sink.0.lock().unwrap().len(), facts.len());
}

struct StaleCommitProvider {
    store: MemorySessionStore,
    id: SessionId,
}
impl LlmStream for StaleCommitProvider {
    fn name(&self) -> &'static str {
        "stale-commit"
    }
    fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let record = self.store.load(&self.id).await.unwrap();
            self.store
                .append_journal(
                    &self.id,
                    record.execution_token().unwrap(),
                    vec![SessionJournalEvent::MiddlewareStateUpdated {
                        namespace: "test-race".into(),
                        state: json!({}),
                    }],
                )
                .await
                .unwrap();
            Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)])) as BoxCompletionStream)
        })
    }
}

#[tokio::test]
async fn failed_completed_turn_append_emits_no_semantic_fact() {
    let store = MemorySessionStore::new();
    let id = SessionId::new("stale");
    store
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let runtime = AgentRuntime::builder(
        model(),
        Arc::new(StaleCommitProvider {
            store: store.clone(),
            id: id.clone(),
        }),
    )
    .build();
    let sessions = SessionCoordinator::new(store);
    let sink = Arc::new(Sink::default());
    let result = sessions
        .prompt(&runtime, id, "run")
        .with_session_event_sink(sink.clone())
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(result.iter().any(Result::is_err));
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn semantic_subscribers_and_lag_are_bounded_without_backfill() {
    assert!(SessionSemanticHub::new(0).is_err());
    assert!(SessionSemanticHub::new(1025).is_err());
    let hub = SessionSemanticHub::new(1).unwrap();
    let mut subscriptions = (0..32)
        .map(|_| hub.subscribe().unwrap())
        .collect::<Vec<_>>();
    assert!(hub.subscribe().is_err());
    let event = SessionSemanticEvent {
        id: "first".into(),
        session_id: SessionId::new("source"),
        incarnation: Some("incarnation".into()),
        revision: Revision::new(1),
        cursor: Some(TimelineCursor::new(1)),
        kind: SessionSemanticKind::TurnCompleted { turn: 0 },
    };
    hub.publish(event.clone()).unwrap();
    hub.publish(SessionSemanticEvent {
        id: "second".into(),
        ..event
    })
    .unwrap();
    assert!(matches!(
        subscriptions[0].recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(1))
    ));
    assert_eq!(subscriptions[0].recv().await.unwrap().id, "second");
    subscriptions.pop();
    let mut fresh = hub.subscribe().unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), fresh.recv())
            .await
            .is_err()
    );
    drop(subscriptions);
    drop(hub);
    assert!(matches!(
        fresh.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Closed)
    ));
}
