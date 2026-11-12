use super::{authorized_tools::*, helper::*};
use crate::{
    agent_host::{
        AgentDefinitionBundle, AgentDefinitionCatalog, Capability, HostRuntimeBuilder,
        authorized_tools::*,
        runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory,
    },
    auth::FileCredentialStore,
    interaction::InteractionBroker,
    permission::ServerPermissionPolicy,
};
use crate::{
    persistence::{DatabaseConfig, Persistence},
    routine::{
        definition::Definition,
        history::{Caller, OperationStatus, Origin, Ownership, Status},
        ports::*,
        service::InvocationService,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use waltr_agent as agent;
use waltr_agent::*;
use waltr_script::{Engine, ScriptEngine};

#[path = "../../../../waltr-agent/tests/support/barrier_store.rs"]
mod barrier_store;

#[allow(dead_code)]
#[path = "../../../../waltr-agent/tests/support/scripted_provider.rs"]
mod scripted_provider;
use scripted_provider::ScriptedProvider;

#[path = "configuration_tests.rs"]
mod configuration_tests;
#[path = "context_tests.rs"]
mod context_tests;
#[path = "tool_streams_integration_tests.rs"]
mod tool_streams_integration_tests;

fn inference() -> InferenceSettings {
    InferenceSettings {
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
    }
}
fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses)
}
fn caller() -> Caller {
    Caller {
        principal: "owner".into(),
        origin: Origin::Manual,
        ownership: Ownership::Detached,
        correlation: "test".into(),
    }
}

struct BindOnce(Mutex<Option<AdapterBindings>>);

struct SourceBindings {
    bindings: BindOnce,
    source: SessionCopy,
}
struct SourceContext(SessionCopy);
impl crate::routine::context::ContextPort for SourceContext {
    fn check(&self, _: &crate::routine::context::ContextOperation) -> std::result::Result<(), PortError> { Err(PortError::Denied) }
    fn resolve_source(&self, source: &str) -> std::result::Result<SessionCopy, PortError> {
        if source == "effective" { Ok(self.0.clone()) } else { Err(PortError::Denied) }
    }
    fn execute(&self, _: crate::routine::context::ContextOperation, _: tokio_util::sync::CancellationToken) -> PortFuture<Value> {
        Box::pin(async { Err(PortError::Denied) })
    }
    fn cleanup(&self) -> PortFuture<()> { Box::pin(async { Ok(()) }) }
}
impl InvocationAdapterFactory for SourceBindings {
    fn bind_context(&self, _: Arc<Definition>, _: SelectedEntrypoint, _: Caller) -> PortFuture<Option<Arc<dyn crate::routine::context::ContextPort>>> {
        let context = Arc::new(SourceContext(self.source.clone())) as Arc<dyn crate::routine::context::ContextPort>;
        Box::pin(async move { Ok(Some(context)) })
    }
    fn bind(&self, definition: Arc<Definition>, selected: SelectedEntrypoint, caller: Caller) -> PortFuture<AdapterBindings> {
        self.bindings.bind(definition, selected, caller)
    }
}

struct MutateAfterAdmission {
    inner: RoutineAgentHostFactory,
    catalog: Arc<std::sync::RwLock<AgentDefinitionCatalog>>,
    replacement: AgentDefinitionCatalog,
}
impl HostFactory for MutateAfterAdmission {
    fn preflight(
        &self,
        binding: crate::routine::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost> {
        let admission = self.inner.preflight(binding, definition, selected, caller);
        let catalog = self.catalog.clone();
        let replacement = self.replacement.clone();
        Box::pin(async move {
            let host = admission.await?;
            *catalog.write().unwrap() = replacement;
            Ok(host)
        })
    }
}
impl InvocationAdapterFactory for BindOnce {
    fn bind(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<AdapterBindings> {
        let bindings = self.0.lock().unwrap().take().ok_or(PortError::Unavailable);
        Box::pin(async move { bindings })
    }
}

fn definition(engine: &Engine, source: &str, capabilities: &BTreeSet<String>) -> Arc<Definition> {
    let captures = if capabilities.contains("helper.capture.verified") {
        let selection = if source.contains("kind:'named',name:'named'") {
            json!({"kind":"named","name":"named"})
        } else {
            json!({"kind":"restricted","profile":"restricted"})
        };
        json!({"verified":{"selection":selection,"contract":{"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"verified":{"type":"boolean"}},"required":["verified"]}}}})
    } else { json!({}) };
    Definition::parse(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"test","description":"test",
        "spec":{"source":source,"entrypoints":{"run":{"trigger":"manual"}},
        "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
        "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":128}},
        "captures":captures,"capabilities":capabilities}}).to_string(), engine).unwrap()
}

#[tokio::test]
async fn engine_cleanup_timeout_does_not_stop_blocked_agent_admission_driver() {
    let agent_store = barrier_store::BarrierStore::default();
    let sessions = SessionCoordinator::new(agent_store.clone());
    let mut create = SessionCreateRequest::new(SessionId::generate());
    create.inference_settings = Some(inference());
    let id = sessions.create(create).await.unwrap().id().clone();
    let target = ConcreteToolTarget::new("server", "effect").unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let (runtime, dispatcher) = HostRuntimeBuilder::new(
        model(),
        Arc::new(ScriptedProvider::new("unused", |_| {
            panic!("concrete execution cannot call provider")
        })),
    )
    .with_tool(
        Capability::new_async(
            ToolDefinition::new("alias", "effect", json!({"type":"object"})),
            {
                let effects = effects.clone();
                move |_| {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(ToolResult::new("effect", "effect", "effect")) })
                }
            },
        )
        .with_concrete_target(target.clone()),
    )
    .with_dispatch_context(
        Some(id.clone()),
        ServerPermissionPolicy::sessions(InteractionBroker::new()),
    )
    .build_with_dispatcher();
    let tools =
        SessionAuthorizedToolExecutor::new(sessions.clone(), id.clone(), runtime, dispatcher);
    let capabilities = BTreeSet::from(["tools.execute".into(), "tool.effect".into()]);
    let bindings = AdapterBindings {
        principal: "owner".into(),
        helpers: Arc::new(SubstitutePorts::default()),
        tools: Arc::new(tools),
        capabilities: capabilities.clone(),
        targets: BTreeMap::from([("tool.effect".into(), target)]),
    };
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let database = root.join(format!("s3-driver-{}.sqlite", uuid::Uuid::new_v4()));
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
    let engine = Arc::new(Engine::new(1).unwrap());
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        Arc::new(RoutineAgentHostFactory {
            bindings: Arc::new(BindOnce(Mutex::new(Some(bindings)))),
        }),
        Arc::new(SystemClock),
        1,
    )
    .unwrap();
    let definition = Definition::parse(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"timeout","description":"test",
        "spec":{"source":"export default async function() { await host.call('tools.execute', {target:{server:'server',tool:'effect'},callId:'effect',arguments:{}}); return 'wrong'; }",
        "entrypoints":{"run":{"trigger":"manual"}},"parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
        "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":128}},
        "budgets":{"valueBytes":65536,"resultBytes":65536,"hostCalls":256,"inFlight":16,"deadlineMs":500},"capabilities":capabilities}}).to_string(), &*engine).unwrap();
    agent_store.arm();
    let handle = service
        .invoke(definition, "run".into(), caller(), Value::Null)
        .unwrap()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), agent_store.entered.notified())
        .await
        .unwrap();
    assert!(sessions.active_state(&id).await.is_some());
    let outcome = tokio::time::timeout(Duration::from_secs(4), handle.join())
        .await
        .unwrap();
    assert_eq!(outcome.status, Some(Status::Uncertain));
    assert!(!outcome.cleanup_complete);
    assert!(outcome.result.is_none());
    let operations = store.operations(handle.id.clone(), 0, 32).await.unwrap();
    assert_eq!(operations.len(), 1);
    assert_ne!(operations[0].status, OperationStatus::Completed);
    let shutdown = tokio::spawn({
        let sessions = sessions.clone();
        async move { sessions.shutdown().await }
    });
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    agent_store.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .operations(handle.id.clone(), 0, 32)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(!outcome.cleanup_complete);
    assert!(matches!(
        service.shutdown().await,
        Err(PortError::Uncertain)
    ));
    assert!(engine.shutdown().await.is_err());
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(database).unwrap();
}

#[derive(Default, Clone)]
struct SubstitutePorts {
    helpers: Arc<AtomicUsize>,
    tools: Arc<AtomicUsize>,
    cleanup: Arc<AtomicUsize>,
}
impl HelperRunner for SubstitutePorts {
    fn run_with_inputs(&self, request: HelperRequest, source: Option<SessionCopy>, payload: Option<crate::routine::ports::HelperPayload>, cancellation: tokio_util::sync::CancellationToken) -> PortFuture<HelperResult> {
        assert!(source.is_none());
        let payload = payload.expect("ordinary helper carries explicit output budgets");
        assert!(payload.attachments.is_empty());
        assert_eq!(payload.response_bytes, 65536);
        assert_eq!(payload.completion_bytes, 65536);
        self.run(request, cancellation)
    }
    fn run(
        &self,
        request: HelperRequest,
        _: tokio_util::sync::CancellationToken,
    ) -> PortFuture<HelperResult> {
        self.helpers.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.task, "task");
        Box::pin(async {
            Ok(HelperResult {
                session_id: "fake-session".into(),
                definition_identity: "fake-pinned".into(),
                response: "accepted".into(),
                captures: vec![],
                evidence: vec![],
            })
        })
    }
    fn cleanup(&self) -> PortFuture<()> {
        self.cleanup.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
impl AuthorizedToolExecutor for SubstitutePorts {
    fn execute(
        &self,
        _: AuthorizedToolRequest,
        _: tokio_util::sync::CancellationToken,
    ) -> AuthorizedToolFuture<ToolResult> {
        self.tools.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(AuthorizedToolError::Denied) })
    }
    fn schemas(
        &self,
        _: Vec<ConcreteToolTarget>,
        _: tokio_util::sync::CancellationToken,
    ) -> AuthorizedToolFuture<Vec<ToolDefinition>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn cleanup(&self) -> AuthorizedToolFuture<()> {
        self.cleanup.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn independent_ports_keep_exact_capabilities_and_trusted_ask_disposition() {
    use waltr_script::{HostContext, HostReply, HostRequest, Limits, StructuredValue};
    let engine = Engine::new(1).unwrap();
    let ports = SubstitutePorts::default();
    let caps = BTreeSet::from_iter(
        [
            "helper.run",
            "helper.restricted.test",
            "tools.execute",
            "tool.exact",
        ]
        .map(str::to_owned),
    );
    let target = ConcreteToolTarget::new("server", "remote.tool").unwrap();
    let definition = definition(
        &engine,
        "export default async function() { return 'ok'; }",
        &caps,
    );
    let selected = SelectedEntrypoint {
        name: "run".into(),
        descriptor: definition.envelope().spec.entrypoints["run"].clone(),
    };
    let factory = RoutineAgentHostFactory {
        bindings: Arc::new(BindOnce(Mutex::new(Some(AdapterBindings {
            principal: "owner".into(),
            helpers: Arc::new(ports.clone()),
            tools: Arc::new(ports.clone()),
            capabilities: caps,
            targets: BTreeMap::from([("tool.exact".into(), target)]),
        })))),
    };
    let prepared = factory
        .preflight(
            crate::routine::history::InvocationBinding::new("test".into(), "owner".into()),
            definition,
            selected,
            caller(),
        )
        .await
        .unwrap();
    let context = HostContext::new(Limits::default()).unwrap();
    for (operation, arguments, expected) in [
        (
            "helper.run",
            json!({"selection":{"kind":"restricted","profile":"test"},"task":"task","maxTurns":1,"deadlineMs":1000,"responseBytes":65536,"completionBytes":65536}),
            "accepted",
        ),
        (
            "tools.execute",
            json!({"target":{"server":"server","tool":"remote.tool"},"callId":"call","arguments":{}}),
            "denied",
        ),
    ] {
        let request = HostRequest {
            operation: operation.into(),
            arguments: StructuredValue::new(arguments, &Limits::default()).unwrap(),
            bytes: None,
            callback: None,
            handle: None,
        };
        prepared.policy.before(&request, &context).await.unwrap();
        let reply = prepared
            .dispatcher
            .dispatch(request, context.clone())
            .await
            .unwrap();
        assert_eq!(
            prepared.policy.project_result(operation, &reply).unwrap(),
            Some(json!({"status":expected}))
        );
        let HostReply::Value(value) = reply else {
            panic!("structured reply")
        };
        assert_eq!(value.as_json()["status"], expected);
    }
    assert_eq!(ports.helpers.load(Ordering::SeqCst), 1);
    assert_eq!(ports.tools.load(Ordering::SeqCst), 1);
    prepared
        .dispatcher
        .cleanup(context.invocation())
        .await
        .unwrap();
    prepared.reservation.finalize(false).await.unwrap();
    assert_eq!(ports.cleanup.load(Ordering::SeqCst), 2);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_js_service_agent_callback_rejects_then_accepts_with_nested_authorized_effect() {
    vertical(false, 0, false).await;
}

#[tokio::test]
async fn named_helper_pins_catalog_definition_through_same_helper_validation() {
    vertical(true, 0, false).await;
}

#[tokio::test]
async fn named_admission_pins_digest_prompt_capture_tool_and_policy_across_replacement_and_deletion()
 {
    vertical(true, 1, false).await;
    vertical(true, 2, false).await;
}

#[tokio::test]
async fn invalid_capture_is_corrected_before_semantic_rejection() {
    vertical(false, 0, true).await;
}

async fn private_source_copy() -> SessionCopy {
    use futures_util::StreamExt;
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let runtime = AgentRuntime::builder(
        model(),
        Arc::new(ScriptedProvider::from_turns(
            "private-source",
            vec![
                vec![CompletionEvent::Delta("MASKED_SECRET".into())],
                vec![
                    CompletionEvent::ThinkingBlock(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: "PRIVATE_REASONING".into(),
                        payload: json!({"secret":"PRIVATE_PAYLOAD"}),
                    }),
                    CompletionEvent::Delta("kept source response".into()),
                ],
                vec![CompletionEvent::Delta("ROLLED_BACK_SECRET".into())],
            ],
        )),
    )
    .build();
    let mut boundaries = Vec::new();
    let mut ends = Vec::new();
    for prompt in [
        "MASKED_SECRET",
        "public source context",
        "ROLLED_BACK_SECRET",
    ] {
        let events = sessions
            .prompt(&runtime, id.clone(), prompt)
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
        let record = sessions.store().load(&id).await.unwrap();
        ends.push(record.last_timeline_cursor().unwrap());
        let mut cursor = None;
        let mut boundary = None;
        for event in &record.journal {
            match event {
                SessionJournalEvent::Timeline(event) => cursor = Some(event.cursor),
                SessionJournalEvent::TurnFinished { .. } => boundary = cursor,
                _ => {}
            }
        }
        boundaries.push(boundary.unwrap());
    }
    sessions
        .rollback_to_cursor(id.clone(), ends[1])
        .await
        .unwrap();
    let mut mutation = SessionMaskMutation::new(Some(ends[1]));
    mutation.add.push(SessionMaskAddition::new(
        "privacy",
        ThroughCursor::new(boundaries[0]),
        ReplacementUserContent::new("safe summary"),
    ));
    sessions.store().mutate_masks(&id, mutation).await.unwrap();
    let record = sessions.store().load(&id).await.unwrap();
    assert!(
        serde_json::to_string(&record)
            .unwrap()
            .contains("ROLLED_BACK_SECRET")
    );
    SessionCopy::from_record_effective(&record, boundaries[1]).unwrap()
}

async fn vertical(named: bool, mutation: u8, invalid_capture: bool) {
    let turns = Arc::new(AtomicUsize::new(0));
    let count = turns.clone();
    let provider = Arc::new(ScriptedProvider::new("helper", move |request| {
        let replay = format!("{request:?}");
        assert!(replay.contains("safe summary") && replay.contains("kept source response"));
        for private in [
            "MASKED_SECRET",
            "PRIVATE_REASONING",
            "PRIVATE_PAYLOAD",
            "ROLLED_BACK_SECRET",
        ] {
            assert!(!replay.contains(private));
        }
        let actual_turn = count.fetch_add(1, Ordering::SeqCst);
        if named {
            assert!(request.system_prompt.as_ref().unwrap().contains("ORIGINAL"));
        }
        if invalid_capture && actual_turn == 0 {
            return vec![
                Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "invalid-capture",
                    "_helper_capture",
                    json!({"verified":"invalid"}),
                ))),
                Ok(CompletionEvent::Finished),
            ];
        }
        if invalid_capture && actual_turn == 1 {
            return vec![
                Ok(CompletionEvent::Delta("missing capture".into())),
                Ok(CompletionEvent::Finished),
            ];
        }
        let turn = actual_turn - if invalid_capture { 2 } else { 0 };
        if invalid_capture && turn == 0 {
            assert!(request.prompt.contains("capture"));
        }
        let events = match turn {
            0 => vec![CompletionEvent::ToolCall(ToolCall::new(
                "capture",
                "_helper_capture",
                json!({"verified":true}),
            ))],
            1 => vec![CompletionEvent::Delta("candidate".into())],
            2 => {
                assert!(request.prompt.contains("verify again"));
                vec![CompletionEvent::Delta("accepted".into())]
            }
            _ => panic!("unexpected helper turn"),
        };
        events
            .into_iter()
            .chain([CompletionEvent::Finished])
            .map(Ok)
            .collect()
    }));
    let runtime = crate::agent_host::HostRuntimeBuilder::new(model(), provider.clone())
        .with_permission_policy(crate::permission::PermissionPolicy::default().with_tool_rule(
            crate::permission::PermissionToolSelector::exact("_helper_capture"),
            crate::permission::PermissionEffect::allow(),
        ));
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let source = "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: named }\nspec:\n  systemPrompt: ORIGINAL\n  permissions:\n    - permissionSetRef: { name: capture }\n---\napiVersion: waltr.dev/v1alpha1\nkind: PermissionSet\nmetadata: { name: capture }\nspec:\n  tools:\n    - name: _helper_capture\n      permission: allow\n";
    let catalog = AgentDefinitionCatalog::from_sources(vec![source.into()], "named").unwrap();
    let expected_identity = catalog.bundle("named").unwrap().identity();
    let current = Arc::new(std::sync::RwLock::new(catalog.clone()));
    let live = current.clone();
    let named_provider = provider.clone();
    let factory = Arc::new(
        AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("unused-s3-auth.json")),
            catalog.resolved("named").unwrap().clone(),
        )
        .with_agent_catalog_provider(move || live.read().unwrap().clone())
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(model(), named_provider.clone()))
        })
        .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new())),
    );
    let agent_store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(agent_store.clone());
    let sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions));
    let capture: crate::routine::definition::Contract = serde_json::from_value(json!({"schemaVersion":"waltr.value/v1","schema":{"type":"object","properties":{"verified":{"type":"boolean"}},"required":["verified"]}})).unwrap();
    let source_copy = private_source_copy().await;
    let helpers = CoordinatorHelperRunner::new(
        sessions.clone(),
        factory,
        inference(),
        BTreeMap::from([(
            "restricted".into(),
            RestrictedHelperProfile {
                runtime,
                inference: inference(),
                prompt: "task prefix".into(),
                max_turns: if invalid_capture { 5 } else { 3 },
            },
        )]),
        BTreeMap::from([("verified".into(), capture)]),
        None,
        "owner".into(),
    )
    .unwrap();
    let target = ConcreteToolTarget::new("exact.server", "effect-name").unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let effect_count = effects.clone();
    let tool = Capability::new(
        ToolDefinition::new("hidden_alias", "effect", json!({"type":"object"})),
        move |context| {
            effect_count.fetch_add(1, Ordering::SeqCst);
            Ok(context.result("original effect"))
        },
    )
    .with_concrete_target(target.clone())
    .with_model_exposure(false);
    let tool_builder = HostRuntimeBuilder::new(
        model(),
        Arc::new(ScriptedProvider::new("unused", |_| {
            panic!("no direct provider")
        })),
    )
    .with_tool(tool);
    let mut create = SessionCreateRequest::new(SessionId::generate());
    create.inference_settings = Some(inference());
    let tool_id = sessions.create(create).await.unwrap().id().clone();
    let (tool_runtime, dispatcher) = tool_builder
        .with_dispatch_context(
            Some(tool_id.clone()),
            ServerPermissionPolicy::sessions(InteractionBroker::new()),
        )
        .build_with_dispatcher();
    let tools = SessionAuthorizedToolExecutor::new(
        sessions.clone(),
        tool_id.clone(),
        tool_runtime,
        dispatcher,
    );
    let capabilities = BTreeSet::from_iter(
        [
            "helper.run",
            "helper.validate",
            "helper.restricted.restricted",
            "helper.named.named",
            "helper.capture.verified",
            "tools.execute",
            "tools.schemas",
            "tool.effect",
        ]
        .map(str::to_owned),
    );
    let bindings = AdapterBindings {
        principal: "owner".into(),
        helpers: Arc::new(helpers),
        tools: Arc::new(tools),
        capabilities: capabilities.clone(),
        targets: BTreeMap::from([("tool.effect".into(), target)]),
    };
    let database = root.join(format!("s3-{}.sqlite", uuid::Uuid::new_v4()));
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
    let engine = Arc::new(Engine::new(1).unwrap());
    let inner = RoutineAgentHostFactory {
        bindings: Arc::new(SourceBindings { bindings: BindOnce(Mutex::new(Some(bindings))), source: source_copy }),
    };
    let replacement = if mutation == 2 {
        AgentDefinitionCatalog::from_sources(
            vec![
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: other}\nspec: {}\n"
                    .into(),
            ],
            "other",
        )
        .unwrap()
    } else {
        AgentDefinitionCatalog::from_sources(
            vec![
                source
                    .replace("ORIGINAL", "REPLACED")
                    .replace("permission: allow", "permission: deny"),
            ],
            "named",
        )
        .unwrap()
    };
    let host_factory: Arc<dyn HostFactory> = if mutation == 0 {
        Arc::new(inner)
    } else {
        Arc::new(MutateAfterAdmission {
            inner,
            catalog: current,
            replacement,
        })
    };
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        host_factory,
        Arc::new(SystemClock),
        1,
    )
    .unwrap();
    let source = r#"export default async function() {
        let proposals = 0;
        const helper = await host.call('helper.run', {selection:{kind:'restricted',profile:'restricted'},source:'effective',task:'work',capture:'verified',maxTurns:3,deadlineMs:3000,responseBytes:65536,completionBytes:65536}, async proposal => {
            const schemas = await host.call('tools.schemas', {targets:[{server:'exact.server',tool:'effect-name'}]});
            if (schemas.schemas.length !== 1) throw new Error('schema');
            if (proposals++ === 0) {
                const effect = await host.call('tools.execute', {target:{server:'exact.server',tool:'effect-name'},callId:'nested-effect',arguments:{}});
                if (effect.result.content[0].text !== 'original effect') throw new Error('effect');
                return {decision:'reject',feedback:{proposalId:proposal.proposalId,callId:'capture',message:'verify again'}};
            }
            return {decision:'accept'};
        });
        if (helper.result.captures[0].value.verified !== true) throw new Error('capture');
        return helper.result.response;
    }"#;
    let source = if named {
        source.replace(
            "kind:'restricted',profile:'restricted'",
            "kind:'named',name:'named'",
        )
    } else {
        source.to_owned()
    };
    let source = if invalid_capture {
        source.replace("maxTurns:3", "maxTurns:5")
    } else {
        source
    };
    let handle = service
        .invoke(
            definition(&engine, &source, &capabilities),
            "run".into(),
            caller(),
            Value::Null,
        )
        .unwrap()
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(8), handle.join())
        .await
        .unwrap();
    assert_eq!(
        outcome.status,
        Some(Status::Succeeded),
        "{outcome:?}; sessions={:?}; operations={:?}",
        sessions.list().await,
        store.operations(handle.id.clone(), 0, 32).await
    );
    assert_eq!(outcome.result, Some(json!("accepted")));
    assert!(outcome.cleanup_complete);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        turns.load(Ordering::SeqCst),
        if invalid_capture { 5 } else { 3 }
    );
    let operations = store.operations(handle.id.clone(), 0, 32).await.unwrap();
    assert_eq!(operations.len(), 4);
    assert!(
        operations
            .iter()
            .all(|operation| operation.status == OperationStatus::Completed)
    );
    let journal = serde_json::to_string(&operations).unwrap();
    assert!(!journal.contains("original effect"));
    assert!(!journal.contains("verify again"));
    let helper = sessions
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.id != tool_id)
        .unwrap();
    let record = agent_store.load(&helper.id).await.unwrap();
    assert_eq!(record.agent_definition.is_some(), named);
    if named {
        assert_eq!(
            serde_json::from_value::<AgentDefinitionBundle>(
                record.agent_definition.clone().unwrap()
            )
            .unwrap()
            .identity(),
            expected_identity
        );
    }
    assert!(
        record
            .snapshot()
            .prompts
            .iter()
            .any(|prompt| prompt.contains("verify again"))
    );
    service.shutdown().await.unwrap();
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(database).unwrap();
}
