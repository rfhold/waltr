//! Server policy gates the original live invocation, not a replayed turn.
use super::{
    sandbox::TestSandbox,
    scripted_provider::{CapturingProvider, ScriptedProvider},
};
use crate::{
    agent_host::{
        Capability, HostRuntimeBuilder,
        dispatcher::CapabilityDispatcher,
        middleware::{
            AgentMiddleware, HookOutcome, chain::MiddlewareChain, context::ToolCallContext,
        },
        tools::filesystem::FilesystemConfig,
    },
    interaction::{Interaction, InteractionBroker, InteractionResponse},
    permission::*,
};
use futures_util::StreamExt;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use waltr_agent::*;

struct Claims(Vec<PermissionClaim>);
impl AgentMiddleware for Claims {
    async fn before_tool_call(
        &self,
        context: &mut ToolCallContext<'_>,
    ) -> waltr_agent::Result<HookOutcome> {
        context.permission_claims.extend(self.0.clone());
        Ok(HookOutcome::Continue)
    }
}

struct Run {
    sessions: SessionCoordinator<MemorySessionStore>,
    broker: InteractionBroker,
    id: SessionId,
    effects: Arc<AtomicUsize>,
    provider: Arc<CapturingProvider<ScriptedProvider>>,
    owner: tokio::task::JoinHandle<Vec<waltr_agent::Result<AgentTimelineEvent>>>,
    cancellation: tokio_util::sync::CancellationToken,
}

fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses)
}
fn input_rule(pointer: &str, effect: PermissionEffect) -> PermissionInputRule {
    PermissionInputRule::new(
        vec![PermissionInputCondition {
            pointer: PermissionJsonPointer::parse(pointer).unwrap(),
            value: PermissionInputValueMatcher::WildcardPrefix("sessions.".into()),
        }],
        effect,
    )
    .unwrap()
}

async fn spawn_run(policy: PermissionPolicy, calls: usize, claims: Vec<PermissionClaim>) -> Run {
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let broker = InteractionBroker::new();
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let capability = Capability::new(
        ToolDefinition::new("effect", "effect", json!({"type":"object"})),
        move |context| {
            assert!(context.session_id().is_some());
            count.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("invoked"))
        },
    );
    let mut chain = MiddlewareChain::new();
    chain.push(Claims(claims));
    let dispatcher = CapabilityDispatcher::new(
        vec![capability],
        Some(id.clone()),
        ServerPermissionPolicy::sessions(broker.clone()),
    )
    .unwrap()
    .with_context(chain, policy);
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "permission-owner",
        move |request| {
            if request.tool_results().next().is_some() {
                return vec![Ok(CompletionEvent::Finished)];
            }
            (0..calls)
                .map(|i| {
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("call-{i}"),
                        "effect",
                        json!({"action":"sessions.read"}),
                    )))
                })
                .collect()
        },
    )));
    let runtime = AgentRuntime::builder(model(), provider.clone())
        .with_tool(dispatcher.agent_tools().remove(0))
        .build();
    let own = sessions.clone();
    let session = id.clone();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let owner_cancel = cancellation.clone();
    let owner = tokio::spawn(async move {
        own.prompt(&runtime, session, "run")
            .with_cancellation_token(owner_cancel)
            .stream()
            .collect::<Vec<_>>()
            .await
    });
    Run {
        sessions,
        broker,
        id,
        effects,
        provider,
        owner,
        cancellation,
    }
}

async fn pending(run: &Run) -> crate::interaction::InteractionSnapshot {
    let mut watch = run.broker.subscribe(run.id.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = watch.next().await.unwrap();
            if !snapshot.pending.is_empty() {
                return snapshot;
            }
        }
    })
    .await
    .unwrap()
}
fn respond(
    run: &Run,
    snapshot: &crate::interaction::InteractionSnapshot,
    response: PermissionResponse,
) {
    let request = &snapshot.pending[0];
    run.broker
        .respond(
            &run.id,
            &snapshot.generation,
            &request.id,
            &request.operation_id,
            InteractionResponse::Permission(response),
        )
        .unwrap();
}
fn read_claim(path: &str) -> PermissionClaim {
    PermissionClaim::new(
        PermissionOperation::Read,
        PermissionResource::FilesystemPath(path.into()),
        "read",
    )
}

#[tokio::test]
async fn nested_input_rules_gate_before_effect_and_validate_exact_tool_bound_scope() {
    for level in [PermissionLevel::Ask, PermissionLevel::Deny] {
        let effect = if level == PermissionLevel::Ask {
            PermissionEffect::ask()
        } else {
            PermissionEffect::deny()
        };
        let run = spawn_run(
            PermissionPolicy::default().with_input_rule(
                PermissionToolSelector::exact("effect"),
                input_rule("/action", effect),
            ),
            1,
            vec![],
        )
        .await;
        if level == PermissionLevel::Ask {
            let snapshot = pending(&run).await;
            assert_eq!(run.effects.load(Ordering::SeqCst), 0);
            assert_eq!(run.provider.request_prompts().len(), 1);
            let request = &snapshot.pending[0];
            let Interaction::Permission { patterns, .. } = &request.request else {
                panic!()
            };
            assert!(patterns.iter().any(|p| matches!(&p.resource,
                PermissionResourcePattern::ToolInput { tool, pointer, value }
                    if tool == "effect" && pointer.as_str() == "/action" && value == &PermissionInputScalar::String("sessions.read".into()))));
            let mut wrong = patterns.clone();
            for p in &mut wrong {
                if let PermissionResourcePattern::ToolInput { tool, .. } = &mut p.resource {
                    *tool = "other".into();
                }
            }
            assert!(
                run.broker
                    .respond(
                        &run.id,
                        &snapshot.generation,
                        &request.id,
                        &request.operation_id,
                        InteractionResponse::Permission(PermissionResponse::AllowAlways {
                            patterns: wrong
                        })
                    )
                    .is_err()
            );
            assert_eq!(
                serde_json::to_value(run.broker.snapshot(&run.id)).unwrap(),
                serde_json::to_value(snapshot.clone()).unwrap()
            );
            respond(
                &run,
                &snapshot,
                PermissionResponse::AllowAlways {
                    patterns: patterns.clone(),
                },
            );
        }
        let expected = usize::from(level == PermissionLevel::Ask);
        assert!(run.owner.await.unwrap().iter().all(Result::is_ok));
        assert_eq!(run.effects.load(Ordering::SeqCst), expected);
        assert!(run.broker.snapshot(&run.id).pending.is_empty());
        run.sessions.shutdown().await;
    }
}

#[tokio::test]
async fn whole_tool_approval_does_not_bypass_contextual_deny_and_distinct_asks_remain_distinct() {
    for contextual in [
        PermissionLevel::Deny,
        PermissionLevel::Ask,
        PermissionLevel::Allow,
    ] {
        let policy = PermissionPolicy::default()
            .with_tool_rule(
                PermissionToolSelector::exact("effect"),
                PermissionEffect::ask(),
            )
            .with_class_rule(
                PermissionToolSelector::exact("effect"),
                PermissionOperation::Read,
                match contextual {
                    PermissionLevel::Deny => PermissionEffect::deny(),
                    PermissionLevel::Ask => PermissionEffect::ask(),
                    PermissionLevel::Allow => PermissionEffect::allow(),
                },
            );
        let run = spawn_run(policy, 1, vec![read_claim("/outside/file")]).await;
        let first = pending(&run).await;
        respond(&run, &first, PermissionResponse::AllowOnce);
        if contextual == PermissionLevel::Ask {
            let second = pending(&run).await;
            assert_ne!(first.pending[0].id, second.pending[0].id);
            assert_eq!(run.effects.load(Ordering::SeqCst), 0);
            respond(&run, &second, PermissionResponse::AllowOnce);
        }
        assert!(run.owner.await.unwrap().iter().all(Result::is_ok));
        assert_eq!(
            run.effects.load(Ordering::SeqCst),
            usize::from(contextual != PermissionLevel::Deny)
        );
        assert_eq!(run.provider.request_prompts().len(), 2);
        run.sessions.shutdown().await;
    }
}

#[tokio::test]
async fn once_and_reusable_grants_preserve_remaining_provider_calls_without_replay() {
    for reusable in [false, true] {
        let policy = PermissionPolicy::default().with_class_rule(
            PermissionToolSelector::Any,
            PermissionOperation::Read,
            PermissionEffect::ask(),
        );
        let run = spawn_run(
            policy,
            2,
            vec![read_claim("/outside/file"), read_claim("/outside/second")],
        )
        .await;
        let first = pending(&run).await;
        assert_eq!(run.effects.load(Ordering::SeqCst), 0);
        let Interaction::Permission { patterns, .. } = &first.pending[0].request else {
            panic!()
        };
        respond(
            &run,
            &first,
            if reusable {
                PermissionResponse::AllowAlways {
                    patterns: patterns.clone(),
                }
            } else {
                PermissionResponse::AllowOnce
            },
        );
        if !reusable {
            let second = pending(&run).await;
            assert_ne!(first.pending[0].id, second.pending[0].id);
            assert_eq!(run.effects.load(Ordering::SeqCst), 1);
            respond(&run, &second, PermissionResponse::AllowOnce);
        }
        let events = run.owner.await.unwrap();
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e.as_ref().unwrap().content,
                    TimelineContent::ToolResult { .. }
                ))
                .count(),
            2
        );
        assert_eq!(run.effects.load(Ordering::SeqCst), 2);
        assert_eq!(run.provider.request_prompts().len(), 2);
        run.sessions.shutdown().await;
    }
}

#[tokio::test]
async fn deny_and_owner_cancellation_never_execute_and_cancel_work_does_not_resume() {
    for mode in [0, 1, 2] {
        let run = spawn_run(PermissionPolicy::new(PermissionLevel::Ask), 1, vec![]).await;
        let snapshot = pending(&run).await;
        match mode {
            0 => respond(
                &run,
                &snapshot,
                PermissionResponse::Deny {
                    message: Some("not permitted".into()),
                    mode: PermissionDenyMode::ReturnToolResult,
                },
            ),
            1 => respond(
                &run,
                &snapshot,
                PermissionResponse::Deny {
                    message: None,
                    mode: PermissionDenyMode::CancelWork,
                },
            ),
            _ => {
                run.cancellation.cancel();
            }
        }
        let events = tokio::time::timeout(std::time::Duration::from_secs(5), run.owner)
            .await
            .expect("permission owner cancellation deadline")
            .unwrap();
        assert_eq!(run.effects.load(Ordering::SeqCst), 0);
        assert!(run.broker.snapshot(&run.id).pending.is_empty());
        if mode == 0 {
            assert!(events.iter().any(|e| matches!(e, Ok(AgentTimelineEvent { content: TimelineContent::ToolResult { result }, .. }) if result.is_error)));
        } else {
            assert_eq!(run.provider.request_prompts().len(), 1);
        }
        run.sessions.shutdown().await;
    }
}

#[tokio::test]
async fn ceilings_and_restart_keep_grants_private() {
    let run = spawn_run(
        PermissionPolicy::default().with_class_rule(
            PermissionToolSelector::Any,
            PermissionOperation::Read,
            PermissionEffect::ask(),
        ),
        1,
        vec![read_claim("/scope")],
    )
    .await;
    let snapshot = pending(&run).await;
    let Interaction::Permission { patterns, .. } = &snapshot.pending[0].request else {
        panic!()
    };
    respond(
        &run,
        &snapshot,
        PermissionResponse::AllowAlways {
            patterns: patterns.clone(),
        },
    );
    assert!(run.owner.await.unwrap().iter().all(Result::is_ok));
    let record = run.sessions.store().load(&run.id).await.unwrap();
    assert!(
        !serde_json::to_string(&record)
            .unwrap()
            .contains("allow_always")
    );
    run.sessions.shutdown().await;
    let restricted = spawn_run(
        PermissionPolicy::new(PermissionLevel::Ask)
            .intersection(PermissionPolicy::new(PermissionLevel::Deny)),
        1,
        vec![],
    )
    .await;
    assert!(restricted.owner.await.unwrap().iter().all(Result::is_ok));
    assert_eq!(restricted.effects.load(Ordering::SeqCst), 0);
    assert!(
        restricted
            .broker
            .snapshot(&restricted.id)
            .pending
            .is_empty()
    );
    restricted.sessions.shutdown().await;
}

#[tokio::test]
async fn denied_worker_capability_does_not_prepare_on_disconnected_worker() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    sandbox.disconnect();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let broker = InteractionBroker::new();
    let provider = Arc::new(ScriptedProvider::from_turns(
        "denied-worker",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "read",
                "read",
                json!({"filePath":"file"}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let runtime = HostRuntimeBuilder::new(model(), provider)
        .with_sandbox(sandbox.handle())
        .with_permission_policy(PermissionPolicy::new(PermissionLevel::Deny))
        .with_filesystem_tools(FilesystemConfig::new(root.path()))
        .with_dispatch_context(
            Some(id.clone()),
            ServerPermissionPolicy::sessions(broker.clone()),
        )
        .build();
    let events = sessions
        .prompt(&runtime, id.clone(), "read")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(events.iter().any(|e| matches!(e, Ok(AgentTimelineEvent {content:TimelineContent::ToolResult {result},..}) if result.is_error && result.content.contains("denied"))));
    assert!(broker.snapshot(&id).pending.is_empty());
    sessions.shutdown().await;
}

#[tokio::test]
async fn reusable_grants_are_bound_to_policy_fingerprint_and_record_incarnation() {
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let broker = InteractionBroker::new();
    let authority = ServerPermissionPolicy::sessions(broker.clone());
    let effects = Arc::new(AtomicUsize::new(0));
    for stage in 0..4 {
        if stage == 3 {
            sessions.delete(&id).await.unwrap();
            sessions
                .create(SessionCreateRequest::new(id.clone()))
                .await
                .unwrap();
        }
        let provider = Arc::new(ScriptedProvider::from_turns(
            "grant-binding",
            vec![
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    format!("effect-{stage}"),
                    "effect",
                    json!({"action":"sessions.read"}),
                ))],
                vec![CompletionEvent::Finished],
            ],
        ));
        let policy = if stage < 2 {
            PermissionPolicy::default().with_input_rule(
                PermissionToolSelector::Any,
                input_rule("/action", PermissionEffect::ask()),
            )
        } else {
            PermissionPolicy::default()
                .with_input_rule(
                    PermissionToolSelector::Any,
                    input_rule("/action", PermissionEffect::ask()),
                )
                .with_tool_rule(
                    PermissionToolSelector::exact("unrelated"),
                    PermissionEffect::deny(),
                )
        };
        let count = effects.clone();
        let runtime = HostRuntimeBuilder::new(model(), provider)
            .with_tool(Capability::new(
                ToolDefinition::new("effect", "effect", json!({"type":"object"})),
                move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(ctx.result("done"))
                },
            ))
            .with_permission_policy(policy)
            .with_dispatch_context(Some(id.clone()), authority.clone())
            .build();
        let owner_sessions = sessions.clone();
        let owner_id = id.clone();
        let owner = tokio::spawn(async move {
            owner_sessions
                .prompt(&runtime, owner_id, "run")
                .stream()
                .collect::<Vec<_>>()
                .await
        });
        if stage != 1 {
            let snapshot = super::pending(&broker, &id).await;
            assert_eq!(effects.load(Ordering::SeqCst), stage);
            let Interaction::Permission { patterns, .. } = &snapshot.pending[0].request else {
                panic!("permission")
            };
            super::respond(
                &broker,
                &snapshot,
                InteractionResponse::Permission(PermissionResponse::AllowAlways {
                    patterns: patterns.clone(),
                }),
            )
            .unwrap();
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), owner)
                .await
                .unwrap()
                .unwrap()
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(effects.load(Ordering::SeqCst), stage + 1);
        assert!(broker.snapshot(&id).pending.is_empty());
    }
    sessions.shutdown().await;
}
