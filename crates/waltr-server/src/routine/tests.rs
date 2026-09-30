use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::host::{
    PauseCaptureFuture, PauseFuture, PauseMaskFuture, PauseUnitFuture, RoutinePause,
    RoutinePauseGuard,
};
use super::*;

type SubmittedRun = (String, NonZeroU64);

struct AutomaticUsageProvider;

impl waltr_agent::LlmStream for AutomaticUsageProvider {
    fn name(&self) -> &'static str {
        "automatic_usage_provider"
    }

    fn stream_request<'a>(
        &'a self,
        _request: waltr_agent::CompletionRequest<'a>,
    ) -> waltr_agent::BoxStreamFuture<'a> {
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(waltr_agent::CompletionEvent::Delta("complete".to_owned())),
                Ok(waltr_agent::CompletionEvent::ProviderMetadata(
                    waltr_agent::ProviderTurnMetadata {
                        provider: "test".to_owned(),
                        url: String::new(),
                        model: "test-model".to_owned(),
                        usage: Some(waltr_agent::TokenUsage {
                            input_tokens: Some(180_000),
                            ..waltr_agent::TokenUsage::default()
                        }),
                        cost: None,
                        resume: Value::Null,
                    },
                )),
                Ok(waltr_agent::CompletionEvent::Finished),
            ])) as waltr_agent::BoxCompletionStream)
        })
    }
}

#[derive(Default)]
struct FakeHost {
    host_calls: AtomicUsize,
    children: Mutex<Vec<SessionCreateRequest>>,
    writes: Mutex<Vec<(String, String, Value)>>,
    response: Mutex<Option<Result<Option<String>, String>>>,
    block_capture: AtomicBool,
    capture_entered: AtomicBool,
    capture_release: tokio::sync::Notify,
    block_create: AtomicBool,
    create_entered: AtomicBool,
    create_release: tokio::sync::Notify,
    block_submit: AtomicBool,
    submit_entered: AtomicBool,
    block_runs: AtomicBool,
    block_load: AtomicBool,
    load_entered: AtomicBool,
    load_release: tokio::sync::Notify,
    run_started: tokio::sync::Notify,
    submitted_runs: AtomicUsize,
    active_runs: Arc<Mutex<HashSet<String>>>,
    interrupts: AtomicUsize,
    capture: Mutex<Option<waltr_agent::CompletedSessionCapture>>,
    submissions: Mutex<Vec<SubmittedRun>>,
    destinations: Mutex<Vec<SessionCreateRequest>>,
    starts: AtomicUsize,
    block_destination: AtomicBool,
    destination_entered: AtomicBool,
    destination_release: tokio::sync::Notify,
    block_start: AtomicBool,
    start_entered: AtomicBool,
    fail_destination: AtomicBool,
    panic_destination: AtomicBool,
    fail_start: AtomicBool,
    paused: AtomicBool,
    pause_reasons: Mutex<Vec<waltr_agent::SessionCoordinationReason>>,
    pause_captures: AtomicUsize,
    pause_releases: AtomicUsize,
    mask_mutations: Mutex<Vec<waltr_agent::SessionMaskMutation>>,
    continuations: Mutex<Vec<waltr_agent::TimelineCursor>>,
    fail_mask: AtomicBool,
    fail_continue: AtomicBool,
    block_release_once: AtomicBool,
    release_entered: tokio::sync::Notify,
}

impl FakeHost {
    fn with_response(response: Result<Option<&str>, &str>) -> Self {
        Self {
            response: Mutex::new(Some(
                response
                    .map(|value| value.map(ToOwned::to_owned))
                    .map_err(ToOwned::to_owned),
            )),
            ..Self::default()
        }
    }

    fn with_capture(response: &str) -> Self {
        let mut bundle = waltr_agent::AgentDefinitionBundle::new("source-agent", vec![
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: source-agent\nspec:\n  systemPrompt: source\n".to_owned(),
        ]);
        bundle.pending_switch_audit = Some(waltr_agent::AgentSwitchAudit {
            id: "audit".to_owned(),
            old_name: "old".to_owned(),
            new_name: "source-agent".to_owned(),
        });
        let source_id = SessionId::new("source-session");
        let call = waltr_agent::ToolCall::new("call-0", "source-tool-0", json!({"turn": 0}));
        let result = waltr_agent::ToolResult::new("call-0", "source-tool-0", "source result 0");
        let mut source = waltr_agent::SessionRecord::new(source_id.clone());
        source.journal = vec![
            waltr_agent::SessionJournalEvent::PromptAppended {
                prompt: "source prompt".to_owned(),
            },
            waltr_agent::SessionJournalEvent::Timeline(waltr_agent::AgentTimelineEvent {
                cursor: waltr_agent::TimelineCursor::new(1),
                session_id: source_id.clone(),
                turn: 0,
                item_id: waltr_agent::TimelineItemId::from_first_cursor(
                    waltr_agent::TimelineCursor::new(1),
                ),
                parent_id: None,
                kind: waltr_agent::TimelineEventKind::ItemFinished,
                channel: Some(waltr_agent::TimelineChannel::new(
                    waltr_agent::CHANNEL_USER_TEXT,
                )),
                content: waltr_agent::TimelineContent::TextSnapshot {
                    text: "source prompt".to_owned(),
                },
                status: Some(waltr_agent::TimelineStatus::Finished),
                metadata: json!({}),
            }),
            waltr_agent::SessionJournalEvent::Timeline(waltr_agent::AgentTimelineEvent {
                cursor: waltr_agent::TimelineCursor::new(2),
                session_id: source_id.clone(),
                turn: 0,
                item_id: waltr_agent::TimelineItemId::from_first_cursor(
                    waltr_agent::TimelineCursor::new(2),
                ),
                parent_id: None,
                kind: waltr_agent::TimelineEventKind::ItemStarted,
                channel: Some(waltr_agent::TimelineChannel::new(
                    waltr_agent::CHANNEL_TOOL_CALL,
                )),
                content: waltr_agent::TimelineContent::ToolCall { call },
                status: Some(waltr_agent::TimelineStatus::Pending),
                metadata: json!({}),
            }),
            waltr_agent::SessionJournalEvent::Timeline(waltr_agent::AgentTimelineEvent {
                cursor: waltr_agent::TimelineCursor::new(3),
                session_id: source_id.clone(),
                turn: 0,
                item_id: waltr_agent::TimelineItemId::from_first_cursor(
                    waltr_agent::TimelineCursor::new(3),
                ),
                parent_id: Some(waltr_agent::TimelineItemId::from_first_cursor(
                    waltr_agent::TimelineCursor::new(2),
                )),
                kind: waltr_agent::TimelineEventKind::ItemFinished,
                channel: Some(waltr_agent::TimelineChannel::new(
                    waltr_agent::CHANNEL_TOOL_RESULT,
                )),
                content: waltr_agent::TimelineContent::ToolResult { result },
                status: Some(waltr_agent::TimelineStatus::Finished),
                metadata: json!({}),
            }),
            waltr_agent::SessionJournalEvent::Timeline(waltr_agent::AgentTimelineEvent {
                cursor: waltr_agent::TimelineCursor::new(4),
                session_id: source_id.clone(),
                turn: 0,
                item_id: waltr_agent::TimelineItemId::from_first_cursor(
                    waltr_agent::TimelineCursor::new(4),
                ),
                parent_id: None,
                kind: waltr_agent::TimelineEventKind::ItemFinished,
                channel: Some(waltr_agent::TimelineChannel::new(
                    waltr_agent::CHANNEL_ASSISTANT_TEXT,
                )),
                content: waltr_agent::TimelineContent::TextSnapshot {
                    text: "final source answer".to_owned(),
                },
                status: Some(waltr_agent::TimelineStatus::Finished),
                metadata: json!({}),
            }),
            waltr_agent::SessionJournalEvent::TurnFinished { turn: 0 },
        ];
        let copy =
            waltr_agent::SessionCopy::from_record(&source, waltr_agent::TimelineCursor::new(4))
                .expect("valid source copy");
        Self {
            capture: Mutex::new(Some(waltr_agent::CompletedSessionCapture {
                source_id,
                through_cursor: waltr_agent::TimelineCursor::new(4),
                completed_turn: 0,
                inference_settings: test_inference(),
                agent_definition: bundle,
                copy,
            })),
            response: Mutex::new(Some(Ok(Some(response.to_owned())))),
            ..Self::default()
        }
    }
}

fn test_inference() -> InferenceSettings {
    serde_json::from_value(json!({
        "provider": {
            "key": "test", "api": "OpenAiResponses", "url": "http://localhost",
            "model_id": "test-model", "model_cost": null, "headers": {}, "oauth": null,
            "reasoning_effort": "medium"
        }
    }))
    .unwrap()
}

impl RoutineHost for FakeHost {
    fn capture_latest_completed<'a>(&'a self, _id: &'a SessionId) -> CaptureFuture<'a> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.block_capture.load(Ordering::SeqCst) {
                self.capture_entered.store(true, Ordering::SeqCst);
                self.capture_release.notified().await;
            }
            self.capture.lock().unwrap().clone().ok_or_else(|| {
                waltr_agent::SessionCaptureError::FailedPrecondition(
                    "capture unavailable".to_owned(),
                )
            })
        })
    }

    fn acquire_pause<'a>(
        &'a self,
        id: &'a SessionId,
        reason: waltr_agent::SessionCoordinationReason,
    ) -> PauseFuture<'a> {
        Box::pin(async move {
            if self.paused.swap(true, Ordering::SeqCst) {
                return Err(waltr_agent::SessionPauseError::Paused {
                    session_id: id.clone(),
                });
            }
            self.pause_reasons.lock().unwrap().push(reason);
            let cursor = self
                .capture
                .lock()
                .unwrap()
                .as_ref()
                .expect("pause test capture")
                .through_cursor;
            Ok(RoutinePause::for_test(id.clone(), cursor))
        })
    }

    fn capture_paused_effective<'a>(&'a self, _pause: &'a RoutinePause) -> PauseCaptureFuture<'a> {
        Box::pin(async move {
            self.pause_captures.fetch_add(1, Ordering::SeqCst);
            self.capture.lock().unwrap().clone().ok_or_else(|| {
                waltr_agent::SessionPauseError::CompletedBoundaryRequired {
                    session_id: SessionId::new("source-session"),
                }
            })
        })
    }

    fn apply_paused_mask<'a>(
        &'a self,
        pause: &'a RoutinePause,
        mutation: waltr_agent::SessionMaskMutation,
    ) -> PauseMaskFuture<'a> {
        Box::pin(async move {
            if self.fail_mask.load(Ordering::SeqCst) {
                return Err(waltr_agent::SessionPauseError::BoundaryChanged {
                    session_id: pause.session_id().clone(),
                    expected: pause.source_cursor(),
                    actual: None,
                });
            }
            self.mask_mutations.lock().unwrap().push(mutation);
            Ok(waltr_agent::SessionMaskMutationOutcome {
                revision: waltr_agent::Revision::new(1),
                cursor: pause.source_cursor().next(),
                removed_mask_ids: Vec::new(),
                added_masks: Vec::new(),
            })
        })
    }

    fn arm_paused_continuation<'a>(
        &'a self,
        pause: &'a RoutinePause,
        cursor: waltr_agent::TimelineCursor,
    ) -> PauseUnitFuture<'a> {
        Box::pin(async move {
            if self.fail_continue.load(Ordering::SeqCst) {
                return Err(waltr_agent::SessionPauseError::ContinuationCursorMismatch {
                    session_id: pause.session_id().clone(),
                    expected: None,
                    actual: cursor,
                });
            }
            self.continuations.lock().unwrap().push(cursor);
            Ok(())
        })
    }

    fn release_pause<'a>(&'a self, _pause: &'a RoutinePause) -> PauseUnitFuture<'a> {
        Box::pin(async move {
            if self.block_release_once.swap(false, Ordering::SeqCst) {
                self.release_entered.notify_waiters();
                std::future::pending::<()>().await;
            }
            self.paused.store(false, Ordering::SeqCst);
            self.pause_releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn load_inference<'a>(&'a self, _id: &'a SessionId) -> HostFuture<'a, InferenceSettings> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            if self.block_load.load(Ordering::SeqCst) {
                self.load_entered.store(true, Ordering::SeqCst);
                self.load_release.notified().await;
            }
            Ok(test_inference())
        })
    }

    fn create_child(&self, request: SessionCreateRequest) -> HostFuture<'_, ()> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.children.lock().unwrap().push(request);
            if self.block_create.load(Ordering::SeqCst) {
                self.create_entered.store(true, Ordering::SeqCst);
                self.create_release.notified().await;
            }
            Ok(())
        })
    }

    fn submit_child(&self, id: SessionId, max_turns: NonZeroU64) -> HostFuture<'_, ChildRunFuture> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.run_started.notify_waiters();
            self.submissions
                .lock()
                .unwrap()
                .push((id.as_str().to_owned(), max_turns));
            self.submitted_runs.fetch_add(1, Ordering::SeqCst);
            self.active_runs
                .lock()
                .unwrap()
                .insert(id.as_str().to_owned());
            if self.block_submit.load(Ordering::SeqCst) {
                self.submit_entered.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            let active_runs = Arc::clone(&self.active_runs);
            let id = id.as_str().to_owned();
            let blocked = self.block_runs.load(Ordering::SeqCst);
            let response = self
                .response
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Ok(Some("generated".to_owned())));
            Ok(Box::pin(async move {
                let response = if blocked {
                    std::future::pending().await
                } else {
                    response
                };
                active_runs.lock().unwrap().remove(&id);
                response
            }) as ChildRunFuture)
        })
    }

    fn create_destination(&self, request: SessionCreateRequest) -> HostFuture<'_, ()> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.block_destination.load(Ordering::SeqCst) {
                self.destination_entered.store(true, Ordering::SeqCst);
                self.destination_release.notified().await;
            }
            assert!(
                !self.panic_destination.load(Ordering::SeqCst),
                "destination create panic"
            );
            if self.fail_destination.load(Ordering::SeqCst) {
                return Err("create failed".to_owned());
            }
            self.destinations.lock().unwrap().push(request);
            Ok(())
        })
    }

    fn start_destination(&self, _id: SessionId) -> HostFuture<'_, ()> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.block_start.load(Ordering::SeqCst) {
                self.start_entered.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            if self.fail_start.load(Ordering::SeqCst) {
                Err("start failed".to_owned())
            } else {
                Ok(())
            }
        })
    }

    fn set_metadata_if_absent<'a>(
        &'a self,
        id: &'a SessionId,
        key: String,
        value: Value,
    ) -> HostFuture<'a, bool> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.writes
                .lock()
                .unwrap()
                .push((id.as_str().to_owned(), key, value));
            Ok(false)
        })
    }

    fn interrupt<'a>(&'a self, id: &'a SessionId) -> HostFuture<'a, ()> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.interrupts.fetch_add(1, Ordering::SeqCst);
            self.active_runs.lock().unwrap().remove(id.as_str());
            Ok(())
        })
    }
}

async fn state() -> AgentDefinitionState {
    let loader = crate::test_support::default_catalog_loader();
    let materializer = crate::catalog::CatalogMaterializer::load(&loader)
        .await
        .unwrap();
    let root = crate::test_support::temp_root();
    let resource_dir = root.join(".waltr/resources/mcp-servers");
    std::fs::create_dir_all(&resource_dir).unwrap();
    std::fs::write(
        resource_dir.join("waltr.yaml"),
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://waltr.example/base\n  path: /mcp\n",
    )
    .unwrap();
    AgentDefinitionState::load_unified(root, materializer).unwrap()
}

async fn compact_state() -> AgentDefinitionState {
    let loader = crate::test_support::default_catalog_loader();
    let materializer = crate::catalog::CatalogMaterializer::load(&loader)
        .await
        .unwrap();
    let root = crate::test_support::temp_root();
    let resources = root.join(".waltr/resources");
    std::fs::create_dir_all(resources.join("mcp-servers")).unwrap();
    std::fs::create_dir_all(resources.join("routines")).unwrap();
    std::fs::write(
        resources.join("mcp-servers/waltr.yaml"),
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: waltr\nspec:\n  transport: streamableHttp\n  url: https://waltr.example/base\n  path: /mcp\n",
    )
    .unwrap();
    std::fs::write(
        resources.join("routines/compact-and-continue.yaml"),
        include_str!("../../../../docs/examples/routines/compact-and-continue.yaml"),
    )
    .unwrap();
    AgentDefinitionState::load_unified(root, materializer).unwrap()
}

fn completion_snapshot(
    provider_key: &str,
    model_id: &str,
    tokens: Option<u64>,
    purpose: Option<&str>,
) -> waltr_agent::SessionRunCompletionSnapshot {
    waltr_agent::SessionRunCompletionSnapshot {
        session_id: SessionId::new("source-session"),
        run_id: "session-run:source-session:r7".to_owned(),
        terminal_cursor: waltr_agent::TimelineCursor::new(9),
        context_tokens: tokens,
        purpose: purpose.map(ToOwned::to_owned),
        provider_key: provider_key.to_owned(),
        model_id: model_id.to_owned(),
    }
}

#[tokio::test]
async fn automatic_completion_pressure_selection_is_strict_and_deterministic() {
    let state = compact_state().await;
    let catalog = state.snapshot();
    let (provider_key, model_id, limit) = catalog
        .configuration()
        .providers
        .iter()
        .find_map(|(provider_key, provider)| {
            provider.models.iter().find_map(|(model_id, model)| {
                model
                    .limits
                    .and_then(|limits| limits.context)
                    .map(|limit| (provider_key.as_str(), model_id.as_str(), u64::from(limit)))
            })
        })
        .expect("test catalog model with context limit");
    let percentage_threshold = u64::try_from((u128::from(limit) * 80).div_ceil(100)).unwrap();
    let below_both = 179_999_u64.min(percentage_threshold.saturating_sub(1));

    assert!(
        engine::select_automatic_invocation(
            &catalog,
            &completion_snapshot(provider_key, model_id, Some(below_both), None),
        )
        .unwrap()
        .is_none()
    );
    let (_, trigger, context) = engine::select_automatic_invocation(
        &catalog,
        &completion_snapshot(provider_key, model_id, Some(percentage_threshold), None),
    )
    .unwrap()
    .expect("80 percent pressure must match");
    assert_eq!(trigger, "context-pressure");
    assert_eq!(context.params["continue"], json!(true));
    assert!(
        engine::select_automatic_invocation(
            &catalog,
            &completion_snapshot("unknown", "unknown", Some(180_000), None),
        )
        .unwrap()
        .is_some(),
        "absolute pressure must work without a known model limit"
    );
    assert!(
        engine::select_automatic_invocation(
            &catalog,
            &completion_snapshot(provider_key, model_id, None, None),
        )
        .unwrap()
        .is_none(),
        "missing provider usage must not match"
    );
    assert!(
        engine::select_automatic_invocation(
            &catalog,
            &completion_snapshot(provider_key, model_id, Some(u64::MAX), Some("routine-task"),),
        )
        .unwrap()
        .is_none(),
        "Routine helper sessions must be excluded"
    );
}

#[tokio::test]
async fn automatic_failed_durable_admission_fails_open_without_history_or_pause() {
    let catalog = compact_state().await;
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    sqlx::query(
        "CREATE TRIGGER fail_automatic_admission BEFORE INSERT ON routine_invocations BEGIN SELECT RAISE(ABORT, 'injected admission failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();
    let host = Arc::new(FakeHost::with_capture("unused"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        catalog,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }

    let mut registry = waltr_agent::AgentDefinitionRegistry::new();
    let source = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: test }\nspec: {}\n";
    registry.load_yaml(source).unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(std::env::temp_dir())
        .await
        .unwrap();
    let runtime_factory = waltr_agent::AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        std::env::temp_dir(),
        waltr_agent::FileCredentialStore::new(
            std::env::temp_dir().join("waltr-automatic-admission-auth.json"),
        ),
        registry.resolve_agent("test").unwrap(),
    )
    .with_runtime_provider(|_| {
        Ok(waltr_agent::AgentRuntimeProvider::new(
            waltr_agent::ModelRef::new(
                waltr_agent::ProviderId::OpenAi,
                "test-model",
                waltr_agent::ModelApi::OpenAiResponses,
            ),
            Arc::new(AutomaticUsageProvider),
        ))
    });
    let sessions =
        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(runtime_factory);
    sessions.set_completion_admission_handler(engine.completion_handler_for_test());
    let mut request = SessionCreateRequest::new(SessionId::new("source-session"));
    request.inference_settings = Some(test_inference());
    request.agent_definition = Some(waltr_agent::AgentDefinitionBundle::new(
        "test",
        vec![source.to_owned()],
    ));
    sessions.create(request).await.unwrap();

    sessions
        .submit_prompt(
            SessionId::new("source-session"),
            "pressure",
            waltr_agent::RunConflictPolicy::QueueIfActive,
        )
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();

    assert!(
        store
            .list(None, 10, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        sessions
            .active_state(&SessionId::new("source-session"))
            .await
            .is_none()
    );
    let pause = sessions
        .acquire_pause_reject_busy(
            &SessionId::new("source-session"),
            waltr_agent::SessionCoordinationReason::new(
                "host-operation",
                Some("test".to_owned()),
                Some("manual".to_owned()),
            )
            .unwrap(),
        )
        .await
        .expect("failed admission must release the coordinator pause");
    sessions.release_pause(&pause).await.unwrap();
    assert_eq!(host.pause_captures.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
    sessions.shutdown().await;
    sandbox.shutdown().await;
}

fn event(id: &str, ephemeral: bool) -> EventEnvelope {
    let metadata = ephemeral
        .then(|| json!({"waltr.session.ephemeral": true}))
        .unwrap_or_else(|| json!({}));
    EventEnvelope {
        id: id.to_owned(),
        r#type: USER_MESSAGE_CREATED.to_owned(),
        source_session_id: Some("source-session".to_owned()),
        data: json!({
            "message": { "text": "Explain the event loop" },
            "userMessageCount": 1,
            "session": {
                "id": "source-session",
                "metadata": metadata,
            }
        }),
    }
}

fn event_context(event: EventEnvelope) -> RoutineInvocationContext {
    RoutineInvocationContext {
        event: Some(event),
        trigger: Value::Null,
        params: BTreeMap::new(),
        source: None,
        source_values: Value::Null,
    }
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("routine action should complete");
}

fn manual_invocation(request_id: &str) -> ManualRoutineInvocation {
    ManualRoutineInvocation {
        request_id: request_id.to_owned(),
        routine_name: "fork".to_owned(),
        trigger_name: "manual".to_owned(),
        session_id: SessionId::new("source-session"),
        inputs: BTreeMap::from([(
            "direction".to_owned(),
            RoutineValue::String("continue".to_owned()),
        )]),
    }
}

fn compact_invocation(request_id: &str, continue_after: Option<bool>) -> ManualRoutineInvocation {
    ManualRoutineInvocation {
        request_id: request_id.to_owned(),
        routine_name: "compact-and-continue".to_owned(),
        trigger_name: "manual".to_owned(),
        session_id: SessionId::new("source-session"),
        inputs: continue_after
            .map(|value| BTreeMap::from([("continue".to_owned(), RoutineValue::Boolean(value))]))
            .unwrap_or_default(),
    }
}

#[tokio::test]
async fn manual_compaction_defaults_false_uses_effective_pause_and_translates_mask() {
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, host.clone());

    let results = engine
        .manual_handle()
        .invoke_for_test(compact_invocation("compact-default", None))
        .await
        .unwrap();

    assert_eq!(host.pause_captures.load(Ordering::SeqCst), 1);
    assert_eq!(host.pause_releases.load(Ordering::SeqCst), 1);
    let reasons = host.pause_reasons.lock().unwrap();
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code(), "context-compaction");
    assert_eq!(reasons[0].source(), Some("compact-and-continue"));
    assert_eq!(reasons[0].trigger(), Some("manual"));
    drop(reasons);
    assert!(host.continuations.lock().unwrap().is_empty());
    let mutations = host.mask_mutations.lock().unwrap();
    assert_eq!(mutations.len(), 1);
    assert_eq!(mutations[0].subtract.overlaps[0].cursor.get(), 4);
    assert_eq!(mutations[0].add[0].selector.cursor.get(), 4);
    assert_eq!(mutations[0].add[0].key, "context.compaction-prefix");
    assert_eq!(mutations[0].add[0].replacement.text, "compact summary");
    assert!(matches!(
        host.children.lock().unwrap()[0].start,
        SessionStart::Copy { .. }
    ));
    assert_eq!(
        results,
        vec![
            RoutineInvocationResult {
                name: "summary".to_owned(),
                value: RoutineValue::String("compact summary".to_owned()),
            },
            RoutineInvocationResult {
                name: "mask-cursor".to_owned(),
                value: RoutineValue::String("5".to_owned()),
            },
        ]
    );
    drop(mutations);
    engine.shutdown().await;
}

#[test]
fn automatic_compaction_claim_uses_selected_routine_reason() {
    let reason = super::engine::routine_pause_reason("compact-and-continue", "run-completed")
        .expect("valid automatic compaction reason");
    assert_eq!(reason.code(), "context-compaction");
    assert_eq!(reason.source(), Some("compact-and-continue"));
    assert_eq!(reason.trigger(), Some("run-completed"));
}

#[tokio::test]
async fn manual_compaction_explicit_true_arms_exact_mask_cursor_before_release() {
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, host.clone());

    engine
        .manual_handle()
        .invoke_for_test(compact_invocation("compact-continue", Some(true)))
        .await
        .unwrap();

    assert_eq!(
        *host.continuations.lock().unwrap(),
        [waltr_agent::TimelineCursor::new(5)]
    );
    assert_eq!(host.pause_releases.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
}

#[tokio::test]
async fn manual_compaction_busy_rejects_before_execution() {
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    host.paused.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, host.clone());

    let error = engine
        .manual_handle()
        .invoke_for_test(compact_invocation("compact-busy", None))
        .await
        .unwrap_err();

    assert_eq!(error.class, "source_precondition");
    assert!(host.children.lock().unwrap().is_empty());
    assert!(host.mask_mutations.lock().unwrap().is_empty());
    assert_eq!(host.pause_releases.load(Ordering::SeqCst), 0);
    host.paused.store(false, Ordering::SeqCst);
    engine.shutdown().await;
}

#[tokio::test]
async fn continuation_failure_releases_pause_without_compensating_mask() {
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    host.fail_continue.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, host.clone());

    let error = engine
        .manual_handle()
        .invoke_for_test(compact_invocation("continue-fails", Some(true)))
        .await
        .unwrap_err();

    assert_eq!(error.class, "continue_action");
    assert_eq!(host.mask_mutations.lock().unwrap().len(), 1);
    assert_eq!(host.pause_releases.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
}

#[tokio::test]
async fn helper_and_mask_failures_release_the_owned_pause() {
    let helper_host = Arc::new(FakeHost::with_capture("unused"));
    *helper_host.response.lock().unwrap() = Some(Err("provider failed".to_owned()));
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, helper_host.clone());
    let error = engine
        .manual_handle()
        .invoke_for_test(compact_invocation("helper-fails", None))
        .await
        .unwrap_err();
    assert_eq!(error.class, "provider");
    assert_eq!(helper_host.pause_releases.load(Ordering::SeqCst), 1);
    assert!(helper_host.mask_mutations.lock().unwrap().is_empty());
    engine.shutdown().await;

    let mask_host = Arc::new(FakeHost::with_capture("compact summary"));
    mask_host.fail_mask.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, mask_host.clone());
    let error = engine
        .manual_handle()
        .invoke_for_test(compact_invocation("mask-fails", None))
        .await
        .unwrap_err();
    assert_eq!(error.class, "mask_action");
    assert_eq!(mask_host.pause_releases.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_tracker_close_wait_observes_last_task_completion() {
    for _ in 0..256 {
        let tracker = DestinationTaskTracker::default();
        let (release, released) = tokio::sync::oneshot::channel();
        let task = tracker.spawn(async move {
            let _ = released.await;
        });
        let waiting = tokio::spawn({
            let tracker = tracker.clone();
            async move { tracker.close_and_wait().await }
        });

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("tracker wait should observe final task completion")
            .unwrap();
        task.await.unwrap();
    }
}

#[test]
fn session_event_adapter_projects_complete_metadata_without_special_fields() {
    let metadata = BTreeMap::from([
        ("opaque".to_owned(), json!({"nested": true})),
        (
            "waltr.session.ephemeral".to_owned(),
            Value::String("true".to_owned()),
        ),
    ]);
    let envelope = session_event_envelope(SessionEvent::UserMessageCreated {
        id: "event-1".to_owned(),
        session_id: SessionId::new("session-1"),
        prompt_index: 0,
        user_message_count: 1,
        text: "prompt".to_owned(),
        metadata: metadata.clone(),
        revision: waltr_agent::Revision::new(1),
        cursor: None,
    });

    assert_eq!(envelope.r#type, USER_MESSAGE_CREATED);
    assert_eq!(envelope.source_session_id.as_deref(), Some("session-1"));
    assert_eq!(envelope.data["session"]["metadata"], json!(metadata));
    assert_eq!(envelope.data["session"].get("ephemeral"), None);
}

#[test]
fn expressions_preserve_whole_scalar_types_and_expand_structured_manual_context() {
    let context = RoutineInvocationContext {
        event: None,
        trigger: json!({"session": {"id": "source", "latestCompletedCursor": 7}}),
        params: BTreeMap::from([
            ("enabled".to_owned(), Value::Bool(true)),
            (
                "direction".to_owned(),
                Value::String("line one\nline two".to_owned()),
            ),
        ]),
        source: None,
        source_values: json!({"session": {"id": "source"}, "snapshot": {"cursor": 7}}),
    };
    assert_eq!(
        substitute_scalar(
            &crate::resource_catalog::routine::RoutineScalar::String(
                "$(params.enabled)".to_owned()
            ),
            &context,
            &BTreeMap::new(),
            None,
            None,
        )
        .unwrap(),
        Value::Bool(true),
    );
    assert_eq!(
        substitute_string(
            "$(trigger.session.id):$(sourceSession.snapshot.cursor)\n$(params.direction)",
            &context,
            &BTreeMap::new(),
            None,
            None,
        )
        .unwrap(),
        "source:7\nline one\nline two",
    );
}

#[tokio::test]
async fn bundled_title_routine_creates_visible_child_and_applies_exact_output_with_cas() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("Event loop explanation"))));
    let bus = EventBus::new(8);
    let catalog = state().await;
    let snapshot = catalog.snapshot();
    let task = &snapshot.routine("title").unwrap().definition().spec.tasks[0];
    agent_bundle(task, "system prompt").unwrap();
    drop(snapshot);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), catalog, host.clone());

    EventSink::send(&bus, event("event-title", false)).unwrap();
    wait_for(|| !host.writes.lock().unwrap().is_empty()).await;

    {
        let children = host.children.lock().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].metadata_values["waltr.session.ephemeral"], true);
        assert_eq!(
            children[0].metadata_values["waltr.session.purpose"],
            "routine-task"
        );
        assert_eq!(
            children[0]
                .inference_settings
                .as_ref()
                .unwrap()
                .provider
                .reasoning_effort
                .as_deref(),
            Some("none")
        );
        let SessionStart::Fresh { messages } = &children[0].start else {
            panic!("fresh child start expected")
        };
        assert_eq!(messages[0].text(), "Explain the event loop");
        drop(children);
    }
    assert_eq!(
        host.writes.lock().unwrap().as_slice(),
        [(
            "source-session".to_owned(),
            "title".to_owned(),
            Value::String("Event loop explanation".to_owned())
        )]
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn atomic_destination_prompt_from_session_coordinator_reaches_bundled_title_routine() {
    let host = Arc::new(FakeHost::with_response(Ok(Some(
        "Atomic destination title",
    ))));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let sessions = SessionCoordinator::new(MemorySessionStore::new())
        .with_session_event_sink(Arc::new(bus.clone()));
    let destination_id = SessionId::new("atomic-title-destination");
    let prompt = "Explain atomic destination publication";
    let mut request = SessionCreateRequest::new(destination_id.clone());
    request.start = SessionStart::Fresh {
        messages: vec![SessionStartMessage::user(prompt).expect("valid initial prompt")],
    };

    let destination = sessions.create(request).await.unwrap();
    assert_eq!(destination.record.snapshot().prompts, [prompt]);
    wait_for(|| !host.writes.lock().unwrap().is_empty()).await;

    {
        let children = host.children.lock().unwrap();
        assert_eq!(children.len(), 1);
        let SessionStart::Fresh { messages } = &children[0].start else {
            panic!("fresh child start expected")
        };
        assert_eq!(messages[0].text(), prompt);
        assert_eq!(
            children[0].metadata_values["waltr.routine.source_session_id"],
            destination_id.as_str()
        );
    }
    assert_eq!(
        host.writes.lock().unwrap().as_slice(),
        [(
            destination_id.as_str().to_owned(),
            "title".to_owned(),
            Value::String("Atomic destination title".to_owned())
        )]
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn duplicate_event_runs_one_in_process_attempt() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_response(Ok(Some("one"))));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    let duplicate = event("same-event", false);

    EventSink::send(&bus, duplicate.clone()).unwrap();
    EventSink::send(&bus, duplicate).unwrap();
    wait_for(|| !host.writes.lock().unwrap().is_empty()).await;

    assert_eq!(host.children.lock().unwrap().len(), 1);
    assert_eq!(host.writes.lock().unwrap().len(), 1);
    assert_eq!(
        store.list(None, 10, &BTreeMap::new()).await.unwrap().len(),
        1
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn provider_failure_and_empty_output_skip_metadata_action() {
    for response in [Err("provider failed"), Ok(None), Ok(Some(""))] {
        let host = Arc::new(FakeHost::with_response(response));
        let bus = EventBus::new(8);
        let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
        EventSink::send(&bus, event("failure-event", false)).unwrap();
        wait_for(|| !host.children.lock().unwrap().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(host.writes.lock().unwrap().is_empty());
        engine.shutdown().await;
    }
}

#[tokio::test]
async fn bundled_condition_rejects_auxiliary_session_event() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());

    EventSink::send(&bus, event("auxiliary", true)).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert!(host.children.lock().unwrap().is_empty());
    engine.shutdown().await;
}

#[tokio::test]
async fn events_before_subscription_are_not_backfilled() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    let bus = EventBus::new(8);
    assert!(EventSink::send(&bus, event("before-start", false)).is_err());
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(host.children.lock().unwrap().is_empty());
    engine.shutdown().await;
}

#[tokio::test]
async fn shutdown_interrupts_owned_child_and_joins_worker() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    host.block_runs.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    EventSink::send(&bus, event("shutdown", false)).unwrap();
    wait_for(|| !host.children.lock().unwrap().is_empty()).await;

    tokio::time::timeout(Duration::from_secs(2), engine.shutdown())
        .await
        .expect("engine shutdown should join");
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
    assert!(host.active_runs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn shutdown_during_child_creation_registers_and_interrupts_without_submitting() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    host.block_create.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    EventSink::send(&bus, event("shutdown-create-race", false)).unwrap();
    wait_for(|| host.create_entered.load(Ordering::SeqCst)).await;

    let shutdown = tokio::spawn(engine.shutdown());
    tokio::task::yield_now().await;
    host.create_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("engine shutdown should join")
        .unwrap();

    assert_eq!(host.children.lock().unwrap().len(), 1);
    assert_eq!(host.submitted_runs.load(Ordering::SeqCst), 0);
    assert!(host.active_runs.lock().unwrap().is_empty());
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_cancels_permanently_pending_submit_and_interrupts_owned_child() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    host.block_submit.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    EventSink::send(&bus, event("shutdown-pending-submit", false)).unwrap();
    wait_for(|| host.submit_entered.load(Ordering::SeqCst)).await;

    tokio::time::timeout(Duration::from_secs(2), engine.shutdown())
        .await
        .expect("shutdown should not wait for pending submit");

    assert_eq!(host.children.lock().unwrap().len(), 1);
    assert_eq!(host.submitted_runs.load(Ordering::SeqCst), 1);
    assert!(host.active_runs.lock().unwrap().is_empty());
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_cancels_permanently_pending_pre_child_host_operation() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    host.block_load.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    EventSink::send(&bus, event("shutdown-pending-load", false)).unwrap();
    wait_for(|| host.load_entered.load(Ordering::SeqCst)).await;

    tokio::time::timeout(Duration::from_secs(2), engine.shutdown())
        .await
        .expect("shutdown should not wait for pending inference load");

    assert!(host.children.lock().unwrap().is_empty());
    assert_eq!(host.submitted_runs.load(Ordering::SeqCst), 0);
    assert!(host.active_runs.lock().unwrap().is_empty());
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn manual_shutdown_waits_for_admitted_pre_child_capture_then_rejects_new_admission() {
    let host = Arc::new(FakeHost::with_capture("unused"));
    host.block_capture.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let manual = engine.manual_handle();
    let invocation = tokio::spawn({
        let manual = manual.clone();
        async move {
            manual
                .invoke_for_test(manual_invocation("capture-race"))
                .await
        }
    });
    wait_for(|| host.capture_entered.load(Ordering::SeqCst)).await;

    let shutdown = tokio::spawn(engine.shutdown());
    host.capture_release.notify_one();
    assert_eq!(invocation.await.unwrap().unwrap_err().class, "shutdown");
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown should wait for admitted capture")
        .unwrap();
    let error = manual
        .invoke_for_test(manual_invocation("after-closure"))
        .await
        .unwrap_err();
    assert_eq!(error.class, "shutdown");
    assert!(host.children.lock().unwrap().is_empty());
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn manual_shutdown_during_child_creation_interrupts_registered_child() {
    let host = Arc::new(FakeHost::with_capture("unused"));
    host.block_create.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move {
            manual
                .invoke_for_test(manual_invocation("create-race"))
                .await
        }
    });
    wait_for(|| host.create_entered.load(Ordering::SeqCst)).await;

    tokio::time::timeout(Duration::from_secs(2), engine.shutdown())
        .await
        .expect("shutdown should cancel manual child creation");
    assert_eq!(invocation.await.unwrap().unwrap_err().class, "shutdown");
    assert_eq!(host.children.lock().unwrap().len(), 1);
    assert_eq!(host.submitted_runs.load(Ordering::SeqCst), 0);
    assert!(host.active_runs.lock().unwrap().is_empty());
    assert!(host.interrupts.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn dropping_manual_caller_during_submit_interrupts_child_without_shutdown() {
    let host = Arc::new(FakeHost::with_capture("unused"));
    host.block_submit.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move {
            manual
                .invoke_for_test(manual_invocation("drop-submit"))
                .await
        }
    });
    wait_for(|| host.submit_entered.load(Ordering::SeqCst)).await;

    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    wait_for(|| host.interrupts.load(Ordering::SeqCst) == 1).await;
    assert!(host.active_runs.lock().unwrap().is_empty());
    engine.shutdown().await;
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_manual_caller_during_run_interrupts_child_without_shutdown() {
    let host = Arc::new(FakeHost::with_capture("unused"));
    host.block_runs.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move { manual.invoke_for_test(manual_invocation("drop-run")).await }
    });
    wait_for(|| host.submitted_runs.load(Ordering::SeqCst) == 1).await;

    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    wait_for(|| host.interrupts.load(Ordering::SeqCst) == 1).await;
    assert!(host.active_runs.lock().unwrap().is_empty());
    engine.shutdown().await;
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn manual_shutdown_drains_child_running_outside_event_worker_set() {
    let host = Arc::new(FakeHost::with_capture("unused"));
    host.block_runs.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move {
            manual
                .invoke_for_test(manual_invocation("shutdown-run"))
                .await
        }
    });
    wait_for(|| host.submitted_runs.load(Ordering::SeqCst) == 1).await;

    tokio::time::timeout(Duration::from_secs(2), engine.shutdown())
        .await
        .expect("shutdown should drain a manual child");
    assert_eq!(invocation.await.unwrap().unwrap_err().class, "shutdown");
    assert!(host.active_runs.lock().unwrap().is_empty());
    assert!(host.interrupts.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn admitted_invocation_pins_routine_and_etag_across_reload() {
    let host = Arc::new(FakeHost::with_response(Ok(Some("pinned"))));
    host.block_load.store(true, Ordering::SeqCst);
    let catalog = state().await;
    let original_etag = catalog
        .snapshot()
        .routine("title")
        .unwrap()
        .source_etag()
        .to_owned();
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), catalog.clone(), host.clone());
    EventSink::send(&bus, event("before-reload", false)).unwrap();
    wait_for(|| host.load_entered.load(Ordering::SeqCst)).await;

    let crate::resource_catalog::AgentDefinitionInspection::Resource(detail) =
        catalog.inspect(Some("routines/title.yaml")).unwrap()
    else {
        panic!("routine detail expected");
    };
    let replacement =
        detail
            .source
            .unwrap()
            .replacen("Generate and apply", "Generate, then apply", 1);
    catalog
        .replace("routines/title.yaml", replacement, &detail.persisted_etag)
        .await
        .unwrap();
    let replacement_etag = catalog
        .snapshot()
        .routine("title")
        .unwrap()
        .source_etag()
        .to_owned();
    assert_ne!(replacement_etag, original_etag);

    host.block_load.store(false, Ordering::SeqCst);
    host.load_release.notify_one();
    wait_for(|| !host.children.lock().unwrap().is_empty()).await;
    assert_eq!(
        host.children.lock().unwrap()[0].metadata_values["waltr.routine.revision"],
        original_etag
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn manual_fork_seeds_durable_helper_history_and_creates_exact_ordinary_destination() {
    let host = Arc::new(FakeHost::with_capture("  compacted context\n"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let results = engine
        .manual_handle()
        .invoke_for_test(ManualRoutineInvocation {
            request_id: "request-1".to_owned(),
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            session_id: SessionId::new("source-session"),
            inputs: BTreeMap::from([(
                "direction".to_owned(),
                RoutineValue::String("branch\nexact".to_owned()),
            )]),
        })
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "session-id");
    let RoutineValue::String(destination_id) = &results[0].value else {
        panic!("string result expected")
    };
    {
        let submissions = host.submissions.lock().unwrap().clone();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].1.get(), 1);
    }
    {
        let children = host.children.lock().unwrap();
        assert_eq!(children.len(), 1);
        let helper = &children[0];
        let SessionStart::Copy { copy, messages } = &helper.start else {
            panic!("copied child start expected")
        };
        assert_eq!(copy.source_id().as_str(), "source-session");
        assert_eq!(copy.boundary(), waltr_agent::TimelineCursor::new(4));
        assert_eq!(
            messages[0].text(),
            "<fork-direction>\nbranch\nexact\n</fork-direction>"
        );
        let snapshot = SessionCoordinator::new(MemorySessionStore::new())
            .create(helper.clone())
            .await
            .unwrap()
            .record
            .snapshot();
        assert_eq!(snapshot.prompt_turns.len(), 2);
        assert_eq!(
            snapshot.prompt_turns[1].prompt,
            "<fork-direction>\nbranch\nexact\n</fork-direction>"
        );
        assert_eq!(snapshot.turns.len(), 1);
        assert_eq!(
            snapshot.turns[0]
                .tool_calls()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            ["call-0"]
        );
    }
    {
        let destinations = host.destinations.lock().unwrap().clone();
        assert_eq!(destinations.len(), 1);
        let request = &destinations[0];
        assert_eq!(request.id.as_str(), destination_id);
        assert_eq!(request.inference_settings.as_ref(), Some(&test_inference()));
        let SessionStart::Fresh { messages } = &request.start else {
            panic!("fresh destination start expected")
        };
        assert_eq!(
            request.agent_definition.as_ref().unwrap().agent_name,
            "source-agent"
        );
        assert!(
            request
                .agent_definition
                .as_ref()
                .unwrap()
                .pending_switch_audit
                .is_none()
        );
        assert_eq!(
            request.metadata_values,
            BTreeMap::from([(
                "waltr.session.fork.source".to_owned(),
                Value::String("source-session".to_owned())
            )])
        );
        assert_eq!(
            messages[0].text(),
            "  compacted context\n\n\nOriginal user message:\nbranch\nexact"
        );
        assert!(!messages[0].text().ends_with('\n'));
    }
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn manual_input_validation_happens_before_capture_and_preserves_start_failure_destination() {
    let default_host = Arc::new(FakeHost::default());
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, default_host);
    let error = engine
        .manual_handle()
        .invoke_for_test(ManualRoutineInvocation {
            request_id: "invalid".to_owned(),
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            session_id: SessionId::new("source-session"),
            inputs: BTreeMap::from([(
                "direction".to_owned(),
                RoutineValue::String(" \n\t".to_owned()),
            )]),
        })
        .await
        .unwrap_err();
    assert_eq!(error.class, "input");
    engine.shutdown().await;

    let host = Arc::new(FakeHost::with_capture("compacted"));
    host.fail_start.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let results = engine
        .manual_handle()
        .invoke_for_test(ManualRoutineInvocation {
            request_id: "start-failure".to_owned(),
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            session_id: SessionId::new("source-session"),
            inputs: BTreeMap::from([(
                "direction".to_owned(),
                RoutineValue::String("continue".to_owned()),
            )]),
        })
        .await
        .unwrap();
    assert_eq!(results[0].name, "session-id");
    assert_eq!(host.destinations.lock().unwrap().len(), 1);
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
}

#[tokio::test]
async fn manual_cancellation_waits_for_atomic_destination_creation_and_returns_retained_id() {
    let host = Arc::new(FakeHost::with_capture("compacted"));
    host.block_destination.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move {
            manual
                .invoke_for_test(manual_invocation("cancel-destination-create"))
                .await
        }
    });
    wait_for(|| host.destination_entered.load(Ordering::SeqCst)).await;

    let shutdown = tokio::spawn(engine.shutdown());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!invocation.is_finished());
    assert!(host.destinations.lock().unwrap().is_empty());

    host.destination_release.notify_one();
    let results = tokio::time::timeout(Duration::from_secs(2), invocation)
        .await
        .expect("invocation should finish after destination creation")
        .unwrap()
        .unwrap();
    let RoutineValue::String(returned_id) = &results[0].value else {
        panic!("destination id result expected")
    };
    let destinations = host.destinations.lock().unwrap();
    assert_eq!(destinations.len(), 1);
    assert_eq!(destinations[0].id.as_str(), returned_id);
    assert!(matches!(
        &destinations[0].start,
        SessionStart::Fresh { messages } if !messages.is_empty()
    ));
    drop(destinations);
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown should join after atomic destination creation")
        .unwrap();
}

#[tokio::test]
async fn dropping_manual_caller_during_destination_creation_completes_without_starting() {
    let host = Arc::new(FakeHost::with_capture("compacted"));
    host.block_destination.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move {
            manual
                .invoke_for_test(manual_invocation("drop-during-create"))
                .await
        }
    });
    wait_for(|| host.destination_entered.load(Ordering::SeqCst)).await;

    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    assert!(host.destinations.lock().unwrap().is_empty());

    let shutdown = tokio::spawn(engine.shutdown());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!shutdown.is_finished());
    assert!(host.destinations.lock().unwrap().is_empty());

    host.destination_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        wait_for(|| host.destinations.lock().unwrap().len() == 1).await;
    })
    .await
    .expect("owned destination creation should finish after caller drop");
    let destinations = host.destinations.lock().unwrap();
    assert!(matches!(
        &destinations[0].start,
        SessionStart::Fresh { messages } if !messages.is_empty()
    ));
    drop(destinations);
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown should return after destination creation")
        .unwrap();
    let host_calls = host.host_calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(host.host_calls.load(Ordering::SeqCst), host_calls);
}

#[tokio::test]
async fn manual_destination_create_failure_returns_error_without_starting() {
    let host = Arc::new(FakeHost::with_capture("compacted"));
    host.fail_destination.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let error = engine
        .manual_handle()
        .invoke_for_test(ManualRoutineInvocation {
            request_id: "create-failure".to_owned(),
            routine_name: "fork".to_owned(),
            trigger_name: "manual".to_owned(),
            session_id: SessionId::new("source-session"),
            inputs: BTreeMap::from([(
                "direction".to_owned(),
                RoutineValue::String("continue".to_owned()),
            )]),
        })
        .await
        .unwrap_err();
    assert_eq!(error.class, "destination_create");
    assert!(host.destinations.lock().unwrap().is_empty());
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    engine.shutdown().await;
}

#[tokio::test]
async fn manual_destination_creation_join_failure_is_precise_and_does_not_start() {
    let host = Arc::new(FakeHost::with_capture("compacted"));
    host.panic_destination.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let error = engine
        .manual_handle()
        .invoke_for_test(manual_invocation("create-task-panic"))
        .await
        .unwrap_err();

    assert_eq!(error.class, "destination_create_join");
    assert_eq!(error.message, "routine destination creation task panicked");
    assert!(host.destinations.lock().unwrap().is_empty());
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    engine.shutdown().await;
}

#[tokio::test]
async fn dropping_manual_caller_after_prompt_durability_retains_ordinary_destination() {
    let host = Arc::new(FakeHost::with_capture("compacted"));
    host.block_start.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host(bus.subscribe(), state().await, host.clone());
    let invocation = tokio::spawn({
        let manual = engine.manual_handle();
        async move {
            manual
                .invoke_for_test(manual_invocation("drop-after-prompt"))
                .await
        }
    });
    wait_for(|| host.start_entered.load(Ordering::SeqCst)).await;
    assert_eq!(host.destinations.lock().unwrap().len(), 1);

    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    engine.shutdown().await;
    assert_eq!(host.destinations.lock().unwrap().len(), 1);
    assert_eq!(host.interrupts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn oversized_event_prompt_and_task_result_fail_before_downstream_side_effects() {
    let prompt_host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), state().await, prompt_host.clone());
    let mut oversized_event = event("oversized-event", false);
    oversized_event.data["message"]["text"] =
        Value::String("x".repeat(MAX_EXPANDED_PROMPT_BYTES + 1));
    EventSink::send(&bus, oversized_event).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(prompt_host.children.lock().unwrap().is_empty());
    engine.shutdown().await;

    let boundary_host = Arc::new(FakeHost::with_response(Ok(Some("boundary"))));
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), state().await, boundary_host.clone());
    let mut boundary_event = event("boundary-event", false);
    boundary_event.data["message"]["text"] = Value::String("x".repeat(MAX_EXPANDED_PROMPT_BYTES));
    EventSink::send(&bus, boundary_event).unwrap();
    wait_for(|| !boundary_host.children.lock().unwrap().is_empty()).await;
    engine.shutdown().await;

    let result_host = Arc::new(FakeHost::default());
    let catalog = state().await;
    let routine = catalog.snapshot().routine("title").unwrap().clone();
    let invocation = AdmittedInvocation {
        routine,
        trigger: "first-user-message".to_owned(),
        context: event_context(event("oversized-result", false)),
        identity: "oversized-result-invocation".to_owned(),
        correlation_id: "oversized-result".to_owned(),
        pause: None,
    };
    let results = BTreeMap::from([(
        "generate-title".to_owned(),
        BTreeMap::from([(
            "title".to_owned(),
            Value::String("x".repeat(MAX_EXPANDED_STRING_BYTES + 1)),
        )]),
    )]);
    let active_children = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    let child_admission = Arc::new(AtomicBool::new(false));
    let destination_tasks = DestinationTaskTracker::default();
    let cancellation = CancellationToken::new();
    let routine_host: Arc<dyn RoutineHost> = result_host.clone();
    let error = execute_task(
        &invocation,
        &invocation.routine.definition().spec.tasks[1],
        &results,
        &routine_host,
        &active_children,
        &child_admission,
        &destination_tasks,
        &cancellation,
    )
    .await
    .unwrap_err();
    assert_eq!(error.class, "expanded_string");
    assert!(result_host.writes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn metadata_serialized_bound_rejects_oversize_and_accepts_boundary_value() {
    let host = Arc::new(FakeHost::default());
    let catalog = state().await;
    let routine = catalog.snapshot().routine("title").unwrap().clone();
    let invocation = AdmittedInvocation {
        routine,
        trigger: "first-user-message".to_owned(),
        context: event_context(event("metadata-bound", false)),
        identity: "metadata-bound-invocation".to_owned(),
        correlation_id: "metadata-bound".to_owned(),
        pause: None,
    };
    let active_children = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    let child_admission = Arc::new(AtomicBool::new(false));
    let destination_tasks = DestinationTaskTracker::default();
    let cancellation = CancellationToken::new();
    let action = &invocation.routine.definition().spec.tasks[1];
    let routine_host: Arc<dyn RoutineHost> = host.clone();

    let oversized = BTreeMap::from([(
        "generate-title".to_owned(),
        BTreeMap::from([(
            "title".to_owned(),
            Value::String("x".repeat(MAX_SERIALIZED_METADATA_VALUE_BYTES - 1)),
        )]),
    )]);
    let error = execute_task(
        &invocation,
        action,
        &oversized,
        &routine_host,
        &active_children,
        &child_admission,
        &destination_tasks,
        &cancellation,
    )
    .await
    .unwrap_err();
    assert_eq!(error.class, "metadata_value_size");
    assert!(host.writes.lock().unwrap().is_empty());

    let boundary = BTreeMap::from([(
        "generate-title".to_owned(),
        BTreeMap::from([(
            "title".to_owned(),
            Value::String("x".repeat(MAX_SERIALIZED_METADATA_VALUE_BYTES - 2)),
        )]),
    )]);
    execute_task(
        &invocation,
        action,
        &boundary,
        &routine_host,
        &active_children,
        &child_admission,
        &destination_tasks,
        &cancellation,
    )
    .await
    .unwrap();
    assert_eq!(host.writes.lock().unwrap().len(), 1);
}

#[test]
fn terminal_cache_never_evicts_running_and_allows_late_duplicate_after_eviction() {
    let mut cache = InvocationCache::default();
    assert!(cache.admit("running"));
    for index in 0..=TERMINAL_CACHE_CAPACITY {
        let identity = format!("terminal-{index}");
        assert!(cache.admit(&identity));
        cache.finish(&identity);
    }
    assert!(!cache.admit("running"));
    assert!(cache.admit("terminal-0"));
}

#[tokio::test]
async fn durable_event_lifecycle_records_ordered_tasks_and_terminal_results() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_response(Ok(Some("Durable title"))));
    let bus = EventBus::new(8);
    let catalog = state().await;
    let crate::resource_catalog::AgentDefinitionInspection::Resource(detail) =
        catalog.inspect(Some("routines/title.yaml")).unwrap()
    else {
        panic!("title detail expected")
    };
    let replacement = detail.source.unwrap().replace(
        "          - name: title\n            type: string\n            value: $(output.text)",
        "          - name: title\n            type: string\n            value: $(output.text)\n          - name: alpha\n            type: string\n            value: $(output.text)",
    );
    catalog
        .replace("routines/title.yaml", replacement, &detail.persisted_etag)
        .await
        .unwrap();
    let engine =
        RoutineEngine::start_with_host_and_store(bus.subscribe(), catalog, host, store.clone());
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }

    EventSink::send(&bus, event("durable-event", false)).unwrap();
    let summary = loop {
        if let Some(summary) = store
            .list(None, 1, &BTreeMap::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            && summary.status
                == crate::persistence::routine_invocation::RoutineInvocationStatus::Succeeded
        {
            break summary;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(summary.correlation_id, "durable-event");
    assert_eq!(
        summary.metadata.get("waltr.routine.source_session_id"),
        Some(&"source-session".to_owned())
    );
    assert_eq!(summary.routine_revision, summary.routine_revision.trim());
    assert_eq!(
        summary
            .declared_tasks
            .iter()
            .map(|task| task.name.as_str())
            .collect::<Vec<_>>(),
        ["generate-title", "apply-title"]
    );
    let events = store
        .events_for_invocation(&summary.invocation_id)
        .await
        .unwrap();
    let task_results = events
        .iter()
        .find(|event| {
            event.kind == crate::persistence::routine_invocation::RoutineEventKind::TaskSucceeded
                && event.task_name.as_deref() == Some("generate-title")
        })
        .unwrap()
        .values
        .as_ref()
        .unwrap();
    assert_eq!(
        task_results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>(),
        ["title", "alpha"]
    );
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        [
            crate::persistence::routine_invocation::RoutineEventKind::InvocationAdmitted,
            crate::persistence::routine_invocation::RoutineEventKind::InvocationStarted,
            crate::persistence::routine_invocation::RoutineEventKind::TaskStarted,
            crate::persistence::routine_invocation::RoutineEventKind::TaskSucceeded,
            crate::persistence::routine_invocation::RoutineEventKind::TaskStarted,
            crate::persistence::routine_invocation::RoutineEventKind::TaskSucceeded,
            crate::persistence::routine_invocation::RoutineEventKind::InvocationSucceeded,
        ]
    );
    let results = events.last().unwrap().values.as_ref().unwrap();
    assert_eq!(
        results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>(),
        ["title", "applied"]
    );
    assert_eq!(
        results[0].value,
        crate::persistence::routine_invocation::StoredRoutineValue::String(
            "Durable title".to_owned()
        )
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn durable_manual_start_returns_after_registration_and_finishes_detached() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_capture("compacted"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host,
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    let invocation_id = engine
        .manual_handle()
        .start(manual_invocation("durable-manual"))
        .await
        .unwrap();
    assert!(invocation_id.starts_with("rinv_"));
    assert!(store.get(&invocation_id).await.unwrap().is_some());

    loop {
        let summary = store.get(&invocation_id).await.unwrap().unwrap();
        if summary.status
            == crate::persistence::routine_invocation::RoutineInvocationStatus::Succeeded
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let events = store.events_for_invocation(&invocation_id).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                crate::persistence::routine_invocation::RoutineEventKind::InvocationSucceeded
                    | crate::persistence::routine_invocation::RoutineEventKind::InvocationFailed
                    | crate::persistence::routine_invocation::RoutineEventKind::InvocationInterrupted
            ))
            .count(),
        1
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn durable_paused_admission_persists_cursor_and_busy_rejection_creates_no_history() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        compact_state().await,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }

    host.paused.store(true, Ordering::SeqCst);
    let error = engine
        .manual_handle()
        .start(compact_invocation("durable-busy", None))
        .await
        .unwrap_err();
    assert_eq!(error.class, "source_precondition");
    assert!(
        store
            .list(None, 10, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );

    host.paused.store(false, Ordering::SeqCst);
    let invocation_id = engine
        .manual_handle()
        .start(compact_invocation("durable-paused", None))
        .await
        .unwrap();
    let summary = store.get(&invocation_id).await.unwrap().unwrap();
    assert_eq!(summary.source_cursor.as_deref(), Some("4"));
    wait_for(|| host.pause_releases.load(Ordering::SeqCst) == 1).await;
    engine.shutdown().await;
}

#[tokio::test]
async fn paused_admission_and_lifecycle_persistence_failures_release_without_unmasking() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    sqlx::query(
        "CREATE TRIGGER fail_paused_admission BEFORE INSERT ON routine_invocations BEGIN SELECT RAISE(ABORT, 'injected admission failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();
    let admission_host = Arc::new(FakeHost::with_capture("compact summary"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        compact_state().await,
        admission_host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    let error = engine
        .manual_handle()
        .start(compact_invocation("admission-fails", None))
        .await
        .unwrap_err();
    assert_eq!(error.class, "routine_persistence");
    assert_eq!(admission_host.pause_releases.load(Ordering::SeqCst), 1);
    assert!(
        store
            .list(None, 10, &BTreeMap::new())
            .await
            .unwrap()
            .is_empty()
    );
    engine.shutdown().await;

    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    sqlx::query(
        "CREATE TRIGGER fail_mask_lifecycle BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind = 'task_succeeded' AND NEW.task_name = 'apply-mask' BEGIN SELECT RAISE(ABORT, 'injected mask lifecycle failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();
    let lifecycle_host = Arc::new(FakeHost::with_capture("compact summary"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        compact_state().await,
        lifecycle_host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    let invocation_id = engine
        .manual_handle()
        .start(compact_invocation("lifecycle-fails", None))
        .await
        .unwrap();
    wait_for(|| lifecycle_host.pause_releases.load(Ordering::SeqCst) == 1).await;
    assert_eq!(lifecycle_host.mask_mutations.lock().unwrap().len(), 1);
    let summary = store.get(&invocation_id).await.unwrap().unwrap();
    assert_eq!(
        summary.status,
        crate::persistence::routine_invocation::RoutineInvocationStatus::Interrupted
    );
    assert_eq!(summary.terminal_error.unwrap().code, "persistence_failure");
    engine.shutdown().await;
}

#[tokio::test]
async fn canceled_paused_invocation_uses_drop_fallback_to_release() {
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    host.block_runs.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, host.clone());
    let manual = engine.manual_handle();
    let invocation = tokio::spawn(async move {
        manual
            .invoke_for_test(compact_invocation("cancel-paused", None))
            .await
    });
    host.run_started.notified().await;

    invocation.abort();
    assert!(invocation.await.unwrap_err().is_cancelled());
    wait_for(|| host.pause_releases.load(Ordering::SeqCst) == 1).await;
    assert!(!host.paused.load(Ordering::SeqCst));
    engine.shutdown().await;
}

#[tokio::test]
async fn canceled_explicit_release_retains_ownership_for_drop_fallback_retry() {
    let host = Arc::new(FakeHost::default());
    host.paused.store(true, Ordering::SeqCst);
    host.block_release_once.store(true, Ordering::SeqCst);
    let entered = host.release_entered.notified();
    let routine_host: Arc<dyn RoutineHost> = host.clone();
    let mut guard = RoutinePauseGuard::new(
        RoutinePause::for_test(
            SessionId::new("source-session"),
            waltr_agent::TimelineCursor::new(4),
        ),
        routine_host,
    );
    let release = tokio::spawn(async move { guard.release().await });
    entered.await;

    release.abort();
    assert!(release.await.unwrap_err().is_cancelled());
    wait_for(|| host.pause_releases.load(Ordering::SeqCst) == 1).await;
    assert!(!host.paused.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shutdown_releases_pause_before_manual_worker_completion() {
    let host = Arc::new(FakeHost::with_capture("compact summary"));
    host.block_runs.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine =
        RoutineEngine::start_with_host(bus.subscribe(), compact_state().await, host.clone());
    let manual = engine.manual_handle();
    let invocation = tokio::spawn(async move {
        manual
            .invoke_for_test(compact_invocation("shutdown-paused", None))
            .await
    });
    host.run_started.notified().await;

    engine.shutdown().await;
    assert_eq!(invocation.await.unwrap().unwrap_err().class, "shutdown");
    assert_eq!(host.pause_releases.load(Ordering::SeqCst), 1);
    assert!(!host.paused.load(Ordering::SeqCst));
}

#[tokio::test]
async fn engine_reconciles_restart_history_before_becoming_ready() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    store
        .admit(crate::persistence::routine_invocation::RoutineAdmission {
            invocation_id: "left-open".to_owned(),
            execution_id: "execution-left-open".to_owned(),
            routine_name: "title".to_owned(),
            routine_revision: "revision".to_owned(),
            trigger_name: "first-user-message".to_owned(),
            trigger_kind: crate::persistence::routine_invocation::RoutineTriggerKind::Event,
            correlation_id: "event-left-open".to_owned(),
            metadata: BTreeMap::new(),
            source_cursor: None,
            declared_tasks: vec![
                crate::persistence::routine_invocation::RoutineTaskDescriptor {
                    name: "generate-title".to_owned(),
                    task_type: "session_template".to_owned(),
                },
            ],
            admitted_at_ms: 1,
        })
        .await
        .unwrap();
    let host = Arc::new(FakeHost::default());
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host,
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    let reconciled = store.get("left-open").await.unwrap().unwrap();
    assert_eq!(
        reconciled.status,
        crate::persistence::routine_invocation::RoutineInvocationStatus::Interrupted
    );
    assert_eq!(reconciled.terminal_error.unwrap().code, "server_restart");
    engine.shutdown().await;
}

#[tokio::test]
async fn graceful_shutdown_durably_interrupts_running_invocation() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_response(Ok(Some("unused"))));
    host.block_runs.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host,
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    EventSink::send(&bus, event("shutdown-history", false)).unwrap();
    let invocation_id = loop {
        if let Some(summary) = store
            .list(None, 1, &BTreeMap::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            && summary.status
                == crate::persistence::routine_invocation::RoutineInvocationStatus::Running
        {
            break summary.invocation_id;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    engine.shutdown().await;
    let summary = store.get(&invocation_id).await.unwrap().unwrap();
    assert_eq!(
        summary.status,
        crate::persistence::routine_invocation::RoutineInvocationStatus::Interrupted
    );
    assert_eq!(summary.terminal_error.unwrap().code, "server_shutdown");
}

#[tokio::test]
async fn event_recorder_failure_after_side_effect_recovers_with_durable_interruption() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_response(Ok(Some("completed side effect"))));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    sqlx::query(
        "CREATE TRIGGER fail_routine_task_success BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind = 'task_succeeded' BEGIN SELECT RAISE(ABORT, 'injected task success failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();

    EventSink::send(&bus, event("recover-persistence", false)).unwrap();
    let summary = loop {
        if let Some(summary) = store
            .list(None, 1, &BTreeMap::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            && summary.status
                == crate::persistence::routine_invocation::RoutineInvocationStatus::Interrupted
        {
            break summary;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(host.children.lock().unwrap().len(), 1);
    assert_eq!(summary.terminal_error.unwrap().code, "persistence_failure");
    let events = store
        .events_for_invocation(&summary.invocation_id)
        .await
        .unwrap();
    assert!(!events.iter().any(|event| {
        event.kind == crate::persistence::routine_invocation::RoutineEventKind::InvocationSucceeded
    }));
    assert!(matches!(&*engine.health().borrow(), EngineHealth::Running));
    engine.shutdown().await;
}

#[tokio::test]
async fn manual_persistent_recorder_failure_fails_engine_and_blocks_admission() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_capture("completed side effect"));
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    sqlx::query(
        "CREATE TRIGGER fail_routine_terminal_recovery BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind IN ('task_succeeded', 'task_interrupted', 'invocation_interrupted') BEGIN SELECT RAISE(ABORT, 'injected persistent recorder failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();

    let invocation_id = engine
        .manual_handle()
        .start(manual_invocation("persistent-recorder-failure"))
        .await
        .unwrap();
    loop {
        if matches!(&*engine.health().borrow(), EngineHealth::Failed(_)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(host.children.lock().unwrap().len(), 1);
    let summary = store.get(&invocation_id).await.unwrap().unwrap();
    assert_eq!(
        summary.status,
        crate::persistence::routine_invocation::RoutineInvocationStatus::Running
    );
    let events = store.events_for_invocation(&invocation_id).await.unwrap();
    assert!(!events.iter().any(|event| {
        matches!(
            event.kind,
            crate::persistence::routine_invocation::RoutineEventKind::InvocationSucceeded
                | crate::persistence::routine_invocation::RoutineEventKind::InvocationFailed
                | crate::persistence::routine_invocation::RoutineEventKind::InvocationInterrupted
        )
    }));
    let error = engine
        .manual_handle()
        .start(manual_invocation("blocked-after-failure"))
        .await
        .unwrap_err();
    assert!(matches!(error.class, "reconciling" | "shutdown"));
    engine.shutdown().await;
}

#[tokio::test]
async fn fail_closed_waits_for_manual_admission_commit_and_worker_ownership() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_capture("completed side effect"));
    host.block_capture.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    sqlx::query(
        "CREATE TRIGGER fail_manual_race_recovery BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind IN ('task_succeeded', 'task_interrupted', 'invocation_interrupted') AND EXISTS (SELECT 1 FROM routine_invocations WHERE invocation_id = NEW.invocation_id AND correlation_id = 'manual-race-fail') BEGIN SELECT RAISE(ABORT, 'injected scoped failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();

    let manual = engine.manual_handle();
    let racing_start = tokio::spawn({
        let manual = manual.clone();
        async move { manual.start(manual_invocation("manual-race")).await }
    });
    wait_for(|| host.capture_entered.load(Ordering::SeqCst)).await;
    EventSink::send(&bus, event("manual-race-fail", false)).unwrap();
    wait_for(|| manual.fail_closed_pending_for_test()).await;

    sqlx::query("DROP TRIGGER fail_manual_race_recovery")
        .execute(persistence.sqlite_pool().unwrap())
        .await
        .unwrap();
    host.block_runs.store(true, Ordering::SeqCst);
    host.capture_release.notify_one();
    let racing_id = racing_start.await.unwrap().unwrap();
    wait_for(|| matches!(&*engine.health().borrow(), EngineHealth::Failed(_))).await;
    let error = manual
        .start(manual_invocation("manual-after-fail-close"))
        .await
        .unwrap_err();
    assert!(matches!(error.class, "reconciling" | "shutdown"));

    engine.shutdown().await;
    let racing = store.get(&racing_id).await.unwrap().unwrap();
    assert_eq!(
        racing.status,
        crate::persistence::routine_invocation::RoutineInvocationStatus::Interrupted
    );
    assert!(store.nonterminal(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn fail_closed_serializes_with_waiting_event_admission() {
    let persistence = crate::persistence::Persistence::initialize(
        crate::persistence::DatabaseConfig::for_tests().unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.routine_invocation_store();
    let host = Arc::new(FakeHost::with_response(Ok(Some("completed side effect"))));
    host.block_load.store(true, Ordering::SeqCst);
    let bus = EventBus::new(8);
    let engine = RoutineEngine::start_with_host_and_store(
        bus.subscribe(),
        state().await,
        host.clone(),
        store.clone(),
    );
    while !matches!(&*engine.health().borrow(), EngineHealth::Running) {
        tokio::task::yield_now().await;
    }
    sqlx::query(
        "CREATE TRIGGER fail_event_race_recovery BEFORE INSERT ON routine_invocation_events WHEN NEW.event_kind IN ('task_succeeded', 'task_interrupted', 'invocation_interrupted') AND EXISTS (SELECT 1 FROM routine_invocations WHERE invocation_id = NEW.invocation_id AND correlation_id = 'event-race-fail') BEGIN SELECT RAISE(ABORT, 'injected scoped failure'); END",
    )
    .execute(persistence.sqlite_pool().unwrap())
    .await
    .unwrap();

    EventSink::send(&bus, event("event-race-fail", false)).unwrap();
    wait_for(|| host.load_entered.load(Ordering::SeqCst)).await;
    let manual = engine.manual_handle();
    let gate = manual.lifecycle_gate_for_test();
    let gate_guard = gate.write().await;
    EventSink::send(&bus, event("event-race-admission", false)).unwrap();
    wait_for(|| manual.event_admission_waiters_for_test() == 1).await;
    host.load_release.notify_one();
    wait_for(|| manual.fail_closed_pending_for_test()).await;

    sqlx::query("DROP TRIGGER fail_event_race_recovery")
        .execute(persistence.sqlite_pool().unwrap())
        .await
        .unwrap();
    host.block_runs.store(true, Ordering::SeqCst);
    drop(gate_guard);
    wait_for(|| matches!(&*engine.health().borrow(), EngineHealth::Failed(_))).await;
    let error = manual
        .start(manual_invocation("manual-after-event-fail-close"))
        .await
        .unwrap_err();
    assert!(matches!(error.class, "reconciling" | "shutdown"));

    engine.shutdown().await;
    let summaries = store.list(None, 10, &BTreeMap::new()).await.unwrap();
    assert!(
        summaries
            .iter()
            .any(|summary| summary.correlation_id == "event-race-admission")
    );
    assert!(store.nonterminal(10).await.unwrap().is_empty());
}
