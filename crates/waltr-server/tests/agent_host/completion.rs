use crate::host_tests::{pending, respond, scripted_provider::ScriptedProvider};
use crate::{
    agent_host::{Capability, HostRuntimeBuilder, authorized_tools::*},
    interaction::{Interaction, InteractionBroker, InteractionResponse},
    permission::*,
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use waltr_agent::*;

fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses)
}
fn unused() -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new("unused", |_| {
        panic!("concrete calls never invoke inference")
    }))
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
struct Observer(Arc<Mutex<Vec<CompletionProposal>>>);
impl CompletionObserver for Observer {
    fn observe(
        &self,
        proposal: CompletionProposal,
        _: CancellationToken,
    ) -> CompletionObserverFuture {
        self.0.lock().unwrap().push(proposal);
        Box::pin(async { Ok(CompletionDecision::Accept) })
    }
}
fn validation(
    proposals: Arc<Mutex<Vec<CompletionProposal>>>,
    turns: u64,
    duration: Duration,
) -> CompletionValidation {
    CompletionValidation::new(Arc::new(Observer(proposals)), turns, duration).unwrap()
}

#[tokio::test]
async fn persisted_validation_fence_precedes_template_rendering_after_restart() {
    for namespace in ["completion-validation", "concrete-execution"] {
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let record = sessions.store().load(&id).await.unwrap();
        sessions
            .store()
            .append_journal(
                &id,
                record.execution_token().unwrap(),
                vec![SessionJournalEvent::MiddlewareStateUpdated {
                    namespace: namespace.into(),
                    state: json!({"active":true}),
                }],
            )
            .await
            .unwrap();
        let runtime = HostRuntimeBuilder::new(model(), unused())
            .with_tool(Capability::new(
                ToolDefinition::new("effect", "effect", json!({"type":"object"})),
                move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(ctx.result("must not execute"))
                },
            ))
            .with_dispatch_context(
                Some(id.clone()),
                ServerPermissionPolicy::sessions(InteractionBroker::new()),
            )
            .build();
        let events = sessions
            .resume(
                &runtime,
                id,
                ResumeInput::Prompt("@tool(effect, {})".into()),
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(Result::is_err));
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn live_denial_result_has_exact_completion_evidence_and_byte_charge() {
    for large in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let provider = Arc::new(ScriptedProvider::new("evidence", move |request| {
            let results = request.tool_results().cloned().collect::<Vec<_>>();
            requests.lock().unwrap().extend(results.clone());
            if results.is_empty() {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "denied",
                    "effect",
                    json!({"value":"original"}),
                )))]
            } else {
                vec![
                    Ok(CompletionEvent::Delta("done".into())),
                    Ok(CompletionEvent::Finished),
                ]
            }
        }));
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let broker = InteractionBroker::new();
        let runtime = HostRuntimeBuilder::new(model(), provider)
            .with_tool(Capability::new(
                ToolDefinition::new("effect", "effect", json!({"type":"object"})),
                move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(ctx.result("effect"))
                },
            ))
            .with_permission_policy(PermissionPolicy::new(PermissionLevel::Ask))
            .with_dispatch_context(
                Some(id.clone()),
                ServerPermissionPolicy::sessions(broker.clone()),
            )
            .build();
        let proposals = Arc::new(Mutex::new(Vec::new()));
        let run = sessions
            .start_pinned_helper(
                id.clone(),
                runtime,
                Some(
                    validation(proposals.clone(), 3, Duration::from_secs(5))
                        .with_encoded_byte_limit(if large { 1024 } else { 16384 })
                        .unwrap(),
                ),
                AgentRunLimits::default(),
            )
            .await
            .unwrap();
        let snapshot = pending(&broker, &id).await;
        respond(
            &broker,
            &snapshot,
            InteractionResponse::Permission(PermissionResponse::Deny {
                message: Some(if large {
                    "x".repeat(4096)
                } else {
                    "not approved".into()
                }),
                mode: PermissionDenyMode::ReturnToolResult,
            }),
        )
        .unwrap();
        assert!(
            respond(
                &broker,
                &snapshot,
                InteractionResponse::Permission(PermissionResponse::AllowOnce)
            )
            .is_err()
        );
        let outcome = run.handle.wait().await;
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        if large {
            assert!(
                outcome.is_err()
                    || matches!(
                        outcome.as_ref().unwrap().status,
                        SessionStatus::Failed { .. }
                    )
            );
            assert!(seen.lock().unwrap().is_empty());
            assert!(proposals.lock().unwrap().is_empty());
        } else {
            assert_eq!(outcome.as_ref().unwrap().status, SessionStatus::Finished);
            let proposals = proposals.lock().unwrap();
            assert_eq!(proposals.len(), 1);
            let evidence = &proposals[0].evidence;
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].result, seen.lock().unwrap()[0]);
            assert!(evidence[0].result.is_error);
            assert_eq!(evidence[0].call.arguments, json!({"value":"original"}));
            let record = sessions.store().load(&id).await.unwrap();
            let event = record.timeline_events().find(|e| matches!(&e.content,TimelineContent::ToolResult{result} if result.call_id=="denied")).unwrap();
            assert_eq!(event.parent_id.as_ref(), Some(&evidence[0].item_id));
            assert_eq!(event.turn, evidence[0].turn);
        }
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn live_helper_consent_keeps_original_owner_limits_and_private_grants() {
    for mode in [
        "once",
        "always",
        "deny",
        "cancel-work",
        "cancel",
        "deadline",
    ] {
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let provider = Arc::new(ScriptedProvider::from_turns(
            "helper",
            vec![
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "one",
                    "effect",
                    json!({"scope":"same"}),
                ))],
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "two",
                    "effect",
                    json!({"scope":"same"}),
                ))],
                vec![
                    CompletionEvent::Delta("done".into()),
                    CompletionEvent::Finished,
                ],
            ],
        ));
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let broker = InteractionBroker::new();
        let runtime = HostRuntimeBuilder::new(model(), provider)
            .with_tool(Capability::new(
                ToolDefinition::new("effect", "effect", json!({"type":"object"})),
                move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(ctx.result("once"))
                },
            ))
            .with_permission_policy(
                PermissionPolicy::default().with_input_rule(
                    PermissionToolSelector::exact("effect"),
                    PermissionInputRule::new(
                        vec![PermissionInputCondition {
                            pointer: PermissionJsonPointer::parse("/scope").unwrap(),
                            value: PermissionInputValueMatcher::Exact(
                                PermissionInputScalar::String("same".into()),
                            ),
                        }],
                        PermissionEffect::ask(),
                    )
                    .unwrap(),
                ),
            )
            .with_dispatch_context(
                Some(id.clone()),
                ServerPermissionPolicy::sessions(broker.clone()),
            )
            .build();
        let proposals = Arc::new(Mutex::new(Vec::new()));
        let duration = if mode == "deadline" {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(5)
        };
        let limit = if mode == "always" { 2 } else { 4 };
        let submission = sessions
            .start_pinned_helper(
                id.clone(),
                runtime.clone(),
                Some(validation(proposals.clone(), limit, duration)),
                AgentRunLimits::default(),
            )
            .await
            .unwrap();
        let first = pending(&broker, &id).await;
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert!(InteractionBroker::new().snapshot(&id).pending.is_empty());
        match mode {
            "once" => {
                respond(
                    &broker,
                    &first,
                    InteractionResponse::Permission(PermissionResponse::AllowOnce),
                )
                .unwrap();
                let second = pending(&broker, &id).await;
                assert_ne!(first.pending[0].id, second.pending[0].id);
                assert_eq!(effects.load(Ordering::SeqCst), 1);
                respond(
                    &broker,
                    &second,
                    InteractionResponse::Permission(PermissionResponse::AllowOnce),
                )
                .unwrap();
            }
            "always" => {
                let Interaction::Permission { patterns, .. } = &first.pending[0].request else {
                    panic!("permission")
                };
                respond(
                    &broker,
                    &first,
                    InteractionResponse::Permission(PermissionResponse::AllowAlways {
                        patterns: patterns.clone(),
                    }),
                )
                .unwrap();
            }
            "deny" | "cancel-work" => {
                respond(
                    &broker,
                    &first,
                    InteractionResponse::Permission(PermissionResponse::Deny {
                        message: Some("no".into()),
                        mode: if mode == "deny" {
                            PermissionDenyMode::ReturnToolResult
                        } else {
                            PermissionDenyMode::CancelWork
                        },
                    }),
                )
                .unwrap();
                if mode == "deny" {
                    let second = pending(&broker, &id).await;
                    respond(
                        &broker,
                        &second,
                        InteractionResponse::Permission(PermissionResponse::Deny {
                            message: None,
                            mode: PermissionDenyMode::ReturnToolResult,
                        }),
                    )
                    .unwrap();
                }
            }
            "cancel" => {
                sessions.interrupt(&id).await.unwrap();
            }
            "deadline" => {}
            _ => unreachable!(),
        }
        let outcome = tokio::time::timeout(Duration::from_secs(3), submission.handle.wait())
            .await
            .unwrap();
        if mode == "once" {
            assert_eq!(outcome.as_ref().unwrap().status, SessionStatus::Finished);
            assert_eq!(proposals.lock().unwrap()[0].evidence.len(), 2);
        }
        if mode == "always" {
            assert!(matches!(
                outcome.as_ref().unwrap().status,
                SessionStatus::Failed { .. }
            ));
        }
        assert_eq!(
            effects.load(Ordering::SeqCst),
            if matches!(mode, "once" | "always") {
                2
            } else {
                0
            }
        );
        assert!(broker.snapshot(&id).pending.is_empty());
        assert!(
            respond(
                &broker,
                &first,
                InteractionResponse::Permission(PermissionResponse::AllowOnce)
            )
            .is_err()
        );
        assert!(
            !serde_json::to_string(&sessions.store().load(&id).await.unwrap())
                .unwrap()
                .contains("allow_always")
        );
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn exact_targets_hidden_exposure_and_live_ask_deny_have_one_dispatcher() {
    let first = ConcreteToolTarget::new("a-b", "x.y").unwrap();
    let second = ConcreteToolTarget::new("a_b", "x_y").unwrap();
    assert_ne!(first.permission_name(), second.permission_name());
    for level in [
        PermissionLevel::Ask,
        PermissionLevel::Deny,
        PermissionLevel::Allow,
    ] {
        let effects = Arc::new(AtomicUsize::new(0));
        let count = effects.clone();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = session(&sessions).await;
        let broker = InteractionBroker::new();
        let (runtime, dispatcher) = HostRuntimeBuilder::new(model(), unused())
            .with_tool(
                Capability::new(
                    ToolDefinition::new("colliding_alias", "test", json!({"type":"object"})),
                    move |ctx| {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(ctx.result("post-policy"))
                    },
                )
                .with_concrete_target(first.clone())
                .with_model_exposure(false),
            )
            .with_permission_policy(PermissionPolicy::new(level))
            .with_dispatch_context(
                Some(id.clone()),
                ServerPermissionPolicy::sessions(broker.clone()),
            )
            .build_with_dispatcher();
        let executor =
            SessionAuthorizedToolExecutor::new(sessions.clone(), id.clone(), runtime, dispatcher);
        let future = executor.execute(
            AuthorizedToolRequest {
                target: first.clone(),
                call: ToolCall::new("direct", first.permission_name(), json!({})),
            },
            CancellationToken::new(),
        );
        let task = tokio::spawn(future);
        if level == PermissionLevel::Ask {
            let snapshot = pending(&broker, &id).await;
            assert_eq!(effects.load(Ordering::SeqCst), 0);
            assert!(!task.is_finished());
            respond(
                &broker,
                &snapshot,
                InteractionResponse::Permission(PermissionResponse::AllowOnce),
            )
            .unwrap();
        }
        let result = task.await.unwrap().unwrap();
        assert_eq!(result.is_error, level == PermissionLevel::Deny);
        assert_eq!(
            effects.load(Ordering::SeqCst),
            usize::from(level != PermissionLevel::Deny)
        );
        executor.cleanup().await.unwrap();
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn discovery_does_not_authorize_alias_input_rules() {
    let target = ConcreteToolTarget::new("configured", "remote").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = session(&sessions).await;
    let policy = PermissionPolicy::default().with_input_rule(
        PermissionToolSelector::exact("legacy_alias"),
        PermissionInputRule::new(
            vec![PermissionInputCondition {
                pointer: PermissionJsonPointer::parse("/action").unwrap(),
                value: PermissionInputValueMatcher::WildcardPrefix("blocked".into()),
            }],
            PermissionEffect::deny(),
        )
        .unwrap(),
    );
    let (runtime, dispatcher) = HostRuntimeBuilder::new(model(), unused())
        .with_tool(
            Capability::new(
                ToolDefinition::new("legacy_alias", "test", json!({"type":"object"})),
                move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(ctx.result("effect"))
                },
            )
            .with_concrete_target(target.clone())
            .with_model_exposure(false),
        )
        .with_permission_policy(policy)
        .with_dispatch_context(
            Some(id.clone()),
            ServerPermissionPolicy::sessions(InteractionBroker::new()),
        )
        .build_with_dispatcher();
    let executor = SessionAuthorizedToolExecutor::new(sessions.clone(), id, runtime, dispatcher);
    assert_eq!(
        executor
            .schemas(vec![target.clone()], CancellationToken::new())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let result = executor
        .execute(
            AuthorizedToolRequest {
                target: target.clone(),
                call: ToolCall::new(
                    "denied-input",
                    target.permission_name(),
                    json!({"action":"blocked-effect"}),
                ),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    executor.cleanup().await.unwrap();
    sessions.shutdown().await;
}

#[tokio::test]
async fn dropped_execution_future_is_cancelled_and_joined_by_cleanup() {
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let target = ConcreteToolTarget::new("server", "pending").unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicUsize::new(0));
    let notify = started.clone();
    let count = dropped.clone();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = session(&sessions).await;
    let (runtime, dispatcher) = HostRuntimeBuilder::new(model(), unused())
        .with_tool(
            Capability::new_async(
                ToolDefinition::new("pending", "test", json!({"type":"object"})),
                move |_| {
                    let notify = notify.clone();
                    let count = count.clone();
                    Box::pin(async move {
                        let _guard = Dropped(count);
                        notify.notify_one();
                        std::future::pending().await
                    })
                },
            )
            .with_concrete_target(target.clone()),
        )
        .with_dispatch_context(
            Some(id.clone()),
            ServerPermissionPolicy::sessions(InteractionBroker::new()),
        )
        .build_with_dispatcher();
    let executor = SessionAuthorizedToolExecutor::new(sessions.clone(), id, runtime, dispatcher);
    let task = tokio::spawn(executor.execute(
        AuthorizedToolRequest {
            target: target.clone(),
            call: ToolCall::new("pending", target.permission_name(), json!({})),
        },
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        executor
            .execute(
                AuthorizedToolRequest {
                    target: target.clone(),
                    call: ToolCall::new("concurrent", target.permission_name(), json!({}))
                },
                CancellationToken::new()
            )
            .await
            .unwrap_err(),
        AuthorizedToolError::Unavailable
    );
    tokio::time::timeout(Duration::from_secs(2), executor.cleanup())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    sessions.shutdown().await;
}
