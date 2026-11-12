use super::*;
use crate::permission::{PermissionEffect, PermissionPolicy, PermissionToolSelector};
use tokio_util::sync::CancellationToken;

struct GatedProvider {
    inner: Arc<ScriptedProvider>,
    turns: Arc<AtomicUsize>,
    effect_started: Option<Arc<tokio::sync::Notify>>,
}
impl LlmStream for GatedProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            // Prove subsequent helper progress occurs during the blocked consumer effect.
            if self.turns.load(Ordering::SeqCst) == 1 {
                if let Some(started) = &self.effect_started {
                    started.notified().await;
                }
            }
            self.inner.stream_request(request).await
        })
    }
}
async fn fixture(
    permission: &str,
    collision: bool,
    effect_started: Option<Arc<tokio::sync::Notify>>,
) -> (
    CoordinatorHelperRunner<MemorySessionStore>,
    SessionCoordinator<MemorySessionStore>,
    crate::sandbox::LocalSandbox,
    Arc<AtomicUsize>,
    InteractionBroker,
) {
    fixture_with_factory_restriction(permission, collision, effect_started, None).await
}
async fn fixture_with_factory_restriction(
    permission: &str,
    collision: bool,
    effect_started: Option<Arc<tokio::sync::Notify>>,
    factory_restriction: Option<PermissionPolicy>,
) -> (
    CoordinatorHelperRunner<MemorySessionStore>,
    SessionCoordinator<MemorySessionStore>,
    crate::sandbox::LocalSandbox,
    Arc<AtomicUsize>,
    InteractionBroker,
) {
    let turns = Arc::new(AtomicUsize::new(0));
    let seen = turns.clone();
    let expect_queued = permission == "allow" && factory_restriction.is_none();
    let provider = Arc::new(ScriptedProvider::new("stream-helper", move |request| {
        let turn = seen.fetch_add(1, Ordering::SeqCst);
        let events = match turn {
            0 => vec![CompletionEvent::ToolCall(ToolCall::new(
                "first",
                "speak",
                json!({"text":"first"}),
            ))],
            1 => {
                if expect_queued {
                    assert!(format!("{request:?}").contains("queued"));
                }
                vec![CompletionEvent::ToolCall(ToolCall::new(
                    "second",
                    "speak",
                    json!({"text":"second"}),
                ))]
            }
            2 => vec![CompletionEvent::ToolCall(ToolCall::new(
                "capture",
                "_helper_capture",
                json!({"verified":true}),
            ))],
            3 => vec![CompletionEvent::Delta("done".into())],
            _ => panic!("unexpected helper turn"),
        };
        events
            .into_iter()
            .chain([CompletionEvent::Finished])
            .map(Ok)
            .collect()
    }));
    let provider = Arc::new(GatedProvider {
        inner: provider,
        turns: turns.clone(),
        effect_started,
    });
    let root = crate::test_support::temp_root();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let tool_permission = if permission == "target-deny" {
        "allow"
    } else {
        permission
    };
    let target_rule = if permission == "target-deny" {
        ", {name: 'builtin:speak', permission: deny}"
    } else {
        ""
    };
    let source = format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: writer }}\nspec:\n  systemPrompt: ORIGINAL\n  permissions: [{{name: speak, permission: {tool_permission}}}, {{name: _helper_capture, permission: allow}}{target_rule}]\n"
    );
    let catalog = AgentDefinitionCatalog::from_sources(vec![source], "writer").unwrap();
    let named = catalog.clone();
    let named_provider = provider.clone();
    let broker = InteractionBroker::new();
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("unused.json")),
        catalog.resolved("writer").unwrap().clone(),
    )
    .with_agent_catalog_provider(move || named.clone())
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), named_provider.clone())))
    .with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()));
    let factory = Arc::new(match factory_restriction {
        Some(restriction) => factory.with_permission_restriction(restriction),
        None => factory,
    });
    let policy = PermissionPolicy::default()
        .with_tool_rule(
            PermissionToolSelector::exact("speak"),
            if permission == "deny" {
                PermissionEffect::deny()
            } else if permission == "ask" {
                PermissionEffect::ask()
            } else {
                PermissionEffect::allow()
            },
        )
        .with_tool_rule(
            PermissionToolSelector::exact("_helper_capture"),
            PermissionEffect::allow(),
        );
    let policy = if permission == "target-deny" {
        policy.with_tool_rule(
            PermissionToolSelector::exact("builtin:speak"),
            PermissionEffect::deny(),
        )
    } else {
        policy
    };
    let mut runtime = HostRuntimeBuilder::new(model(), provider).with_permission_policy(policy);
    if collision {
        runtime = runtime.with_tool(Capability::new(
            ToolDefinition::new("speak", "existing", json!({"type":"object"})),
            |context| Ok(context.result("must not run")),
        ));
    }
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let helper = CoordinatorHelperRunner::new(
        sessions.clone(),
        factory,
        inference(),
        BTreeMap::from([(
            "writer".into(),
            RestrictedHelperProfile {
                runtime,
                inference: inference(),
                prompt: "work".into(),
                max_turns: 4,
            },
        )]),
        BTreeMap::new(),
        None,
        "owner".into(),
    )
    .unwrap();
    (helper, sessions, sandbox, turns, broker)
}
fn stream_declarations(named: bool) -> BTreeMap<String, crate::routine::definition::ToolStream> {
    serde_json::from_value(json!({"segments":{
        "selection":if named {json!({"kind":"named","name":"writer"})} else {json!({"kind":"restricted","profile":"writer"})},
        "tool":"speak","description":"Queue one text segment, not a delivery receipt",
        "contract":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"text":{"type":"string","maxLength":64}},"required":["text"]}},
        "maxCalls":1,"maxBytes":1024
    }})).unwrap()
}

#[tokio::test]
async fn tool_streams_real_js_progress_capture_coexistence_and_normal_history() {
    for (named, scenario) in [
        (false, "success"),
        (true, "success"),
        (false, "consumer-failure"),
        (false, "undrained"),
        (false, "cancel-receive"),
        (false, "cancel-send"),
        (false, "cancel-effect"),
    ] {
        let progress_gate = (scenario == "success").then(|| Arc::new(tokio::sync::Notify::new()));
        let (helpers, sessions, sandbox, turns, _) =
            fixture("allow", false, progress_gate.clone()).await;
        let effect_started = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let target = ConcreteToolTarget::new("effects", "blocked").unwrap();
        let release = ConcreteToolTarget::new("effects", "release").unwrap();
        struct Tools {
            target: ConcreteToolTarget,
            started: Arc<tokio::sync::Notify>,
            gate: Arc<tokio::sync::Semaphore>,
            turns: Arc<AtomicUsize>,
            progress_gate: Option<Arc<tokio::sync::Notify>>,
        }
        impl AuthorizedToolExecutor for Tools {
            fn execute(
                &self,
                request: AuthorizedToolRequest,
                cancel: CancellationToken,
            ) -> AuthorizedToolFuture<ToolResult> {
                let blocked = request.target == self.target;
                let started = self.started.clone();
                let gate = self.gate.clone();
                let turns = self.turns.clone();
                let progress_gate = self.progress_gate.clone();
                Box::pin(async move {
                    if blocked {
                        started.notify_one();
                        if let Some(gate) = progress_gate {
                            gate.notify_one();
                        }
                        tokio::select! { () = cancel.cancelled() => return Err(AuthorizedToolError::Unavailable), permit = gate.acquire() => drop(permit.unwrap()) }
                    } else {
                        started.notified().await;
                        assert_eq!(
                            turns.load(Ordering::SeqCst),
                            4,
                            "helper must finish while effect remains blocked"
                        );
                        gate.add_permits(1);
                    }
                    Ok(ToolResult::new(
                        request.call.id,
                        request.call.name,
                        "effect complete",
                    ))
                })
            }
            fn schemas(
                &self,
                _: Vec<ConcreteToolTarget>,
                _: CancellationToken,
            ) -> AuthorizedToolFuture<Vec<ToolDefinition>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn cleanup(&self) -> AuthorizedToolFuture<()> {
                Box::pin(async { Ok(()) })
            }
        }
        let caps = BTreeSet::from_iter(
            [
                "helper.run",
                "helper.streams",
                if named {
                    "helper.named.writer"
                } else {
                    "helper.restricted.writer"
                },
                "helper.capture.verified",
                "tools.execute",
                "tool.block",
                "tool.release",
            ]
            .map(str::to_owned),
        );
        let engine = Arc::new(Engine::new(1).unwrap());
        let selection = if named {
            json!({"kind":"named","name":"writer"})
        } else {
            json!({"kind":"restricted","profile":"writer"})
        };
        let mut source = format!(
            r#"export default async function() {{
            const helper = host.call('helper.run', {{selection:{selection},task:'work',capture:'verified',maxTurns:4,deadlineMs:5000,responseBytes:65536,completionBytes:65536}});
            const first = await host.call('helper.next', {{stream:'segments'}});
            if (first.value.arguments.text !== 'first') throw new Error('first');
            const effect = host.call('tools.execute', {{target:{{server:'effects',tool:'blocked'}},callId:'blocked',arguments:{{}}}});
            const second = await host.call('helper.next', {{stream:'segments'}});
            if (second.value.arguments.text !== 'second') throw new Error('second');
            const result = await helper;
            if (!result.result.captures[0].value.verified) throw new Error('capture lost');
            await host.call('tools.execute', {{target:{{server:'effects',tool:'release'}},callId:'release',arguments:{{}}}});
            await effect;
            const end = await host.call('helper.next', {{stream:'segments'}});
            if (end.value !== null) throw new Error('missing EOF');
            return 'done';
        }}"#
        );
        if scenario == "consumer-failure" {
            source = source.replace(
                "const effect = host.call",
                "throw new Error('consumer failed'); const effect = host.call",
            );
        } else if scenario == "cancel-effect" {
            source = source.replace("await host.call('tools.execute', {target:{server:'effects',tool:'release'},callId:'release',arguments:{}});", "await effect;");
        } else if scenario == "undrained" || scenario == "cancel-send" {
            source = format!(
                "export default async function() {{await host.call('helper.run', {{selection:{selection},task:'work',capture:'verified',maxTurns:4,deadlineMs:5000,responseBytes:65536,completionBytes:65536}}); return 'done';}}"
            );
        } else if scenario == "cancel-receive" {
            source = "export default async function() {await host.call('helper.next',{stream:'segments'}); return 'done';}".into();
        }
        let mut envelope: Value = serde_json::from_str(
            definition(
                &engine,
                "export default async function() {return 'ok';}",
                &BTreeSet::new(),
            )
            .canonical(),
        )
        .unwrap();
        envelope["spec"]["source"] = json!(source);
        envelope["spec"]["capabilities"] = json!(caps);
        envelope["spec"]["toolStreams"] = json!(stream_declarations(named));
        if scenario == "undrained" {
            envelope["spec"]["toolStreams"]["segments"]["maxCalls"] = json!(4);
        }
        envelope["spec"]["captures"] = json!({"verified":{"selection":selection,"contract":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"verified":{"type":"boolean"}},"required":["verified"]}}}});
        let definition = Definition::parse(&envelope.to_string(), engine.as_ref()).unwrap();
        let factory = RoutineAgentHostFactory {
            bindings: Arc::new(BindOnce(Mutex::new(Some(AdapterBindings {
                principal: "owner".into(),
                helpers: Arc::new(helpers),
                tools: Arc::new(Tools {
                    target: target.clone(),
                    started: effect_started.clone(),
                    gate,
                    turns: turns.clone(),
                    progress_gate,
                }),
                capabilities: caps,
                targets: BTreeMap::from([
                    ("tool.block".into(), target),
                    ("tool.release".into(), release),
                ]),
            })))),
        };
        let database = crate::test_support::temp_root().join("stream.sqlite3");
        let persistence = Persistence::initialize(
            DatabaseConfig::from_urls(
                &format!("sqlite:{}?mode=rwc", database.display()),
                "sqlite::memory:",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let service = InvocationService::new(
            engine.clone(),
            persistence.script_invocation_store(),
            Arc::new(factory),
            Arc::new(SystemClock),
            1,
        )
        .unwrap();
        let handle = service
            .invoke(definition, "run".into(), caller(), Value::Null)
            .unwrap()
            .await
            .unwrap();
        if scenario.starts_with("cancel-") {
            tokio::time::timeout(Duration::from_secs(3), async {
                if scenario == "cancel-effect" {
                    effect_started.notified().await;
                } else if scenario == "cancel-send" {
                    while turns.load(Ordering::SeqCst) < 2 {
                        tokio::task::yield_now().await;
                    }
                } else {
                    while persistence
                        .script_invocation_store()
                        .operations(handle.id.clone(), 0, 100)
                        .await
                        .unwrap()
                        .is_empty()
                    {
                        tokio::task::yield_now().await;
                    }
                }
            })
            .await
            .unwrap();
            handle.cancel();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(8), handle.join())
            .await
            .unwrap();
        if scenario == "success" {
            assert_eq!(outcome.status, Some(Status::Succeeded), "{outcome:?}");
        } else if scenario == "undrained" {
            assert_eq!(outcome.status, Some(Status::Failed), "{outcome:?}");
        } else {
            assert_ne!(
                outcome.status,
                Some(Status::Succeeded),
                "{scenario}: {outcome:?}"
            );
        }
        assert!(outcome.cleanup_complete, "{scenario}: {outcome:?}");
        let records = sessions.list().await.unwrap();
        if scenario == "success" {
            assert_eq!(records.len(), 1);
            let record = sessions.store().load(&records[0].id).await.unwrap();
            let history = serde_json::to_string(&record).unwrap();
            assert!(
                history.contains("queued")
                    && history.contains("speak")
                    && history.contains("first")
            );
        }
        service.shutdown().await.unwrap();
        engine.shutdown().await.unwrap();
        sandbox.shutdown().await;
    }
}

#[tokio::test]
async fn tool_streams_live_ask_denial_has_no_queue_effect_and_allow_resumes_original_call() {
    for named in [false, true] {
        let (helpers, sessions, sandbox, _, broker) = if named {
            fixture_with_factory_restriction(
                "allow",
                false,
                None,
                Some(
                    PermissionPolicy::new(crate::permission::PermissionLevel::Allow)
                        .with_tool_rule(
                            PermissionToolSelector::exact("builtin:speak"),
                            PermissionEffect::ask(),
                        ),
                ),
            )
            .await
        } else {
            fixture("ask", false, None).await
        };
        helpers
            .pin_named(if named { vec!["writer".into()] } else { vec![] })
            .await
            .unwrap();
        let streams = super::super::tool_streams::ToolStreams::new(stream_declarations(named));
        helpers.bind_tool_streams(streams.clone()).await.unwrap();
        let cancel = CancellationToken::new();
        let run = helpers.run(
            HelperRequest {
                selection: if named {
                    HelperSelection::Named {
                        name: "writer".into(),
                    }
                } else {
                    HelperSelection::Restricted {
                        profile: "writer".into(),
                    }
                },
                task: "work".into(),
                capture: None,
                max_turns: 4,
                deadline: Duration::from_secs(5),
                observer: None,
            },
            cancel.clone(),
        );
        let consent = async {
            use crate::interaction::InteractionResponse;
            use crate::permission::{PermissionDenyMode, PermissionResponse};
            let id = loop {
                if let Some(session) = sessions.list().await.unwrap().first() {
                    break session.id.clone();
                }
                tokio::task::yield_now().await;
            };
            let mut subscription = broker.subscribe(id.clone());
            let mut previous = String::new();
            for deny in [true, false] {
                let snapshot = loop {
                    let snapshot = subscription.next().await.unwrap();
                    if snapshot.pending.iter().any(|p| p.id != previous) {
                        break snapshot;
                    }
                };
                let request = snapshot.pending.iter().find(|p| p.id != previous).unwrap();
                previous = request.id.clone();
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(10),
                        streams.next("segments", cancel.clone())
                    )
                    .await
                    .is_err(),
                    "Ask must precede admission"
                );
                broker
                    .respond(
                        &id,
                        &snapshot.generation,
                        &request.id,
                        &request.operation_id,
                        InteractionResponse::Permission(if deny {
                            PermissionResponse::Deny {
                                message: None,
                                mode: PermissionDenyMode::ReturnToolResult,
                            }
                        } else {
                            PermissionResponse::AllowOnce
                        }),
                    )
                    .unwrap();
            }
            let event = streams
                .next("segments", cancel.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event["callId"], "second");
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(7), async { tokio::join!(run, consent) })
                .await
                .unwrap();
        result.unwrap();
        assert!(streams.next("segments", cancel).await.unwrap().is_none());
        assert!(streams.drained());
        helpers.cleanup().await.unwrap();
        sandbox.shutdown().await;
    }
}

#[tokio::test]
async fn tool_streams_named_factory_restrictions_deny_before_session_or_inference() {
    for selector in ["speak", "builtin:speak"] {
        let restriction = PermissionPolicy::new(crate::permission::PermissionLevel::Allow)
            .with_tool_rule(
                PermissionToolSelector::exact(selector),
                PermissionEffect::deny(),
            );
        let (helpers, sessions, sandbox, turns, _) =
            fixture_with_factory_restriction("allow", false, None, Some(restriction)).await;
        helpers.pin_named(vec!["writer".into()]).await.unwrap();
        assert_eq!(
            helpers
                .bind_tool_streams(super::super::tool_streams::ToolStreams::new(
                    stream_declarations(true),
                ))
                .await,
            Err(PortError::Denied),
            "factory restriction on {selector} must reject preflight",
        );
        assert!(sessions.list().await.unwrap().is_empty());
        assert_eq!(turns.load(Ordering::SeqCst), 0);
        helpers.cleanup().await.unwrap();
        sandbox.shutdown().await;
    }
}

#[tokio::test]
async fn tool_streams_declarations_do_not_grant_permission_and_collisions_fail_closed() {
    for (named, permission, caller_denies) in [
        (false, "deny", false),
        (true, "deny", false),
        (false, "target-deny", false),
        (true, "target-deny", false),
        (false, "allow", true),
        (true, "allow", true),
    ] {
        let (helpers, sessions, sandbox, turns, _) = fixture(permission, false, None).await;
        let helpers = if caller_denies {
            helpers.with_permission_restriction(PermissionPolicy::new(
                crate::permission::PermissionLevel::Deny,
            ))
        } else {
            helpers
        };
        helpers
            .pin_named(if named { vec!["writer".into()] } else { vec![] })
            .await
            .unwrap();
        assert_eq!(
            helpers
                .bind_tool_streams(super::super::tool_streams::ToolStreams::new(
                    stream_declarations(named)
                ))
                .await,
            Err(PortError::Denied)
        );
        assert!(sessions.list().await.unwrap().is_empty());
        assert_eq!(turns.load(Ordering::SeqCst), 0);
        sandbox.shutdown().await;
    }
    let (helpers, _, sandbox, turns, _) = fixture("allow", true, None).await;
    helpers.pin_named(vec![]).await.unwrap();
    helpers
        .bind_tool_streams(super::super::tool_streams::ToolStreams::new(
            stream_declarations(false),
        ))
        .await
        .unwrap();
    assert!(
        helpers
            .run(
                HelperRequest {
                    selection: HelperSelection::Restricted {
                        profile: "writer".into()
                    },
                    task: "work".into(),
                    capture: None,
                    max_turns: 4,
                    deadline: Duration::from_secs(2),
                    observer: None
                },
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(turns.load(Ordering::SeqCst), 0);
    helpers.cleanup().await.unwrap();
    sandbox.shutdown().await;
}
