use super::session::SessionRollbackTarget;
use crate::*;
use futures_util::StreamExt as _;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Provider;
impl LlmStream for Provider {
    fn name(&self) -> &'static str {
        "dispatch-test"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let done = request
            .turns
            .iter()
            .any(|turn| turn.tool_results().next().is_some());
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(if done {
                    CompletionEvent::Delta("done".into())
                } else {
                    CompletionEvent::ToolCall(ToolCall::new("stable-call", "code", json!({})))
                }),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}
fn runtime(tool: AgentTool) -> AgentRuntime {
    AgentRuntime::builder(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        Arc::new(Provider),
    )
    .with_tool(tool)
    .build()
}
fn binding() -> HostDispatchBinding {
    HostDispatchBinding {
        invocation_id: "reserved-invocation".into(),
        source_revision: "source-revision".into(),
    }
}

#[tokio::test]
async fn host_execution_guard_checks_before_tool_lifecycle_and_again_before_effect() {
    for allowed_checks in 0..=4 {
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let tool = AgentTool::new(
            ToolDefinition::new("code", "test", json!({})),
            move |context| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(context.result("done"))
            },
        );
        let checks = Arc::new(AtomicUsize::new(0));
        let guard_checks = checks.clone();
        let runtime = runtime(tool).with_tool_execution_guard(move |_| {
            let checks = guard_checks.clone();
            Box::pin(async move {
                if checks.fetch_add(1, Ordering::SeqCst) < allowed_checks {
                    Ok(())
                } else {
                    Err(LlmError::Protocol {
                        provider: "host",
                        message: "authority ended".into(),
                    })
                }
            })
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap()
            .id()
            .clone();
        drop(
            sessions
                .prompt(&runtime, id, "execute")
                .stream()
                .collect::<Vec<_>>()
                .await,
        );
        assert_eq!(checks.load(Ordering::SeqCst), (allowed_checks + 1).min(4));
        assert_eq!(
            effects.load(Ordering::SeqCst),
            usize::from(allowed_checks == 4)
        );
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn host_dispatch_commits_before_polling_and_closes_after_result() {
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = sessions
        .create(SessionCreateRequest::new("host-session"))
        .await
        .unwrap()
        .id()
        .clone();
    let read = store.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let effects = count.clone();
    let tool=AgentTool::new_async(ToolDefinition::new("code","test",json!({})),move |context| {
        let store=read.clone();let effects=effects.clone();Box::pin(async move {
            assert_eq!(context.host_dispatch(),Some(&binding()));
            let record=store.load(context.session_id().unwrap()).await.unwrap();
            assert_eq!(context.session_incarnation(),record.metadata.incarnation.as_deref());
            assert!(record.journal.iter().any(|event|matches!(event,SessionJournalEvent::MiddlewareStateUpdated{namespace,state}if namespace=="host-dispatch" && state["active"]==true)));
            effects.fetch_add(1,Ordering::SeqCst);
            Ok(context.result("done"))
        })
    }).with_host_dispatch(|_|Ok(binding()));
    let events = sessions
        .prompt(&runtime(tool), id.clone(), "execute")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let record = store.load(&id).await.unwrap();
    let fence=record.journal.iter().rposition(|event|matches!(event,SessionJournalEvent::MiddlewareStateUpdated{namespace,state}if namespace=="host-dispatch" && state["active"]==false)).unwrap();
    let result=record.journal.iter().rposition(|event|matches!(event,SessionJournalEvent::Timeline(event)if matches!(event.content,TimelineContent::ToolResult{..}))).unwrap();
    assert!(result < fence);
}

#[tokio::test]
async fn interrupted_dispatch_refuses_resume_before_provider_or_templates() {
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = sessions
        .create(SessionCreateRequest::new("interrupted-session"))
        .await
        .unwrap()
        .id()
        .clone();
    let record = store.load(&id).await.unwrap();
    store.append_journal(&id,record.execution_token().unwrap(),vec![SessionJournalEvent::MiddlewareStateUpdated{namespace:"host-dispatch".into(),state:json!({"active":true,"callId":"stable-call","invocationId":"reserved-invocation"})}]).await.unwrap();
    let tool = AgentTool::new(ToolDefinition::new("code", "test", json!({})), |_| {
        panic!("no replay effects")
    })
    .with_host_dispatch(|_| panic!("no new association"));
    let events = sessions
        .prompt(&runtime(tool), id, "{{ code({}) }}")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(Result::is_err), "{events:?}");
}

#[tokio::test]
async fn host_dispatch_cannot_be_called_without_the_loop_fence() {
    let tool = AgentTool::new(ToolDefinition::new("code", "test", json!({})), |_| {
        panic!("no ambient fallback")
    })
    .with_host_dispatch(|_| Ok(binding()));
    let context = ToolExecutionContext::new(ToolCall::new("x", "code", json!({})));
    assert!(tool.call(context).await.is_err());
}

#[derive(Clone)]
struct FaultStore {
    inner: MemorySessionStore,
    mode: Arc<AtomicUsize>,
}
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $output:ty;)*) => {$(
        async fn $name(&self,$($arg:$ty),*)->SessionStoreResult<$output> {self.inner.$name($($arg),*).await}
    )*};
}
impl SessionStore for FaultStore {
    async fn append_journal(
        &self,
        id: &SessionId,
        expected_revision: ExecutionToken,
        events: Vec<SessionJournalEvent>,
    ) -> SessionStoreResult<SessionCommitReceipt> {
        let mode = self.mode.load(Ordering::SeqCst);
        let fault=events.iter().any(|event|matches!(event,SessionJournalEvent::MiddlewareStateUpdated{namespace,state}if namespace=="host-dispatch" && state["active"]==json!(mode<=2)));
        if fault && self.mode.swap(0, Ordering::SeqCst) != 0 {
            if mode == 2 || mode == 4 {
                self.inner
                    .append_journal(id, expected_revision, events)
                    .await?;
            }
            return Err(SessionStoreError::Backend {
                message: "injected durable append failure/ACK loss".into(),
            });
        }
        self.inner
            .append_journal(id, expected_revision, events)
            .await
    }
    forward! {
        create(request:SessionCreateRequest)->SessionRecord;
        load(id:&SessionId)->SessionRecord;
        delete(id:&SessionId)->();
        wipe()->();
        list()->Vec<SessionSummary>;
        append_timeline(id:&SessionId,expected_revision:ExecutionToken,events:Vec<PendingTimelineEvent>)->TimelineAppendReceipt;
        mutate(id:&SessionId,mutation:SessionMutation)->SessionMutationOutcome;
        set_metadata_value_conditionally(id:&SessionId,key:String,value:serde_json::Value,condition:MetadataSetCondition)->MetadataSetOutcome;
        remove_metadata_value(id:&SessionId,key:String)->SessionRecord;
        set_inference_settings(id:&SessionId,inference_settings:Option<InferenceSettings>)->SessionRecord;
        set_agent_definition(id:&SessionId,agent_definition:Option<serde_json::Value>)->SessionRecord;
        timeline(id:&SessionId,query:TimelineQuery)->Vec<AgentTimelineEvent>;
        timeline_item_text(id:&SessionId,item_id:&TimelineItemId)->TimelineItemText;
    }
}

#[tokio::test]
async fn ordinary_custom_store_failure_and_lost_ack_never_replay_dispatched_work() {
    for mode in 1..=4 {
        let store = FaultStore {
            inner: MemorySessionStore::new(),
            mode: Arc::new(AtomicUsize::new(mode)),
        };
        let sessions = SessionCoordinator::new(store.clone());
        let id = sessions
            .create(SessionCreateRequest::new(format!("fault-{mode}")))
            .await
            .unwrap()
            .id()
            .clone();
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let tool = AgentTool::new(
            ToolDefinition::new("code", "test", json!({})),
            move |context| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(context.result("done"))
            },
        )
        .with_host_dispatch(|_| Ok(binding()));
        let runtime = runtime(tool);
        let events = sessions
            .prompt(&runtime, id.clone(), "run")
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(Result::is_err), "mode {mode}: {events:?}");
        assert_eq!(effects.load(Ordering::SeqCst), usize::from(mode >= 3));
        if mode >= 2 {
            assert_eq!(
                store.load(&id).await.unwrap().snapshot().middleware_state["host-dispatch"]["active"],
                json!(mode != 4),
                "only an acknowledged or durably committed settlement can clear the fence",
            );
        }
        if mode >= 2 {
            let restarted = SessionCoordinator::new(store.clone());
            let events = restarted
                .prompt(&runtime, id.clone(), "{{ code({}) }}")
                .stream()
                .collect::<Vec<_>>()
                .await;
            if mode != 4 {
                assert!(events.iter().any(Result::is_err), "mode {mode}: {events:?}");
            }
            assert_eq!(effects.load(Ordering::SeqCst), usize::from(mode >= 3));
        }
    }
}

#[tokio::test]
async fn logical_rollback_cannot_hide_a_completed_external_dispatch() {
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = sessions
        .create(SessionCreateRequest::new("rollback-dispatch"))
        .await
        .unwrap()
        .id()
        .clone();
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let tool = AgentTool::new(
        ToolDefinition::new("code", "test", json!({})),
        move |context| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("done"))
        },
    )
    .with_host_dispatch(|_| Ok(binding()));
    let runtime = runtime(tool);
    assert!(
        sessions
            .prompt(&runtime, id.clone(), "run")
            .stream()
            .collect::<Vec<_>>()
            .await
            .iter()
            .all(Result::is_ok)
    );
    let record = store.load(&id).await.unwrap();
    store
        .append_journal(
            &id,
            record.execution_token().unwrap(),
            vec![SessionJournalEvent::Rollback {
                target: SessionRollbackTarget {
                    journal_len: 0,
                    after_cursor: None,
                    next_turn: 0,
                    remove_prompt_index: None,
                },
            }],
        )
        .await
        .unwrap();
    let events = sessions
        .prompt(&runtime, id, "{{ code({}) }}")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(Result::is_err));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}
