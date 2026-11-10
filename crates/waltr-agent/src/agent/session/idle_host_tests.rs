use super::*;
use crate::{
    AgentTool, HostDispatchBinding, MemorySessionStore, ToolCall, ToolDefinition, ToolResult,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn audit(session: SessionId) -> impl FnOnce(&ToolResult) -> PendingTimelineEvent + Send + 'static {
    move |result| {
        PendingTimelineEvent::new(
            session,
            0,
            crate::PendingTimelineItemId::New,
            crate::TimelineEventKind::ItemFinished,
            Some(crate::TimelineChannel::from("host_audit")),
            crate::TimelineContent::Json {
                value: json!({"call": result.call_id}),
            },
        )
    }
}
fn bind(tool: AgentTool) -> AgentTool {
    tool.with_identity("opaque-host-target")
        .with_host_dispatch(|_| {
            Ok(HostDispatchBinding {
                invocation_id: uuid::Uuid::now_v7().to_string(),
                source_revision: "source-v1".into(),
            })
        })
}

#[tokio::test]
async fn idle_host_effect_settles_once_and_never_replays_an_uncertain_effect() {
    for uncertain in [false, true] {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone());
        let id = SessionId::generate();
        sessions
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let tool = bind(AgentTool::new_async(
            ToolDefinition::new("host", "host", json!({"type":"object"})),
            move |context| {
                count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    context
                        .result_metadata()
                        .set_context("host-state".into(), json!({"persisted":true}))?;
                    if uncertain {
                        Err(coordinator_error("unknown upstream outcome"))
                    } else {
                        Ok(ToolResult::new(&context.call().id, "host", "known result"))
                    }
                })
            },
        ));
        let call = ToolCall::new("effect", "host", json!({}));
        let record = store.load(&id).await.unwrap();
        let first = sessions
            .execute_idle_host_tool(
                &id,
                record.revision,
                tool.clone(),
                call.clone(),
                audit(id.clone()),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(first.is_err(), uncertain);
        let record = store.load(&id).await.unwrap();
        assert_eq!(
            record.snapshot().middleware_state["host-state"],
            json!({"persisted":true})
        );
        let second = sessions
            .execute_idle_host_tool(
                &id,
                record.revision,
                tool,
                call,
                audit(id.clone()),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(second.is_err(), uncertain);
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        assert!(
            record
                .timeline_events()
                .all(|event| !matches!(event.content, crate::TimelineContent::ToolCall { .. }))
        );
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn caller_drop_cancels_but_joins_the_original_effect_and_retains_its_fence() {
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = SessionId::generate();
    sessions
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let tool = bind(AgentTool::new_async(
        ToolDefinition::new("host", "host", json!({"type":"object"})),
        move |context| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
                context.cancellation_token().cancelled().await;
                context
                    .result_metadata()
                    .set_context("host-state".into(), json!({"cancelObserved":true}))?;
                Err(coordinator_error("upstream canceled after dispatch"))
            })
        },
    ));
    let runner = sessions.clone();
    let session = id.clone();
    let record = store.load(&id).await.unwrap();
    let waiter = tokio::spawn(async move {
        runner
            .execute_idle_host_tool(
                &session,
                record.revision,
                tool,
                ToolCall::new("cancelled-effect", "host", json!({})),
                audit(session.clone()),
                CancellationToken::new(),
            )
            .await
    });
    entered.notified().await;
    waiter.abort();
    drop(waiter.await);
    sessions.shutdown().await;
    let snapshot = store.load(&id).await.unwrap().snapshot();
    assert_eq!(
        snapshot.middleware_state["host-state"],
        json!({"cancelObserved":true})
    );
    assert_eq!(
        snapshot.middleware_state["host-dispatch"]["active"],
        json!(true)
    );
}
