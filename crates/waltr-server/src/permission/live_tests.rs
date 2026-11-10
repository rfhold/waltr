use super::*;
use crate::agent_host::{
    Capability,
    dispatcher::CapabilityDispatcher,
    middleware::{AgentMiddleware, HookOutcome, chain::MiddlewareChain, context::ToolCallContext},
};
use futures_util::{StreamExt, stream};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use waltr_agent::{
    AgentRuntime, BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
    LlmStream, MemorySessionStore, ModelApi, ModelRef, ProviderId, SessionCoordinator,
    SessionCreateRequest, SessionId, ToolCall, ToolDefinition,
};

struct Provider {
    requests: AtomicUsize,
    tools: usize,
}
impl LlmStream for Provider {
    fn name(&self) -> &'static str {
        "permission-live-test"
    }
    fn stream_request<'a>(&'a self, _: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let first = self.requests.fetch_add(1, Ordering::SeqCst) == 0;
        let mut events = Vec::new();
        if first {
            for i in 0..self.tools {
                events.push(Ok(CompletionEvent::ToolCall(ToolCall::new(
                    format!("call-{i}"),
                    "effect",
                    json!({}),
                ))));
            }
        }
        events.push(Ok(CompletionEvent::Finished));
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
    }
}

struct Claims;
impl AgentMiddleware for Claims {
    async fn before_tool_call(
        &self,
        ctx: &mut ToolCallContext<'_>,
    ) -> waltr_agent::Result<HookOutcome> {
        ctx.permission_claims.push(PermissionClaim::new(
            super::super::PermissionOperation::Read,
            PermissionResource::FilesystemPath("/workspace/data/file".into()),
            "read data",
        ));
        Ok(HookOutcome::Continue)
    }
}

async fn run(
    policy: PermissionPolicy,
    tools: usize,
    claims: bool,
) -> (
    InteractionBroker,
    SessionId,
    Arc<AtomicUsize>,
    Arc<Provider>,
    tokio::task::JoinHandle<Vec<waltr_agent::Result<waltr_agent::AgentTimelineEvent>>>,
) {
    let broker = InteractionBroker::new();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let session = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let effects = Arc::new(AtomicUsize::new(0));
    let counter = effects.clone();
    let capability = Capability::new(
        ToolDefinition::new("effect", "effect", json!({})),
        move |context| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("done"))
        },
    );
    let mut chain = MiddlewareChain::new();
    if claims {
        chain.push(Claims);
    }
    let dispatcher = CapabilityDispatcher::new(
        vec![capability],
        Some(session.clone()),
        ServerPermissionPolicy::sessions(broker.clone()),
    )
    .unwrap()
    .with_context(chain, policy);
    let provider = Arc::new(Provider {
        requests: AtomicUsize::new(0),
        tools,
    });
    let runtime = AgentRuntime::builder(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider.clone(),
    )
    .with_tool(dispatcher.agent_tools().remove(0))
    .build();
    let id = session.clone();
    let owner = tokio::spawn(async move {
        sessions
            .prompt(&runtime, id, "run")
            .stream()
            .collect::<Vec<_>>()
            .await
    });
    (broker, session, effects, provider, owner)
}

#[tokio::test]
async fn allow_and_deny_are_known_results_without_interactions() {
    for (level, count) in [(PermissionLevel::Allow, 1), (PermissionLevel::Deny, 0)] {
        let (broker, session, effects, _, owner) =
            run(PermissionPolicy::new(level), 1, false).await;
        assert!(owner.await.unwrap().iter().all(Result::is_ok));
        assert_eq!(effects.load(Ordering::SeqCst), count);
        assert!(broker.snapshot(&session).pending.is_empty());
    }
}

#[tokio::test]
async fn ask_continues_original_model_operation_without_replaying_turn() {
    let (broker, session, effects, provider, owner) =
        run(PermissionPolicy::new(PermissionLevel::Ask), 1, false).await;
    let mut watch = broker.subscribe(session.clone());
    let snapshot = loop {
        let snapshot = watch.next().await.unwrap();
        if !snapshot.pending.is_empty() {
            break snapshot;
        }
    };
    assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let pending = &snapshot.pending[0];
    broker
        .respond(
            &session,
            &snapshot.generation,
            &pending.id,
            &pending.operation_id,
            InteractionResponse::Permission(PermissionResponse::AllowOnce),
        )
        .unwrap();
    assert!(owner.await.unwrap().iter().all(Result::is_ok));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn reusable_scope_applies_to_later_matching_claims_privately() {
    let policy = PermissionPolicy::default().with_class_rule(
        super::super::PermissionToolSelector::Any,
        super::super::PermissionOperation::Read,
        super::super::PermissionEffect::ask(),
    );
    let (broker, session, effects, _, owner) = run(policy, 2, true).await;
    let mut watch = broker.subscribe(session.clone());
    let snapshot = loop {
        let snapshot = watch.next().await.unwrap();
        if !snapshot.pending.is_empty() {
            break snapshot;
        }
    };
    let pending = &snapshot.pending[0];
    let Interaction::Permission { patterns, .. } = &pending.request else {
        panic!()
    };
    broker
        .respond(
            &session,
            &snapshot.generation,
            &pending.id,
            &pending.operation_id,
            InteractionResponse::Permission(PermissionResponse::AllowAlways {
                patterns: patterns.clone(),
            }),
        )
        .unwrap();
    assert!(owner.await.unwrap().iter().all(Result::is_ok));
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    assert!(broker.snapshot(&session).pending.is_empty());
}

#[tokio::test]
async fn intersection_ceiling_denies_before_any_ask_or_effect() {
    let policy = PermissionPolicy::new(PermissionLevel::Ask)
        .intersection(PermissionPolicy::new(PermissionLevel::Deny));
    let (broker, session, effects, _, owner) = run(policy, 1, false).await;
    assert!(owner.await.unwrap().iter().all(Result::is_ok));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(broker.snapshot(&session).pending.is_empty());
}
