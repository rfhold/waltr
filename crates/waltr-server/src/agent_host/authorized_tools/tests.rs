use super::*;
use crate::agent_host::middleware::{
    AgentMiddleware, HookOutcome,
    context::{ToolCallContext, ToolResultContext},
};
use crate::agent_host::{Capability as AgentTool, builder::HostRuntimeBuilder};
use crate::permission::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use waltr_agent::*;

struct UnusedProvider;

#[test]
fn identical_aliases_and_schemas_cannot_pair_a_runtime_with_another_upstream_source() {
    let target = ConcreteToolTarget::new("remote", "lookup").unwrap();
    let mut first = AgentTool::new(
        ToolDefinition::new("remote__lookup", "lookup", json!({"type":"object"})),
        |context| Ok(context.result("not called")),
    )
    .with_concrete_target(target);
    first.mcp_source_revision = Some("endpoint-and-authority-a".into());
    let mut second = first.clone();
    second.mcp_source_revision = Some("endpoint-and-authority-b".into());
    let dispatcher = |capability| {
        CapabilityDispatcher::new(
            vec![capability],
            None,
            Arc::new(crate::agent_host::dispatcher::UnconfiguredPolicy),
        )
        .unwrap()
    };
    let original = dispatcher(first);
    let changed = dispatcher(second.clone());
    assert_ne!(original.revision(), changed.revision());
    assert!(original.scoped_effect(second).is_err());
    assert_ne!(
        original
            .clone()
            .with_revision_source("resource-only-source-a")
            .revision(),
        original
            .with_revision_source("resource-only-source-b")
            .revision()
    );
}
impl LlmStream for UnusedProvider {
    fn name(&self) -> &'static str {
        "unused"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        panic!("no provider")
    }
}

struct Projection;

struct EvidenceProvider(AtomicUsize);
impl LlmStream for EvidenceProvider {
    fn name(&self) -> &'static str {
        "effective-evidence"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        if !first {
            assert!(request.turns.iter().flat_map(|turn| &turn.items).any(|item| matches!(item, TurnItem::ToolCall(call) if call.arguments == json!({"original":true}))));
        }
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(if first {
                CompletionEvent::ToolCall(ToolCall::new("call", "test", json!({"original":true})))
            } else {
                CompletionEvent::Delta("candidate".into())
            })])) as BoxCompletionStream)
        })
    }
}
struct RewriteOutcome(u8);
impl AgentMiddleware for RewriteOutcome {
    async fn before_tool_call(&self, ctx: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        ctx.call.arguments = json!({"effective":true});
        match self.0 {
            0 => Ok(HookOutcome::ReplaceToolResult(ToolResult::new(
                &ctx.call.id,
                &ctx.call.name,
                "replacement",
            ))),
            1 => Ok(HookOutcome::Continue),
            _ => Err(LlmError::Protocol {
                provider: "test",
                message: "middleware error".into(),
            }),
        }
    }
}
struct EvidenceObserver;
impl CompletionObserver for EvidenceObserver {
    fn observe(
        &self,
        proposal: CompletionProposal,
        _: CancellationToken,
    ) -> CompletionObserverFuture {
        assert_eq!(proposal.evidence.len(), 1);
        assert_eq!(
            proposal.evidence[0].call.arguments,
            json!({"effective":true})
        );
        assert_eq!(proposal.evidence[0].result.call_id, "call");
        Box::pin(async { Ok(CompletionDecision::Accept) })
    }
}

#[tokio::test]
async fn effective_evidence_includes_replacement_contextual_denial_and_hook_error() {
    use futures_util::StreamExt;
    for outcome in 0..3 {
        let policy = if outcome == 1 {
            PermissionPolicy::new(PermissionLevel::Allow).with_input_rule(
                PermissionToolSelector::exact("test"),
                PermissionInputRule::new(
                    vec![PermissionInputCondition {
                        pointer: PermissionJsonPointer::parse("/effective").unwrap(),
                        value: PermissionInputValueMatcher::Exact(PermissionInputScalar::Bool(
                            true,
                        )),
                    }],
                    PermissionEffect::deny(),
                )
                .unwrap(),
            )
        } else {
            PermissionPolicy::new(PermissionLevel::Allow)
        };
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let runtime = HostRuntimeBuilder::new(
            ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
            Arc::new(EvidenceProvider(AtomicUsize::new(0))),
        )
        .with_tool(AgentTool::new(
            ToolDefinition::new("test", "test", json!({"type":"object"})),
            |_| -> Result<ToolResult> { panic!("no concrete effects") },
        ))
        .with_middleware(RewriteOutcome(outcome))
        .with_permission_policy(policy)
        .with_dispatch_context(
            Some(id.clone()),
            crate::permission::ServerPermissionPolicy::sessions(
                crate::interaction::InteractionBroker::new(),
            ),
        )
        .build();
        let events = sessions
            .prompt(&runtime, id, "task")
            .with_completion_validation(
                CompletionValidation::new(
                    Arc::new(EvidenceObserver),
                    2,
                    std::time::Duration::from_secs(2),
                )
                .unwrap(),
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
    }
}
impl AgentMiddleware for Projection {
    async fn before_tool_call(&self, context: &mut ToolCallContext<'_>) -> Result<HookOutcome> {
        context.call.arguments = json!({"effective":true});
        Ok(HookOutcome::Continue)
    }
    async fn after_tool_call(&self, context: &mut ToolResultContext<'_>) -> Result<HookOutcome> {
        assert_eq!(context.call.unwrap().arguments, json!({"effective":true}));
        context.result.content = ToolResultContent::text("projected, not secret");
        Ok(HookOutcome::Continue)
    }
}

async fn session(sessions: &SessionCoordinator<MemorySessionStore>) -> SessionId {
    let mut request = SessionCreateRequest::new(SessionId::generate());
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
    sessions.create(request).await.unwrap().id().clone()
}

#[tokio::test]
async fn concrete_boundary_returns_post_middleware_result() {
    let target = ConcreteToolTarget::new("server", "tool").unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let tool = AgentTool::new(
        ToolDefinition::new("alias", "test", json!({"type":"object"})),
        move |context| {
            assert_eq!(context.call().arguments, json!({"effective":true}));
            count.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("raw secret"))
        },
    )
    .with_concrete_target(target.clone())
    .with_model_exposure(false);
    let provider = Arc::new(UnusedProvider);
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = session(&sessions).await;
    let (runtime, dispatcher) = HostRuntimeBuilder::new(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider,
    )
    .with_tool(tool)
    .with_middleware(Projection)
    .with_dispatch_context(
        Some(id.clone()),
        crate::permission::ServerPermissionPolicy::sessions(
            crate::interaction::InteractionBroker::new(),
        ),
    )
    .build_with_dispatcher();
    let executor =
        SessionAuthorizedToolExecutor::new(sessions.clone(), id.clone(), runtime, dispatcher);
    let result = executor
        .execute(
            AuthorizedToolRequest {
                target: target.clone(),
                call: ToolCall::new("direct", target.permission_name(), json!({"raw":true})),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.content, "projected, not secret");
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let record = sessions.store().load(&id).await.unwrap();
    assert!(record.snapshot().turns.iter().flat_map(|turn| &turn.items).any(|item| matches!(item, TurnItem::ToolResult(result) if result.content == "projected, not secret")));
    executor.cleanup().await.unwrap();
}

#[tokio::test]
async fn app_only_target_is_neither_discoverable_nor_executable() {
    let target = ConcreteToolTarget::new("server", "app-only").unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let mut tool = AgentTool::new(
        ToolDefinition::new("alias", "app only", json!({"type":"object"})),
        move |context| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("not reachable"))
        },
    )
    .with_concrete_target(target.clone())
    .with_model_exposure(false);
    tool.mcp_app_source = Some(crate::agent_host::mcp::apps::McpAppSource {
        server: "server".into(),
        tool: "app-only".into(),
        source: "authority".into(),
        declaration: json!({}),
        ui: ::mcp::apps::UiToolMeta {
            visibility: vec![::mcp::apps::UiVisibility::App],
            ..Default::default()
        },
    });
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = session(&sessions).await;
    let (runtime, dispatcher) = HostRuntimeBuilder::new(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        Arc::new(UnusedProvider),
    )
    .with_tool(tool)
    .with_dispatch_context(
        Some(id.clone()),
        crate::permission::ServerPermissionPolicy::sessions(
            crate::interaction::InteractionBroker::new(),
        ),
    )
    .build_with_dispatcher();
    let executor = SessionAuthorizedToolExecutor::new(sessions, id, runtime, dispatcher);
    assert_eq!(
        executor
            .schemas(vec![target.clone()], CancellationToken::new())
            .await
            .unwrap_err(),
        AuthorizedToolError::Denied
    );
    let result = executor
        .execute(
            AuthorizedToolRequest {
                call: ToolCall::new("direct", target.permission_name(), json!({})),
                target,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err() || result.unwrap().is_error);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    executor.cleanup().await.unwrap();
}
