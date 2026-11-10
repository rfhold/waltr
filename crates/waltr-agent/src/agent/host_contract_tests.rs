use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_util::StreamExt as _;
use serde_json::json;

use crate::*;

struct HostProvider;

impl LlmStream for HostProvider {
    fn name(&self) -> &'static str {
        "host-contract"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "host-tool");
        assert!(
            request
                .system_prompt
                .as_deref()
                .unwrap_or_default()
                .contains("host context")
        );
        let completed = request
            .turns
            .iter()
            .any(|turn| turn.tool_results().next().is_some());
        Box::pin(async move {
            let event = if completed {
                CompletionEvent::Delta("done".into())
            } else {
                CompletionEvent::ToolCall(ToolCall::new("host-call", "host-tool", json!({})))
            };
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(event),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

struct Context {
    mutate_result: bool,
}

impl AgentHostContext for Context {
    fn prepare_request<'a>(
        &'a self,
        context: HostContext<'a>,
        request: &'a mut HostRequest,
    ) -> HostContextFuture<'a> {
        Box::pin(async move {
            assert_eq!(context.session_id, "host-contract");
            assert!(context.metadata.is_some());
            assert!(!context.cancellation.is_cancelled());
            assert!(
                context
                    .state
                    .set("host-dispatch".into(), json!({}))
                    .is_err()
            );
            context
                .state
                .set("host-context".into(), json!({"prepared": true}))?;
            request.system_prompt = Some("host context".into());
            Ok(())
        })
    }

    fn process_result<'a>(
        &'a self,
        context: HostContext<'a>,
        call: Option<&'a ToolCall>,
        result: &'a mut ToolResult,
    ) -> HostContextFuture<'a> {
        Box::pin(async move {
            assert_eq!(
                context.state.get("host-context"),
                Some(&json!({"prepared": true}))
            );
            assert_eq!(call.unwrap().id, result.call_id);
            if self.mutate_result {
                result.content = crate::ToolResultContent::text("host transformed");
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn supplied_tools_context_and_metadata_use_the_ordinary_lifecycle() {
    for mutate_result in [false, true] {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone());
        let id = SessionId::new("host-contract");
        sessions
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let tool = AgentTool::new(
            ToolDefinition::new("host-tool", "host", json!({})),
            |context| {
                assert!(context.host_dispatch().is_some());
                context
                    .result_metadata()
                    .insert("host-presentation", json!({"label": "safe"}));
                Ok(context.result("original"))
            },
        )
        .with_identity("opaque-host-identity")
        .with_host_dispatch(|_| {
            Ok(HostDispatchBinding {
                invocation_id: "host-invocation".into(),
                source_revision: "revision".into(),
            })
        });
        assert_eq!(tool.identity(), Some("opaque-host-identity"));
        let hidden = AgentTool::new(ToolDefinition::new("hidden", "hidden", json!({})), |_| {
            panic!("hidden tool must not execute")
        })
        .with_model_exposure(false);
        let runtime = AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
            Arc::new(HostProvider),
        )
        .with_tool(tool)
        .with_tool(hidden)
        .with_host_context(Arc::new(Context { mutate_result }))
        .build();
        let events = sessions
            .prompt(&runtime, id.clone(), "run")
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let record = store.load(&id).await.unwrap();
        let result = record
            .timeline_events()
            .find(|event| matches!(event.content, TimelineContent::ToolResult { .. }))
            .unwrap();
        assert_eq!(
            result.metadata.get("host-presentation").is_some(),
            !mutate_result
        );
        let state = record.journal.iter().position(|event| matches!(event,
            SessionJournalEvent::MiddlewareStateUpdated { namespace, .. } if namespace == "host-context")).unwrap();
        let provider = record
            .journal
            .iter()
            .position(|event| matches!(event, SessionJournalEvent::ProviderRequestRecorded { .. }))
            .unwrap();
        assert!(state < provider);
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn uncertain_host_failure_remains_fenced_and_never_replays() {
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let id = SessionId::new("host-contract");
    sessions
        .create(SessionCreateRequest::new(id.clone()))
        .await
        .unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let tool = AgentTool::new(
        ToolDefinition::new("host-tool", "host", json!({})),
        move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Err(LlmError::Protocol {
                provider: "host",
                message: "reply lost".into(),
            })
        },
    )
    .with_host_dispatch(|_| {
        Ok(HostDispatchBinding {
            invocation_id: "uncertain-invocation".into(),
            source_revision: "revision".into(),
        })
    });
    let runtime = AgentRuntime::builder(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        Arc::new(HostProvider),
    )
    .with_tool(tool)
    .with_host_context(Arc::new(Context {
        mutate_result: false,
    }))
    .build();
    let events = sessions
        .prompt(&runtime, id.clone(), "run")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(Result::is_err));
    assert!(super::tool::host_dispatch_unsettled(
        &store.load(&id).await.unwrap()
    ));
    let resumed = sessions
        .resume(&runtime, id, ResumeInput::Continue)
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(resumed.iter().any(Result::is_err));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    sessions.shutdown().await;
}
