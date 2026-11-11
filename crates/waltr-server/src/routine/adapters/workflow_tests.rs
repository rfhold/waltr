use super::*;
use crate::routine::{children::ChildPort, definition::Contract};

#[path = "correction_tests.rs"]
mod correction_tests;

type Helpers = Arc<dyn Fn() -> Arc<dyn HelperRunner> + Send + Sync>;

fn named_workflow_catalog() -> AgentDefinitionCatalog {
    AgentDefinitionCatalog::from_sources(named_workflow_sources(), "source").unwrap()
}

fn named_workflow_snapshot() -> Arc<crate::resource_catalog::AgentDefinitionSnapshot> {
    let root = crate::test_support::temp_root();
    let path = root.join("agents.yaml");
    std::fs::write(&path, named_workflow_sources().join("\n---\n")).unwrap();
    crate::resource_catalog::AgentDefinitionState::load(&root, &[path], "source").unwrap().snapshot()
}

fn named_workflow_sources() -> Vec<String> {
    let mut sources = bundle().sources;
    for (name, prompt) in [
        ("routine-categorization", "CATEGORIZE"),
        ("routine-category-context", "MASK"),
        ("routine-context-strategy", "STRATEGY"),
    ] {
        sources.push(format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: {name}}}\nspec:\n  systemPrompt: {prompt}\n  features: []\n  permissions:\n    - permissionSetRef: {{name: capture}}\n"));
    }
    sources.push("apiVersion: waltr.dev/v1alpha1\nkind: PermissionSet\nmetadata: {name: capture}\nspec:\n  tools:\n    - name: _helper_capture\n      permission: allow\n".into());
    sources
}

fn text_helpers<P: LlmStream + 'static>(
    sessions: SessionCoordinator<MemorySessionStore>,
    factory: Arc<AgentSessionRuntimeFactory>,
    source: SessionCopy,
    profile: &str,
    provider: Arc<P>,
) -> Helpers {
    let profile = profile.to_owned();
    Arc::new(move || {
        Arc::new(
            CoordinatorHelperRunner::new(
                sessions.clone(),
                factory.clone(),
                inference(),
                BTreeMap::from([(
                    profile.clone(),
                    RestrictedHelperProfile {
                        runtime: crate::agent_host::HostRuntimeBuilder::new(
                            model(),
                            provider.clone(),
                        ),
                        inference: inference(),
                        prompt: "Return only the summary.".into(),
                        max_turns: 3,
                    },
                )]),
                BTreeMap::new(),
                Some(source.clone()),
                "owner".into(),
            )
            .unwrap(),
        ) as Arc<dyn HelperRunner>
    })
}

struct AutoAdmission<P: LlmStream + 'static> {
    engine: Arc<Engine>,
    store: Arc<dyn InvocationStore>,
    definition: Arc<Definition>,
    sessions: SessionCoordinator<MemorySessionStore>,
    factory: Arc<AgentSessionRuntimeFactory>,
    provider: Arc<P>,
    services: Arc<Mutex<Vec<InvocationService>>>,
}
impl<P: LlmStream + 'static>
    super::super::super::context::automatic::AutomaticCompactionAdmission<MemorySessionStore>
    for AutoAdmission<P>
{
    fn admit(
        &self,
        context: Arc<CoordinatorContext<MemorySessionStore>>,
        _: SessionRunCompletionSnapshot,
    ) -> PortFuture<crate::routine::service::InvocationHandle> {
        let helpers = text_helpers(
            self.sessions.clone(),
            self.factory.clone(),
            context.helper_copy(),
            "compaction",
            self.provider.clone(),
        );
        let result = InvocationService::new(
            self.engine.clone(),
            self.store.clone(),
            Arc::new(RoutineAgentHostFactory {
                bindings: Arc::new(ContextBindings {
                    context,
                    helpers,
                    capabilities: self.definition.envelope().spec.capabilities.clone(),
                }),
            }),
            Arc::new(SystemClock),
            1,
        );
        let services = self.services.clone();
        let definition = self.definition.clone();
        Box::pin(async move {
            let service = result?;
            {
                let mut retained = services.lock().unwrap();
                if retained.len() >= 16 {
                    return Err(PortError::Denied);
                }
                retained.push(service.clone());
            }
            let mut source = caller();
            source.origin = Origin::Lifecycle;
            service
                .invoke(definition, "completion".into(), source, Value::Null)?
                .await
        })
    }
}

struct HeldSummaryProvider {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}
impl LlmStream for HeldSummaryProvider {
    fn name(&self) -> &'static str {
        "held-summary"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            assert!(request.tools.is_empty());
            assert!(format!("{request:?}").contains("original user context"));
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(CompletionEvent::Delta("safe compacted summary".into())),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

#[tokio::test]
async fn compaction_workflow_uses_claimed_completion_barrier_and_native_continuation() {
    for (tokens, limit, expected) in [
        (Some(180000), None, true),
        (Some(80), Some(100), true),
        (Some(179999), None, false),
        (None, Some(100), false),
    ] {
        let root = std::env::temp_dir().join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
            .await
            .unwrap();
        let catalog = AgentDefinitionCatalog::from_sources(bundle().sources, "source").unwrap();
        let resumed = Arc::new(AtomicUsize::new(0));
        let count = resumed.clone();
        let order = Arc::new(Mutex::new(Vec::new()));
        let observed = order.clone();
        let native_turn = Arc::new(AtomicUsize::new(0));
        let native = Arc::new(ScriptedProvider::new("native", move |request| {
            if native_turn.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut events = vec![Ok(CompletionEvent::Delta("source completed".into()))];
                if let Some(input_tokens) = tokens {
                    events.push(Ok(CompletionEvent::ProviderMetadata(
                        ProviderTurnMetadata {
                            provider: "test".into(),
                            url: String::new(),
                            model: "test".into(),
                            usage: Some(TokenUsage {
                                input_tokens: Some(input_tokens),
                                ..Default::default()
                            }),
                            cost: None,
                            resume: Value::Null,
                        },
                    )));
                }
                events.push(Ok(CompletionEvent::Finished));
                return events;
            }
            assert!(format!("{request:?}").contains("safe compacted summary"));
            observed
                .lock()
                .unwrap()
                .push(if request.prompt.contains("queued user") {
                    "queued"
                } else {
                    "continuation"
                });
            count.fetch_add(1, Ordering::SeqCst);
            vec![
                Ok(CompletionEvent::Delta("continued".into())),
                Ok(CompletionEvent::Finished),
            ]
        }));
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("unused-auto-auth.json")),
            catalog.resolved("source").unwrap().clone(),
        )
        .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new()))
        .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), native.clone())));
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions));
        let id = SessionId::generate();
        let mut create = SessionCreateRequest::new(id.clone());
        create.agent_definition = Some(serde_json::to_value(bundle()).unwrap());
        create.inference_settings = Some(inference());
        sessions.create(create).await.unwrap();
        let engine = Arc::new(Engine::new(4).unwrap());
        let definition = Definition::parse(
            include_str!("../../../tests/fixtures/routines/compact-and-continue.yaml"),
            &*engine,
        )
        .unwrap();
        let database = root.join(format!("s4-auto-{}.sqlite", uuid::Uuid::new_v4()));
        let persistence = Persistence::initialize(
            DatabaseConfig::from_urls(
                &format!("sqlite:{}?mode=rwc", database.display()),
                "sqlite::memory:",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let helpers = Arc::new(AtomicUsize::new(0));
        let helper_started = Arc::new(tokio::sync::Notify::new());
        let release_helper = Arc::new(tokio::sync::Notify::new());
        let queue_case = tokens == Some(180000);
        if !queue_case {
            release_helper.notify_one();
        }
        let provider = Arc::new(HeldSummaryProvider {
            started: helper_started.clone(),
            release: release_helper.clone(),
            calls: helpers.clone(),
        });
        let services = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(
            super::super::super::context::automatic::AutomaticCompactionHandler::new(
                sessions.clone(),
                "owner".into(),
                limit
                    .map(|limit| BTreeMap::from([(("test".into(), "test".into()), limit)]))
                    .unwrap_or_default(),
                Arc::new(AutoAdmission {
                    engine: engine.clone(),
                    store: persistence.script_invocation_store(),
                    definition,
                    sessions: sessions.clone(),
                    factory: Arc::new(factory),
                    provider,
                    services: services.clone(),
                }),
            )
            .unwrap(),
        );
        sessions.set_completion_admission_handler(handler.clone());
        let start = sessions
            .submit_prompt(
                id.clone(),
                "original user context",
                RunConflictPolicy::RejectIfActive,
            )
            .await
            .unwrap();
        start.handle.wait().await.unwrap();
        let queued = if queue_case {
            tokio::time::timeout(Duration::from_secs(2), helper_started.notified())
                .await
                .unwrap();
            let queued = sessions
                .submit_prompt(id.clone(), "queued user", RunConflictPolicy::QueueIfActive)
                .await
                .unwrap();
            release_helper.notify_one();
            Some(queued.handle)
        } else {
            None
        };
        if let Some(queued) = queued {
            tokio::time::timeout(Duration::from_secs(5), queued.wait())
                .await
                .unwrap()
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count = services.lock().unwrap().len();
                if count
                    >= if queue_case {
                        3
                    } else if expected {
                        2
                    } else {
                        1
                    }
                    && sessions.active_state(&id).await.is_none()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "completion timeout tokens={tokens:?} services={} helpers={} resumed={}",
                services.lock().unwrap().len(),
                helpers.load(Ordering::SeqCst),
                resumed.load(Ordering::SeqCst)
            )
        });
        sessions.clear_completion_admission_handler();
        handler.shutdown().await.unwrap();
        assert_eq!(helpers.load(Ordering::SeqCst), usize::from(expected));
        assert_eq!(
            resumed.load(Ordering::SeqCst),
            if queue_case { 2 } else { usize::from(expected) }
        );
        assert_eq!(
            *order.lock().unwrap(),
            if queue_case {
                vec!["continuation", "queued"]
            } else if expected {
                vec!["continuation"]
            } else {
                vec![]
            }
        );
        assert_eq!(
            sessions
                .store()
                .load(&id)
                .await
                .unwrap()
                .active_session_masks()
                .len(),
            usize::from(expected)
        );
        let retained = std::mem::take(&mut *services.lock().unwrap());
        for service in retained {
            service.shutdown().await.unwrap();
        }
        sessions.shutdown().await;
        engine.shutdown().await.unwrap();
        persistence.sqlite_pool().unwrap().close().await;
        std::fs::remove_file(database).unwrap();
    }
}

#[tokio::test]
async fn fork_workflow_starts_exact_captured_destination_and_preserves_it_on_start_failure() {
    for (fail_start, original_allowed) in [(false, true), (true, true), (false, false)] {
        let (base, id) = completed().await;
        let root = std::env::temp_dir().join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
            .await
            .unwrap();
        let catalog = AgentDefinitionCatalog::from_sources(bundle().sources, "source").unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let count = starts.clone();
        let native = Arc::new(ScriptedProvider::new("destination", move |request| {
            assert!(
                request
                    .prompt
                    .contains("fork summary\n\n<fork-direction>\n  exact direction\n\n</fork-direction>")
            );
            count.fetch_add(1, Ordering::SeqCst);
            vec![
                Ok(CompletionEvent::Delta("destination response".into())),
                Ok(CompletionEvent::Finished),
            ]
        }));
        let factory = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("unused-fork-auth.json")),
            catalog.resolved("source").unwrap().clone(),
        )
        .with_capability_policy(crate::permission::ServerPermissionPolicy::sessions(
            crate::interaction::InteractionBroker::new(),
        ))
        .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), native.clone())));
        let sessions = if fail_start {
            base
        } else {
            base.clone()
                .with_runtime_factory(factory.session_factory(base))
        };
        sessions
            .set_metadata_value(&id, "private", json!("do not copy"))
            .await
            .unwrap();
        let before_mask = sessions
            .capture_context(&id, "owner", waltr_agent::SessionCaptureLimits::default())
            .await
            .unwrap();
        let mut mask = waltr_agent::SessionMaskMutation::new(
            sessions
                .store()
                .load(&id)
                .await
                .unwrap()
                .last_timeline_cursor(),
        );
        mask.add.push(waltr_agent::SessionMaskAddition::new(
            "existing",
            waltr_agent::ThroughCursor::new(before_mask.completed.through_cursor),
            waltr_agent::ReplacementUserContent::new("MASKED source summary only"),
        ));
        sessions.store().mutate_masks(&id, mask).await.unwrap();
        let mut changed = inference();
        changed.provider.model_id = "edited".into();
        sessions.store().set_inference_settings(&id, Some(changed)).await.unwrap();
        let context = Arc::new(
            CoordinatorContext::pin(sessions.clone(), &id, "owner", original_allowed)
                .await
                .unwrap(),
        );
        assert_eq!(context.resolve_source("original").is_ok(), original_allowed);
        assert!(context.resolve_source("effective").is_ok());
        assert!(context.resolve_source("foreign-source").is_err());
        let effective = text_helpers(
            sessions.clone(),
            Arc::new(factory.clone()),
            context.helper_copy(),
            "compaction",
            Arc::new(ScriptedProvider::new("effective-compaction", |request| {
                let input = format!("{request:?}");
                assert!(input.contains("MASKED source summary only"));
                assert!(!input.contains("original user context"));
                vec![
                    Ok(CompletionEvent::Delta("effective summary".into())),
                    Ok(CompletionEvent::Finished),
                ]
            })),
        )();
        effective
            .run(
                HelperRequest {
                    selection: HelperSelection::Restricted {
                        profile: "compaction".into(),
                    },
                    task: "summarize".into(),
                    capture: None,
                    max_turns: 1,
                    deadline: Duration::from_secs(5),
                    observer: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        effective.cleanup().await.unwrap();
        let captured = sessions.store().load(&id).await.unwrap();
        let provider = Arc::new(ScriptedProvider::new("fork", |request| {
            assert!(request.tools.is_empty());
            assert!(
                request
                    .prompt
                    .contains("<fork-direction>\n  exact direction\n\n</fork-direction>")
            );
            assert!(!format!("{request:?}").contains("original user context"));
            assert!(format!("{request:?}").contains("MASKED source summary only"));
            vec![
                Ok(CompletionEvent::Delta("fork summary".into())),
                Ok(CompletionEvent::Finished),
            ]
        }));
        let helpers = text_helpers(
            sessions.clone(),
            Arc::new(factory),
            context.helper_copy(),
            "fork-compaction",
            provider,
        );
        let engine = Arc::new(Engine::new(1).unwrap());
        let definition =
            Definition::parse(include_str!("../../../tests/fixtures/routines/fork.yaml"), &*engine).unwrap();
        let database = root.join(format!("s4-fork-{}.sqlite", uuid::Uuid::new_v4()));
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
            Arc::new(RoutineAgentHostFactory {
                bindings: Arc::new(ContextBindings {
                    context,
                    helpers,
                    capabilities: definition.envelope().spec.capabilities.clone(),
                }),
            }),
            Arc::new(SystemClock),
            1,
        )
        .unwrap();
        let handle = service
            .invoke(
                definition,
                "run".into(),
                caller(),
                json!({"direction":"  exact direction\n"}),
            )
            .unwrap()
            .await
            .unwrap();
        let outcome = handle.join().await;
        assert_eq!(
            outcome.status,
            Some(if fail_start {
                Status::Uncertain
            } else {
                Status::Succeeded
            })
        );
        if fail_start {
            let operations = persistence.script_invocation_store().operations(handle.id.clone(), 0, 100).await.unwrap();
            assert_eq!(operations.iter().filter(|operation| operation.name == "context.create" && operation.status == crate::routine::history::OperationStatus::Completed).count(), 1);
            assert_eq!(operations.iter().filter(|operation| operation.name == "context.start" && operation.status == crate::routine::history::OperationStatus::Uncertain).count(), 1);
        }
        assert!(outcome.cleanup_complete);
        let destinations = sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|summary| summary.id != id)
            .collect::<Vec<_>>();
        let mut found = 0;
        for summary in destinations {
            let record = sessions.store().load(&summary.id).await.unwrap();
            if record.materialize_context(waltr_agent::SessionContextMode::Raw).current_prompt.as_deref() != Some("fork summary\n\n<fork-direction>\n  exact direction\n\n</fork-direction>") {
                continue;
            }
            found += 1;
            assert_eq!(record.agent_definition, captured.agent_definition);
            assert_eq!(record.inference_settings, captured.inference_settings);
            assert!(!record.metadata.values.contains_key("private"));
        }
        assert_eq!(found, 1);
        sessions.shutdown().await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            usize::from(!fail_start)
        );
        service.shutdown().await.unwrap();
        engine.shutdown().await.unwrap();
        persistence.sqlite_pool().unwrap().close().await;
        std::fs::remove_file(database).unwrap();
    }
}

#[tokio::test]
async fn manual_compaction_rejects_busy_before_history_then_commits_without_continuation() {
    let (sessions, id) = completed().await;
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let catalog = AgentDefinitionCatalog::from_sources(bundle().sources, "source").unwrap();
    let factory = Arc::new(AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("unused-manual-auth.json")),
        catalog.resolved("source").unwrap().clone(),
    ));
    let engine = Arc::new(Engine::new(1).unwrap());
    let definition = Definition::parse(
        include_str!("../../../tests/fixtures/routines/compact-and-continue.yaml"),
        &*engine,
    )
    .unwrap();
    let database = root.join(format!("s4-manual-{}.sqlite", uuid::Uuid::new_v4()));
    let persistence = Persistence::initialize(
        DatabaseConfig::from_urls(
            &format!("sqlite:{}?mode=rwc", database.display()),
            "sqlite::memory:",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.script_invocation_store();
    for (busy, cancel) in [(true, false), (false, true), (false, false)] {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        if !cancel {
            release.notify_one();
        }
        let provider = Arc::new(HeldSummaryProvider {
            started: started.clone(),
            release,
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let lease = if busy {
            Some(
                sessions
                    .acquire_pause_reject_busy(
                        &id,
                        SessionCoordinationReason::new("test.busy", None, None).unwrap(),
                    )
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let context = Arc::new(
            CoordinatorContext::compaction(sessions.clone(), &id, "owner")
                .await
                .unwrap(),
        );
        let helpers = text_helpers(
            sessions.clone(),
            factory.clone(),
            context.helper_copy(),
            "compaction",
            provider.clone(),
        );
        let service = InvocationService::new(
            engine.clone(),
            store.clone(),
            Arc::new(RoutineAgentHostFactory {
                bindings: Arc::new(ContextBindings {
                    context,
                    helpers,
                    capabilities: definition.envelope().spec.capabilities.clone(),
                }),
            }),
            Arc::new(SystemClock),
            1,
        )
        .unwrap();
        let admission = service
            .invoke(definition.clone(), "run".into(), caller(), Value::Null)
            .unwrap()
            .await;
        if busy {
            assert!(matches!(admission, Err(PortError::Conflict)));
            assert!(store.list(0, 100).await.unwrap().is_empty());
            sessions.release_pause(&lease.unwrap()).await.unwrap();
        } else {
            let handle = admission.unwrap();
            if cancel {
                tokio::time::timeout(Duration::from_secs(2), started.notified())
                    .await
                    .unwrap();
                handle.cancel();
                let outcome = tokio::time::timeout(Duration::from_secs(2), handle.join())
                    .await
                    .unwrap();
                // Cancellation leaves the admitted host-call intent without a result acknowledgment.
                assert_eq!(outcome.status, Some(Status::Uncertain));
                assert!(outcome.cleanup_complete);
                assert!(
                    sessions
                        .store()
                        .load(&id)
                        .await
                        .unwrap()
                        .active_session_masks()
                        .is_empty()
                );
                let lease = sessions
                    .acquire_pause_reject_busy(
                        &id,
                        SessionCoordinationReason::new("test.cancel-released", None, None).unwrap(),
                    )
                    .await
                    .unwrap();
                sessions.release_pause(&lease).await.unwrap();
                service.shutdown().await.unwrap();
                continue;
            }
            let outcome = handle.join().await;
            assert_eq!(outcome.status, Some(Status::Succeeded));
            assert_eq!(outcome.result, Some(json!(true)));
            assert!(outcome.cleanup_complete);
            let record = sessions.store().load(&id).await.unwrap();
            assert_eq!(record.active_session_masks().len(), 1);
            assert_eq!(
                record
                    .materialize_context(SessionContextMode::Effective)
                    .current_prompt
                    .as_deref(),
                Some("safe compacted summary")
            );
            assert!(sessions.active_state(&id).await.is_none());
        }
        service.shutdown().await.unwrap();
    }
    sessions.shutdown().await;
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(database).unwrap();
}

struct IgnoreMessages;
impl SessionEventSink for IgnoreMessages {
    fn send(&self, _: SessionEvent) -> SessionEventSinkFuture {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn actual_task_children_feed_the_bounded_semantic_consumer_without_tool_result_counting() {
    use crate::routine::triggers::{SemanticTriggers, TriggerSource};
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let catalog=AgentDefinitionCatalog::from_sources(vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: parent}\nspec:\n  features: [{featureSetRef: {name: task}}]\n---\napiVersion: waltr.dev/v1alpha1\nkind: FeatureSet\nmetadata: {name: task}\nspec:\n  task: {maxSubagentDepth: 1, defaultPermissions: true}\n".into()],"parent").unwrap();
    let provider = Arc::new(ScriptedProvider::new("task-trigger", |request| {
        if request.prompt == "child" {
            return vec![
                Ok(CompletionEvent::Delta("child result".into())),
                Ok(CompletionEvent::Finished),
            ];
        }
        if request.tool_results().next().is_none() {
            vec![
                Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "first",
                    "task",
                    json!({"prompt":"child","mode":"sync"}),
                ))),
                Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "second",
                    "task",
                    json!({"prompt":"child","mode":"sync"}),
                ))),
            ]
        } else {
            vec![Ok(CompletionEvent::Finished)]
        }
    }));
    let factory = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("unused-task-trigger-auth.json")),
        catalog.resolved("parent").unwrap().clone(),
    )
    .with_capability_policy(crate::permission::ServerPermissionPolicy::sessions(
        crate::interaction::InteractionBroker::new(),
    ))
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), provider.clone())));
    let hub = Arc::new(SessionSemanticHub::new(64).unwrap());
    let mut source = hub.subscribe().unwrap();
    let sessions = SessionCoordinator::new(MemorySessionStore::new()).with_session_event_sink(
        Arc::new(SemanticSessionEventSink {
            messages: Arc::new(IgnoreMessages),
            semantics: hub,
        }),
    );
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let id = SessionId::generate();
    let mut request = SessionCreateRequest::new(id.clone());
    request.inference_settings = Some(inference());
    sessions.create(request).await.unwrap();
    let record = sessions.store().load(&id).await.unwrap();
    let mut triggers = SemanticTriggers::default();
    triggers
        .watch(id.clone(), record.metadata.incarnation.unwrap())
        .unwrap();
    sessions
        .submit_prompt(id.clone(), "spawn two", RunConflictPolicy::RejectIfActive)
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    let mut task_facts = 0;
    let mut triggered = 0;
    tokio::time::timeout(Duration::from_secs(2), async {
        while task_facts < 2 {
            let event = TriggerSource::next(&mut source).await.unwrap();
            if let Some(origin) = factory.session_roles.complete_task(&event) {
                task_facts += 1;
                if let Some(trigger) = triggers.task_completed(event, &origin) {
                assert_eq!(trigger.session_id, id);
                assert!(trigger.requested_boundary.is_none());
                triggered += 1;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(triggered, 1);
    sessions.shutdown().await;
}

#[derive(Clone)]
struct WorkflowBindings {
    context: Arc<CoordinatorContext<MemorySessionStore>>,
    helpers: Helpers,
    service: Arc<Mutex<Option<InvocationService>>>,
    definitions: Arc<BTreeMap<String, Arc<Definition>>>,
    scope: Option<ChildScope>,
    administrative_probe: Option<Arc<correction_tests::AdministrativeProbe>>,
}
impl InvocationAdapterFactory for WorkflowBindings {
    fn bind(
        &self,
        definition: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<AdapterBindings> {
        let helpers = (self.helpers)();
        let capabilities = definition.envelope().spec.capabilities.clone();
        Box::pin(async move {
            Ok(AdapterBindings {
                principal: "owner".into(),
                helpers,
                tools: Arc::new(SubstitutePorts::default()),
                capabilities,
                targets: BTreeMap::new(),
            })
        })
    }
    fn bind_context(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<Option<Arc<dyn ContextPort>>> {
        let context = self.context.clone();
        let probe = self.administrative_probe.clone();
        Box::pin(async move {
            Ok(Some(if let Some(probe) = probe {
                Arc::new(correction_tests::ObservedContext { context, probe })
                    as Arc<dyn ContextPort>
            } else {
                context as Arc<dyn ContextPort>
            }))
        })
    }
    fn bind_children(
        &self,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<Option<Arc<dyn ChildPort>>> {
        let service = self.service.lock().unwrap().clone().unwrap();
        let definitions = (*self.definitions).clone();
        let scope = self.scope.clone().map(Ok).unwrap_or_else(|| {
            ChildScope::root(
                self.context.snapshot_identity().unwrap(),
                "owner".into(),
                definition.envelope().spec.capabilities.clone(),
                format!("{}:{}", definition.envelope().name, selected.name),
                tokio::time::Instant::now() + Duration::from_secs(25),
            )
        });
        Box::pin(async move {
            Ok(Some(
                Arc::new(ServiceChildren::new(service, definitions, scope?)?) as Arc<dyn ChildPort>,
            ))
        })
    }
    fn child_bindings(&self, scope: ChildScope) -> PortFuture<Arc<dyn InvocationAdapterFactory>> {
        let mut child = self.clone();
        child.context = Arc::new(self.context.child_context());
        child.scope = Some(scope);
        Box::pin(async move { Ok(Arc::new(child) as Arc<dyn InvocationAdapterFactory>) })
    }
}

fn contract(schema: Value) -> Contract {
    serde_json::from_value(json!({"schemaVersion":"waltr.value/v1","schema":schema})).unwrap()
}
fn capture_contracts() -> BTreeMap<String, Contract> {
    BTreeMap::from([
        (
            "categories".into(),
            contract(json!({"type":"object","properties":{
            "categories":{"type":"array","maxItems":64,"items":{"type":"object","properties":{"key":{"type":"string","maxLength":119},"description":{"type":"string","maxLength":2048}},"required":["key","description"]}},
            "assignments":{"type":"array","maxItems":4096,"items":{"type":"object","properties":{"item_id":{"type":"string","maxLength":128},"category":{"type":"string","maxLength":119}},"required":["item_id","category"]}},
            "note":{"type":"string","maxLength":8192}},"required":["categories","assignments","note"]})),
        ),
        (
            "mask-decision".into(),
            contract(
                json!({"type":"object","properties":{"kind":{"type":"string","maxLength":16},"summary":{"type":"string","maxLength":8192}},"required":["kind"]}),
            ),
        ),
        (
            "context-strategy".into(),
            contract(
                json!({"type":"object","properties":{"kind":{"type":"string","maxLength":16},"edits":{"type":"array","maxItems":4,"items":{"type":"object","properties":{"key":{"type":"string","maxLength":119},"instruction":{"type":"string","maxLength":8192}},"required":["key","instruction"]}}},"required":["kind"]}),
            ),
        ),
    ])
}

fn definitions(engine: &Engine) -> BTreeMap<String, Arc<Definition>> {
    [
        include_str!("../../../resources/routines/context-categorization.yaml"),
        include_str!("../../../resources/routines/manage-category-context.yaml"),
        include_str!("../../../resources/routines/context-management.yaml"),
        include_str!("../../../tests/fixtures/routines/fork.yaml"),
        include_str!("../../../tests/fixtures/routines/compact-and-continue.yaml"),
    ]
    .into_iter()
    .map(|source| {
        let definition = Definition::parse(source, engine).unwrap();
        (definition.envelope().name.clone(), definition)
    })
    .collect()
}

#[tokio::test]
async fn categorization_and_category_management_use_real_helpers_and_nullable_results() {
    for (invalid_coverage, key) in [
        (false, "work".into()),
        (true, "work".into()),
        (false, "k".repeat(119)),
        (false, "k".repeat(120)),
        (false, "k".repeat(128)),
    ] {
        let (sessions, id) = completed().await;
        let context = Arc::new(
            CoordinatorContext::pin(sessions.clone(), &id, "owner", true)
                .await
                .unwrap(),
        );
        let source = context
            .execute(
                operation("context.original", json!({})),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut assignments = source["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| json!({"item_id":item["item_id"],"category":key}))
            .collect::<Vec<_>>();
        if invalid_coverage {
            assignments.pop();
        }
        let update = json!({"categories":[{"key":key,"description":"immutable work"}],"assignments":assignments,"note":"checkpoint note"});
        let category_turn = Arc::new(AtomicUsize::new(0));
        let mask_turn = Arc::new(AtomicUsize::new(0));
        let strategy_turn = Arc::new(AtomicUsize::new(0));
        let strategy_key = key.clone();
        let invalid_key = key.len() > 119;
        let provider = Arc::new(ScriptedProvider::new("workflow", move |request| {
            let (turn, value) = if invalid_key || request.system_prompt == Some("CATEGORIZE") {
                if !invalid_key {
                    assert!(format!("{request:?}").contains("original user context"));
                }
                (category_turn.fetch_add(1, Ordering::SeqCst), update.clone())
            } else if request.system_prompt == Some("STRATEGY") {
                (
                    strategy_turn.fetch_add(1, Ordering::SeqCst),
                    json!({"kind":"edit","edits":[{"key":strategy_key,"instruction":"retain important facts"}]}),
                )
            } else {
                assert_eq!(request.system_prompt, Some("MASK"));
                assert!(format!("{request:?}").contains("original user context"));
                let task: Value = serde_json::from_str(request.prompt).unwrap();
                let background = task["selected"]["background"].as_array().unwrap();
                assert!(background.len() <= 16);
                assert!(background.iter().all(|item| item["excerpt"].as_str().unwrap().encode_utf16().count() <= 240));
                let turn = mask_turn.fetch_add(1, Ordering::SeqCst);
                (
                    turn,
                    if turn < 2 {
                        json!({"kind":"replace","summary":"safe category summary"})
                    } else {
                        json!({"kind":"keep"})
                    },
                )
            };
            let event = if turn % 2 == 0 {
                CompletionEvent::ToolCall(ToolCall::new(
                    format!("capture-{turn}"),
                    "_helper_capture",
                    value,
                ))
            } else {
                CompletionEvent::Delta("accepted".into())
            };
            vec![Ok(event), Ok(CompletionEvent::Finished)]
        }));
        let root = std::env::temp_dir().join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
            .await
            .unwrap();
        let catalog = named_workflow_catalog();
        let live_catalog = catalog.clone();
        let supplied = provider.clone();
        let factory = Arc::new(
            AgentSessionRuntimeFactory::new(
                sandbox.handle(),
                root.clone(),
                FileCredentialStore::new(root.join("unused-workflow-auth.json")),
                catalog.resolved("source").unwrap().clone(),
            )
            .with_agent_catalog_provider(move || live_catalog.clone())
            .with_capability_policy(crate::permission::ServerPermissionPolicy::sessions(
                crate::interaction::InteractionBroker::new(),
            ))
            .with_runtime_provider(move |_| {
                Ok(AgentRuntimeProvider::new(model(), supplied.clone()))
            }),
        );
        let helper_sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions.clone()));
        let helper_context = context.clone();
        let helper_catalog = named_workflow_snapshot();
        let helpers: Helpers = Arc::new(move || {
            Arc::new(
                CoordinatorHelperRunner::new(
                    helper_sessions.clone(),
                    factory.clone(),
                    inference(),
                    BTreeMap::new(),
                    capture_contracts(),
                    Some(helper_context.helper_copy()),
                    "owner".into(),
                )
                .unwrap().with_catalog(helper_catalog.clone(), Some(inference())),
            )
        });
        let engine = Arc::new(Engine::new(4).unwrap());
        let definitions = Arc::new(definitions(&engine));
        let database = root.join(format!("s4-workflow-{}.sqlite", uuid::Uuid::new_v4()));
        let persistence = Persistence::initialize(
            DatabaseConfig::from_urls(
                &format!("sqlite:{}?mode=rwc", database.display()),
                "sqlite::memory:",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let store = persistence.script_invocation_store();
        let slot = Arc::new(Mutex::new(None));
        let bindings = WorkflowBindings {
            context: Arc::new(context.child_context()),
            helpers: helpers.clone(),
            service: slot.clone(),
            definitions: definitions.clone(),
            scope: None,
            administrative_probe: None,
        };
        let service = InvocationService::new(
            engine.clone(),
            store.clone(),
            Arc::new(RoutineAgentHostFactory {
                bindings: Arc::new(bindings),
            }),
            Arc::new(SystemClock),
            4,
        )
        .unwrap();
        *slot.lock().unwrap() = Some(service.clone());
        let coalescer = crate::routine::triggers::InvocationCoalescer::default();
        let invocation = crate::routine::triggers::InvocationIdentity { routine: "context-categorization".into(), revision: definitions["context-categorization"].revision().into(), entrypoint: "run".into() };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let crate::routine::triggers::InvocationAdmission::Leader(mut leader) = coalescer
            .admit(id.clone(), context.snapshot_identity().unwrap(), invocation.clone(), deadline)
            .unwrap()
        else {
            panic!("leader")
        };
        let crate::routine::triggers::InvocationAdmission::Waiter(waiter) = coalescer
            .admit(id.clone(), context.snapshot_identity().unwrap(), invocation.clone(), deadline)
            .unwrap()
        else {
            panic!("waiter")
        };
        let handle = service
            .invoke(
                definitions["context-categorization"].clone(),
                "run".into(),
                caller(),
                Value::Null,
            )
            .unwrap()
            .await
            .unwrap();
        leader.bind_execution(&handle.id).unwrap();
        let outcome = handle.join().await;
        assert!(outcome.cleanup_complete);
        if invalid_coverage || key.len() > 119 {
            drop(leader);
            assert!(waiter.await.is_err());
            assert_ne!(outcome.status, Some(Status::Succeeded));
            assert!(
                !sessions
                    .store()
                    .load(&id)
                    .await
                    .unwrap()
                    .metadata
                    .values
                    .contains_key("routine.categories.v1")
            );
        } else {
            assert_eq!(outcome.status, Some(Status::Succeeded), "{outcome:?}; operations={:?}", store.operations(handle.id.clone(), 0, 64).await);
            leader
                .complete(context.committed_receipt(invocation.clone(), &outcome).await.unwrap())
                .unwrap();
            assert_eq!(waiter.await.unwrap(), "checkpoint note");
            assert_eq!(outcome.status, Some(Status::Succeeded));
            assert_eq!(outcome.result, Some(json!("checkpoint note")));
            // A fresh invocation retains the exact original source and only acknowledged metadata.
            for expected in [true, false] {
                let value = sessions.clone();
                let bindings = WorkflowBindings {
                    context: Arc::new(context.child_context()),
                    helpers: match &service {
                        _ => {
                            // Reuse the same authenticated factory, not an admitted helper owner.
                            let helper_sessions = sessions.clone();
                            let source = context.helper_copy();
                            let provider_turn = Arc::new(AtomicUsize::new(0));
                            let provider = Arc::new(ScriptedProvider::new(
                                "category",
                                move |request| {
                                    assert!(
                                        format!("{request:?}").contains("original user context")
                                    );
                                    let event = if provider_turn.fetch_add(1, Ordering::SeqCst) == 0
                                    {
                                        CompletionEvent::ToolCall(ToolCall::new(
                                            "decision",
                                            "_helper_capture",
                                            if expected {
                                                json!({"kind":"replace","summary":"safe category summary"})
                                            } else {
                                                json!({"kind":"keep"})
                                            },
                                        ))
                                    } else {
                                        CompletionEvent::Delta("accepted".into())
                                    };
                                    vec![Ok(event), Ok(CompletionEvent::Finished)]
                                },
                            ));
                            let live_catalog = catalog.clone();
                            let supplied = provider.clone();
                            let factory = Arc::new(
                                AgentSessionRuntimeFactory::new(
                                    sandbox.handle(),
                                    root.clone(),
                                    FileCredentialStore::new(
                                        root.join("unused-workflow-auth.json"),
                                    ),
                                    catalog.resolved("source").unwrap().clone(),
                                )
                                .with_agent_catalog_provider(move || live_catalog.clone())
                                .with_capability_policy(
                                    crate::permission::ServerPermissionPolicy::sessions(
                                        crate::interaction::InteractionBroker::new(),
                                    ),
                                )
                                .with_runtime_provider(move |_| {
                                    Ok(AgentRuntimeProvider::new(model(), supplied.clone()))
                                }),
                            );
                            let helper_catalog = named_workflow_snapshot();
                            Arc::new(move || {
                                Arc::new(
                                    CoordinatorHelperRunner::new(
                                        helper_sessions.clone().with_runtime_factory(
                                            factory.session_factory(value.clone()),
                                        ),
                                        factory.clone(),
                                        inference(),
                                        BTreeMap::new(),
                                        capture_contracts(),
                                        Some(source.clone()),
                                        "owner".into(),
                                    )
                                    .unwrap().with_catalog(helper_catalog.clone(), Some(inference())),
                                ) as Arc<dyn HelperRunner>
                            }) as Helpers
                        }
                    },
                    service: slot.clone(),
                    definitions: definitions.clone(),
                    scope: None,
                    administrative_probe: None,
                };
                let child_service = InvocationService::new(
                    engine.clone(),
                    store.clone(),
                    Arc::new(RoutineAgentHostFactory {
                        bindings: Arc::new(bindings),
                    }),
                    Arc::new(SystemClock),
                    4,
                )
                .unwrap();
                let handle = child_service
                    .invoke(
                        definitions["manage-category-context"].clone(),
                        "run".into(),
                        caller(),
                        json!({"key":key,"instruction":"retain important facts"}),
                    )
                    .unwrap()
                    .await
                    .unwrap();
                let result = handle.join().await;
                assert_eq!(result.status, Some(Status::Succeeded), "{result:?}");
                let result = result.result.unwrap();
                assert_eq!(result["applied"], expected);
                assert_eq!(result["maskId"].is_null(), !expected);
                child_service.shutdown().await.unwrap();
            }
            assert_eq!(
                sessions
                    .store()
                    .load(&id)
                    .await
                    .unwrap()
                    .active_session_masks()
                    .len(),
                1
            );
            let bindings = WorkflowBindings {
                context: Arc::new(context.child_context()),
                helpers,
                service: slot.clone(),
                definitions: definitions.clone(),
                scope: None,
                administrative_probe: None,
            };
            let management = InvocationService::new(
                engine.clone(),
                store.clone(),
                Arc::new(RoutineAgentHostFactory {
                    bindings: Arc::new(bindings),
                }),
                Arc::new(SystemClock),
                4,
            )
            .unwrap();
            *slot.lock().unwrap() = Some(management.clone());
            let mut tool_caller = caller();
            tool_caller.origin = Origin::Tool;
            tool_caller.ownership = Ownership::Tool;
            let handle = management
                .invoke(
                    definitions["context-management"].clone(),
                    "session-tool".into(),
                    tool_caller,
                    json!({"instruction":"preserve important work"}),
                )
                .unwrap()
                .await
                .unwrap();
            let outcome = handle.join().await;
            assert_eq!(
                outcome.status,
                Some(Status::Succeeded),
                "{outcome:?} {:?}",
                store.operations(handle.id.clone(), 0, 32).await.unwrap()
            );
            let decisions = outcome.result.as_ref().unwrap()["decisions"]
                .as_array()
                .unwrap()
                .clone();
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0]["succeeded"], true);
            assert_eq!(decisions[0]["result"]["applied"], true);
            assert!(decisions[0]["result"]["maskId"].is_string());
            management.shutdown().await.unwrap();
            sessions
                .set_metadata_value(&id, "routine.categories.v1", json!({"newer":"checkpoint"}))
                .await
                .unwrap();
            assert!(matches!(
                context.committed_receipt(invocation.clone(), &outcome).await,
                Err(PortError::Conflict)
            ));
        }
        service.shutdown().await.unwrap();
        slot.lock().unwrap().take();
        sessions.shutdown().await;
        engine.shutdown().await.unwrap();
        persistence.sqlite_pool().unwrap().close().await;
        std::fs::remove_file(database).unwrap();
    }
}
