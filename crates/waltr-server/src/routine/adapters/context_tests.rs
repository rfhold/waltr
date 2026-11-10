use super::super::context::{CoordinatorContext, CoordinatorSessionContext};
use super::*;
use crate::routine::{
    children::{ChildBounds, ChildScope, ServiceChildren},
    context::{ContextOperation, ContextPort},
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "workflow_tests.rs"]
mod workflow_tests;

fn bundle() -> AgentDefinitionBundle {
    AgentDefinitionCatalog::from_sources(vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: source}\nspec: {systemPrompt: SOURCE}\n".into()], "source").unwrap().bundle("source").unwrap()
}

async fn completed() -> (SessionCoordinator<MemorySessionStore>, SessionId) {
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = SessionId::generate();
    let mut request = SessionCreateRequest::new(id.clone());
    request.agent_definition = Some(serde_json::to_value(bundle()).unwrap());
    request.inference_settings = Some(inference());
    sessions.create(request).await.unwrap();
    let runtime = AgentRuntime::builder(
        model(),
        Arc::new(ScriptedProvider::from_turns(
            "source",
            vec![vec![
                CompletionEvent::Delta("completed source response".into()),
                CompletionEvent::Finished,
            ]],
        )),
    )
    .build();
    assert!(
        sessions
            .prompt(&runtime, id.clone(), "original user context")
            .stream()
            .collect::<Vec<_>>()
            .await
            .iter()
            .all(Result::is_ok)
    );
    (sessions, id)
}

fn operation(name: &str, args: Value) -> ContextOperation {
    ContextOperation::parse(name, args).unwrap()
}

struct CancelAfterDurableMessage(CancellationToken);
struct ContinuationHost;
impl waltr_agent::AgentHostRuntimeFactory for ContinuationHost {
    fn build<'a>(&'a self, _: &'a waltr_agent::SessionRecord) -> waltr_agent::HostRuntimeFuture<'a> {
        Box::pin(async {
            Ok(AgentRuntime::builder(model(), Arc::new(ScriptedProvider::from_turns("continuation", vec![vec![CompletionEvent::Delta("continued".into()), CompletionEvent::Finished]]))).build())
        })
    }
}
impl waltr_agent::SessionEventSink for CancelAfterDurableMessage {
    fn send(&self, _: waltr_agent::SessionEvent) -> waltr_agent::SessionEventSinkFuture {
        self.0.cancel();
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn fork_cancel_after_atomic_creation_keeps_destination_without_start_or_replay() {
    let (sessions, source) = completed().await;
    let cancellation = CancellationToken::new();
    let sessions =
        sessions.with_session_event_sink(Arc::new(CancelAfterDurableMessage(cancellation.clone())));
    let context = CoordinatorContext::pin(sessions.clone(), &source, "owner", false)
        .await
        .unwrap();
    let captured = sessions.store().load(&source).await.unwrap();
    let result = context
        .execute(
            operation(
                "context.create",
                json!({"source":"effective","copyHistory":false,"inheritDefinition":true,"inheritInference":true,"message":"summary\n\n<fork-direction>\n  exact direction\n\n</fork-direction>"}),
            ),
            cancellation.clone(),
        )
        .await
        .unwrap();
    assert!(cancellation.is_cancelled());
    assert_eq!(result["created"], true);
    assert!(context.execute(operation("context.start", json!({"destination":result["destination"]})), cancellation.clone()).await.is_err());
    let id = SessionId::new(result["sessionId"].as_str().unwrap());
    let destination = sessions.store().load(&id).await.unwrap();
    assert_eq!(destination.agent_definition, captured.agent_definition);
    assert_eq!(destination.inference_settings, captured.inference_settings);
    assert_eq!(
        destination
            .materialize_context(waltr_agent::SessionContextMode::Raw)
            .current_prompt
            .as_deref(),
        Some("summary\n\n<fork-direction>\n  exact direction\n\n</fork-direction>")
    );
    assert!(sessions.active_state(&id).await.is_none());
    assert_eq!(sessions.list().await.unwrap().len(), 2);
    context.cleanup().await.unwrap();
    assert_eq!(sessions.store().load(&id).await.unwrap(), destination);
    sessions.shutdown().await;
}

#[tokio::test]
async fn compaction_barrier_is_busy_before_history_and_dropped_cleanup_still_joins_release() {
    for continue_after in [false, true] {
        let (sessions, id) = completed().await;
        let credentials = waltr_agent::FileCredentialStore::new(crate::test_support::temp_root().join("unused-auth.json"));
        let sessions = sessions.with_runtime_factory(waltr_agent::AgentSessionRuntimeFactory::new(credentials).with_host_runtime_factory(Arc::new(ContinuationHost)));
        let context = CoordinatorContext::compaction(sessions.clone(), &id, "owner")
            .await
            .unwrap();
        context.prepare().await.unwrap();
        let competitor = CoordinatorContext::compaction(sessions.clone(), &id, "owner")
            .await
            .unwrap();
        assert!(matches!(
            competitor.prepare().await,
            Err(PortError::Conflict)
        ));
        competitor.cleanup().await.unwrap();
        let snapshot = context.execute(operation("context.snapshot", json!({})), CancellationToken::new()).await.unwrap();
        let result = context
            .execute(
                operation(
                    "context.transact",
                    json!({"source":"effective","expectedRevision":snapshot["revision"],"predicates":[],"updates":[],"masks":{"expected":[],"removeKeys":[],"removeOverlaps":[snapshot["boundary"]],"add":[{"key":"routine-compaction","through":snapshot["boundary"],"replacement":"safe compacted context"}]}}),
                ),
                CancellationToken::new(),
            )
            .await;
        let result = result.unwrap();
        assert_eq!(result["committed"], true);
        if continue_after { assert_eq!(context.execute(operation("context.continue", json!({"cursor":result["cursor"]})), CancellationToken::new()).await.unwrap()["armed"], true); }
        let record = sessions.store().load(&id).await.unwrap();
        assert_eq!(record.active_session_masks().len(), 1);
        assert_eq!(
            record
                .materialize_context(waltr_agent::SessionContextMode::Effective)
                .current_prompt
                .as_deref(),
            Some("safe compacted context")
        );
        drop(context.cleanup());
        let reason =
            waltr_agent::SessionCoordinationReason::new("verify.release", None, None).unwrap();
        let lease = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(lease) = sessions
                    .acquire_pause_reject_busy(&id, reason.clone())
                    .await
                {
                    break lease;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        sessions.release_pause(&lease).await.unwrap();
        context.cleanup().await.unwrap();
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn opaque_item_metadata_and_disjoint_masks_commit_atomically_and_reject_stale_sources() {
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", true)
        .await
        .unwrap();
    let original = context
        .execute(
            operation("context.original", json!({})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let items = original["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let checkpoint = json!({"version":1,"through":original["boundary"],"categories":[{"key":"left","description":"immutable left"},{"key":"right","description":"immutable right"}],"note":"captured"});
    let mut updates = items.iter().enumerate().map(|(index, event)| json!({"itemId":event["item_id"],"key":"routine.category.v1","value":{"state":"present","value":if index == 0 {"left"} else {"right"}}})).collect::<Vec<_>>();
    updates.push(json!({"key":"routine.categories.v1","value":{"state":"present","value":checkpoint}}));
    context
        .execute(
            operation("context.transact", json!({"source":"original","expectedRevision":original["revision"],"predicates":[{"key":"routine.categories.v1","value":{"state":"absent"}}],"updates":updates})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for (index, key) in ["left", "right"].into_iter().enumerate() {
        let snapshot = context.execute(operation("context.original", json!({})), CancellationToken::new()).await.unwrap();
        let selected = context
            .execute(
                operation("context.history", json!({"source":"original","itemIds":[items[index]["item_id"]]})),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(selected["items"].as_array().unwrap().len(), 1);
        assert_eq!(selected["itemMetadata"][items[index]["item_id"].as_str().unwrap()]["routine.category.v1"], key);
        let expected = snapshot["masks"].as_array().unwrap().iter().map(|mask| json!([mask["key"],mask["id"]])).collect::<Vec<_>>();
        let applied = context.execute(operation("context.transact", json!({"source":"original","expectedRevision":snapshot["revision"],"predicates":[{"key":"routine.categories.v1","value":{"state":"present","value":checkpoint}}],"updates":[],"masks":{"expected":expected,"removeKeys":[],"removeOverlaps":[],"add":[{"key":format!("category.{key}"),"through":snapshot["boundary"],"itemIds":[items[index]["item_id"]],"replacement":format!("{key} safe summary")}]}})), CancellationToken::new()).await.unwrap();
        assert_eq!(applied["committed"], true);
    }
    let record = sessions.store().load(&id).await.unwrap();
    assert_eq!(record.active_session_masks().len(), 2);
    let newer = CoordinatorContext::pin(sessions.clone(), &id, "owner", true)
        .await
        .unwrap();
    let selected = newer
        .execute(
            operation("context.original", json!({})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(selected["items"].as_array().unwrap().len(), 2);
    let left_key = "category.left";
    let left_mask = record
        .active_session_masks()
        .into_iter()
        .find(|active| active.mask.key == left_key)
        .unwrap();
    let left = selected["masks"].as_array().unwrap().iter().find(|mask| mask["key"] == left_key).unwrap();
    assert_eq!(left["id"], left_mask.mask.id.as_str());
    assert_eq!(left["replacement"], "left safe summary");
    let right_key = "category.right".to_owned();
    let right_mask = record
        .active_session_masks()
        .into_iter()
        .find(|active| active.mask.key == right_key)
        .unwrap();
    let mut external = waltr_agent::SessionMaskMutation::new(record.last_timeline_cursor());
    external.subtract.keys.push(right_key.clone());
    external
        .expected_masks
        .push((right_key, Some(right_mask.mask.id.clone())));
    sessions.mutate_context_masks(&id, external).await.unwrap();
    for stale in [&context, &newer] {
        assert!(matches!(
            stale
                .execute(
                    operation(
                        "context.transact",
                        json!({"source":"original","expectedRevision":selected["revision"],"predicates":[],"updates":[{"key":"must-not-commit","value":{"state":"present","value":true}}]}),
                    ),
                    CancellationToken::new(),
                )
                .await,
            Err(PortError::Conflict)
        ));
    }
    let changed = sessions.store().load(&id).await.unwrap();
    assert!(!changed.metadata.values.contains_key("must-not-commit"));
    sessions
        .set_metadata_value(&id, "routine.categories.v1", json!(null))
        .await
        .unwrap();
    let unproven = CoordinatorContext::pin(sessions.clone(), &id, "owner", true)
        .await
        .unwrap();
    let opaque = unproven.execute(operation("context.metadata", json!({"source":"original","key":"routine.categories.v1"})), CancellationToken::new()).await.unwrap();
    assert_eq!(opaque, json!({"state":"present","value":null}), "the host does not interpret workflow schema or confer authority from it");
    let denied = CoordinatorContext::pin(sessions, &id, "owner", false)
        .await
        .unwrap();
    assert!(matches!(
        denied
            .execute(
                operation("context.original", json!({})),
                CancellationToken::new()
            )
            .await,
        Err(PortError::Denied)
    ));
}

#[tokio::test]
async fn typed_fork_uses_immutable_capture_and_exact_direction_without_metadata_copy() {
    let (sessions, id) = completed().await;
    sessions
        .set_metadata_value(&id, "private", json!("do-not-copy"))
        .await
        .unwrap();
    let mut edited = inference();
    edited.provider.model_id = "configured-before-capture".into();
    sessions
        .store()
        .set_inference_settings(&id, Some(edited))
        .await
        .unwrap();
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", false)
        .await
        .unwrap();
    let before = sessions.store().load(&id).await.unwrap();
    let direction = "  exact direction\nwith trailing space  ";
    let result = context
        .execute(
            operation(
                "context.create",
                json!({"source":"effective","copyHistory":false,"inheritDefinition":true,"inheritInference":true,"message":format!("captured summary\n\n<fork-direction>\n{direction}\n</fork-direction>")}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let destination = sessions
        .store()
        .load(&SessionId::new(result["sessionId"].as_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(destination.agent_definition, before.agent_definition);
    assert_eq!(destination.inference_settings, before.inference_settings);
    assert!(!destination.metadata.values.contains_key("private"));
    let encoded = serde_json::to_string(&destination).unwrap();
    assert!(encoded.contains(
        "captured summary\\n\\n<fork-direction>\\n  exact direction\\nwith trailing space  \\n</fork-direction>"
    ));
    assert!(!destination.metadata.values.contains_key("helperPrincipal"));
}

struct ContextBindings {
    context: Arc<dyn ContextPort>,
    helpers: Arc<dyn Fn() -> Arc<dyn HelperRunner> + Send + Sync>,
    capabilities: BTreeSet<String>,
}
impl InvocationAdapterFactory for ContextBindings {
    fn bind_context(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<Option<Arc<dyn ContextPort>>> {
        let context = self.context.clone();
        Box::pin(async move { Ok(Some(context)) })
    }
    fn bind(
        &self,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<AdapterBindings> {
        let helpers = (self.helpers)();
        let capabilities = self.capabilities.clone();
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
}

#[tokio::test]
async fn script_title_runs_real_restricted_helper_and_exact_absent_cas_in_sqlite_history() {
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = SessionId::generate();
    let mut create = SessionCreateRequest::new(id.clone());
    create.start = SessionStart::Fresh {
        messages: vec![SessionStartMessage::user("first durable message").unwrap()],
    };
    sessions.create(create).await.unwrap();
    let title = "  Exact final title\n";
    let provider = Arc::new(ScriptedProvider::new("title", move |request| {
        assert!(request.tools.is_empty());
        assert!(request.prompt.contains("first durable message"));
        vec![
            Ok(CompletionEvent::Delta(title.into())),
            Ok(CompletionEvent::Finished),
        ]
    }));
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let catalog = AgentDefinitionCatalog::from_sources(vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: source}\nspec: {systemPrompt: SOURCE}\n".into()], "source").unwrap();
    let factory = Arc::new(AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.clone(),
        FileCredentialStore::new(root.join("unused-s4-auth.json")),
        catalog.resolved("source").unwrap().clone(),
    ));
    let mut settings = inference();
    settings.provider.reasoning_effort = Some("none".into());
    let helper_roles = factory.session_roles.clone();
    let helper_sessions = sessions.clone();
    let helpers = Arc::new(move || {
        Arc::new(
            CoordinatorHelperRunner::new(
                helper_sessions.clone(),
                factory.clone(),
                settings.clone(),
                BTreeMap::from([(
                    "title".into(),
                    RestrictedHelperProfile {
                        runtime: crate::agent_host::HostRuntimeBuilder::new(
                            model(),
                            provider.clone(),
                        ),
                        inference: settings.clone(),
                        prompt: "Return only a title.".into(),
                        max_turns: 1,
                    },
                )]),
                BTreeMap::new(),
                None,
                "owner".into(),
            )
            .unwrap(),
        ) as Arc<dyn HelperRunner>
    });
    let engine = Arc::new(Engine::new(1).unwrap());
    let definition = Definition::parse(include_str!("../../../tests/fixtures/routines/title.json"), &*engine).unwrap();
    let database = root.join(format!("s4-title-{}.sqlite", uuid::Uuid::new_v4()));
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
    for expected in [true, false] {
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        Arc::new(RoutineAgentHostFactory {
            bindings: Arc::new(ContextBindings {
                context: Arc::new(
                    CoordinatorSessionContext::pin(sessions.clone(), &id)
                        .await
                        .unwrap(),
                ),
                helpers: helpers.clone(),
                capabilities: definition.envelope().spec.capabilities.clone(),
            }),
        }),
        Arc::new(SystemClock),
        1,
    )
    .unwrap();
        let mut caller = caller();
        caller.origin = Origin::Event;
        let handle = service
            .invoke(
                definition.clone(),
                "first-message".into(),
                caller,
                Value::Null,
            )
            .unwrap()
            .await
            .unwrap();
        let outcome = handle.join().await;
        assert_eq!(outcome.status, Some(Status::Succeeded), "{:?}", store.operations(handle.id.clone(), 0, 32).await);
        assert_eq!(outcome.result, Some(json!(expected)));
        assert!(outcome.cleanup_complete);
        let operations = store.operations(handle.id.clone(), 0, 32).await.unwrap();
        assert_eq!(operations.len(), if expected { 3 } else { 1 });
        assert!(
            operations
                .iter()
                .all(|operation| operation.status == OperationStatus::Completed)
        );
        service.shutdown().await.unwrap();
    }
    assert_eq!(
        sessions.store().load(&id).await.unwrap().metadata.values["title"],
        title.trim()
    );
    for summary in sessions.list().await.unwrap() {
        let record = sessions.store().load(&summary.id).await.unwrap();
        if helper_roles.excluded(&record) {
            assert_eq!(
                record
                    .inference_settings
                    .as_ref()
                    .unwrap()
                    .provider
                    .reasoning_effort
                    .as_deref(),
                Some("none")
            );
            assert!(
                CoordinatorSessionContext::pin(sessions.clone(), &summary.id)
                    .await
                    .is_ok()
            );
        }
    }
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(database).unwrap();
}

#[derive(Clone)]
struct TreeBindings {
    service: Arc<std::sync::Mutex<Option<InvocationService>>>,
    context: Arc<dyn ContextPort>,
    definitions: Arc<BTreeMap<String, Arc<Definition>>>,
    scope: Option<ChildScope>,
    admission_probe: Option<Arc<AdmissionProbe>>,
}
#[derive(Default)]
struct AdmissionProbe {
    started: std::sync::atomic::AtomicBool,
    dropped: std::sync::atomic::AtomicBool,
    wait_for_success: Option<Arc<dyn InvocationStore>>,
    deadline: Option<Duration>,
}

struct PhasedChildren {
    service: InvocationService,
    definitions: BTreeMap<String, Arc<Definition>>,
    scope: ChildScope,
    deadline: Duration,
    calls: std::sync::atomic::AtomicUsize,
    owners: std::sync::Mutex<Vec<Arc<ServiceChildren>>>,
}

impl crate::routine::children::ChildPort for PhasedChildren {
    fn check(
        &self,
        calls: &[crate::routine::children::ChildCall],
    ) -> std::result::Result<(), PortError> {
        ServiceChildren::new(
            self.service.clone(),
            self.definitions.clone(),
            self.scope.clone(),
        )?
        .check(calls)
    }

    fn call(
        &self,
        calls: Vec<crate::routine::children::ChildCall>,
        cancellation: CancellationToken,
    ) -> PortFuture<Vec<crate::routine::children::ChildResult>> {
        let mut scope = self.scope.clone();
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            // The parent awaits the first child's joined result before this call.
            scope.deadline = tokio::time::Instant::now() + self.deadline;
        }
        let children = Arc::new(
            ServiceChildren::new(self.service.clone(), self.definitions.clone(), scope).unwrap(),
        );
        self.owners.lock().unwrap().push(children.clone());
        Box::pin(async move { children.call(calls, cancellation).await })
    }

    fn cleanup(&self) -> PortFuture<()> {
        let children = self.owners.lock().unwrap().clone();
        Box::pin(async move {
            for child in children {
                child.cleanup().await?;
            }
            Ok(())
        })
    }
}
struct AdmissionGuard(Arc<AdmissionProbe>);
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.0
            .dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
impl InvocationAdapterFactory for TreeBindings {
    fn child_bindings(&self, scope: ChildScope) -> PortFuture<Arc<dyn InvocationAdapterFactory>> {
        let mut child = self.clone();
        child.scope = Some(scope);
        Box::pin(async move { Ok(Arc::new(child) as Arc<dyn InvocationAdapterFactory>) })
    }
    fn bind_context(
        &self,
        definition: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<Option<Arc<dyn ContextPort>>> {
        let context = self.context.clone();
        let probe = self.admission_probe.clone();
        Box::pin(async move {
            if let Some(probe) = probe {
                match definition.envelope().name.as_str() {
                    "blocked" => {
                        let _guard = AdmissionGuard(probe.clone());
                        probe
                            .started
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        std::future::pending::<()>().await;
                    }
                    "failing" => {
                        if let Some(store) = &probe.wait_for_success {
                            loop {
                                if store
                                    .list(0, 16)
                                    .await?
                                    .iter()
                                    .any(|entry| entry.status == Status::Succeeded)
                                {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }
                        } else {
                            while !probe.started.load(std::sync::atomic::Ordering::SeqCst) {
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(context))
        })
    }
    fn bind_children(
        &self,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<Option<Arc<dyn crate::routine::children::ChildPort>>> {
        let result = (|| {
            let scope = match self.scope.clone() {
                Some(scope) => scope,
                None => ChildScope::root(
                    self.context.snapshot_identity().unwrap(),
                    caller.principal,
                    definition.envelope().spec.capabilities.clone(),
                    format!("{}:{}", definition.envelope().name, selected.name),
                    tokio::time::Instant::now() + Duration::from_secs(10),
                )?,
            };
            if let Some(deadline) = self
                .admission_probe
                .as_ref()
                .and_then(|probe| probe.deadline)
            {
                return Ok(Some(Arc::new(PhasedChildren {
                    service: self.service.lock().unwrap().as_ref().unwrap().clone(),
                    definitions: self.definitions.as_ref().clone(),
                    scope,
                    deadline,
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    owners: std::sync::Mutex::new(Vec::new()),
                }) as Arc<dyn crate::routine::children::ChildPort>));
            }
            Ok(Some(Arc::new(ServiceChildren::new(
                self.service.lock().unwrap().as_ref().unwrap().clone(),
                self.definitions.as_ref().clone(),
                scope,
            )?)
                as Arc<dyn crate::routine::children::ChildPort>))
        })();
        Box::pin(async move { result })
    }
    fn bind(
        &self,
        definition: Arc<Definition>,
        _: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<AdapterBindings> {
        let capabilities = self.scope.as_ref().map_or_else(
            || definition.envelope().spec.capabilities.clone(),
            |scope| scope.capabilities.clone(),
        );
        Box::pin(async move {
            Ok(AdapterBindings {
                principal: caller.principal,
                helpers: Arc::new(SubstitutePorts::default()),
                tools: Arc::new(SubstitutePorts::default()),
                capabilities,
                targets: BTreeMap::new(),
            })
        })
    }
}

async fn child_vertical(
    service_capacity: usize,
    engine_capacity: usize,
    privilege_drop: bool,
    recursive: bool,
) {
    let (sessions, id) = completed().await;
    let context = Arc::new(
        CoordinatorContext::pin(sessions.clone(), &id, "owner", false)
            .await
            .unwrap(),
    );
    let engine = Arc::new(Engine::new(engine_capacity).unwrap());
    let child = Definition::parse(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"child","description":"test",
        "spec":{"source":"export default async function() { const snapshot = (await host.call('context.snapshot', {})).result; await host.call('context.transact', {source:snapshot.source, expectedRevision:snapshot.revision, predicates:[{key:'title',value:{state:'absent'}}], updates:[{key:'title',value:{state:'present',value:'child effect'}}]}); return 'child'; }",
        "entrypoints":{"run":{"trigger":"inline"}},"parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
        "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":128}},"capabilities":["context.snapshot","context.transact"]}}).to_string(), &*engine).unwrap();
    let mut caps = vec!["children.call", "context.snapshot"];
    if !privilege_drop {
        caps.push("context.transact");
    }
    let target = if recursive { "parent" } else { "child" };
    let parent = Definition::parse(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":"parent","description":"test",
        "spec":{"source":format!("export default async function() {{ return JSON.stringify((await host.call('children.call', {{calls:[{{key:'category',definition:'{target}',entrypoint:'run',input:null}}]}})).result); }}"),
        "entrypoints":{"run":{"trigger":"manual"}},"parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
        "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":4096}},"capabilities":caps}}).to_string(), &*engine).unwrap();
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let database = root.join(format!("s4-child-{}.sqlite", uuid::Uuid::new_v4()));
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
    let slot = Arc::new(std::sync::Mutex::new(None));
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        Arc::new(RoutineAgentHostFactory {
            bindings: Arc::new(TreeBindings {
                service: slot.clone(),
                context,
                definitions: Arc::new(BTreeMap::from([
                    ("child".into(), child),
                    ("parent".into(), parent.clone()),
                ])),
                scope: None,
                admission_probe: None,
            }),
        }),
        Arc::new(SystemClock),
        service_capacity,
    )
    .unwrap();
    *slot.lock().unwrap() = Some(service.clone());
    let handle = service
        .invoke(parent, "run".into(), caller(), Value::Null)
        .unwrap()
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(4), handle.join())
        .await
        .unwrap();
    let succeeded = service_capacity > 1 && engine_capacity > 1 && !privilege_drop && !recursive;
    let record = sessions.store().load(&id).await.unwrap();
    assert_eq!(record.metadata.values.contains_key("title"), succeeded);
    if succeeded {
        assert_eq!(record.metadata.values["title"], "child effect");
    }
    if !privilege_drop && !recursive {
        assert_eq!(outcome.status, Some(Status::Succeeded));
        let results: Value =
            serde_json::from_str(outcome.result.as_ref().unwrap().as_str().unwrap()).unwrap();
        assert_eq!(results[0]["succeeded"], succeeded);
        assert_eq!(results[0]["cleanupComplete"], true);
    }
    let history = store.list(0, 100).await.unwrap();
    if service_capacity == 1 || engine_capacity == 1 || privilege_drop || recursive {
        assert_eq!(history.len(), 1);
    } else {
        assert_eq!(history.len(), 2);
    }
    assert!(outcome.cleanup_complete);
    service.shutdown().await.unwrap();
    slot.lock().unwrap().take();
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn script_children_same_service_capacity_one_fails_fast_and_larger_capacity_commits() {
    child_vertical(1, 1, false, false).await;
    child_vertical(2, 1, false, false).await;
    child_vertical(2, 2, false, false).await;
}

#[tokio::test]
async fn script_children_capability_drop_and_recursive_cycle_fail_before_child_history() {
    child_vertical(2, 2, true, false).await;
    child_vertical(2, 2, false, true).await;
    assert!(
        ChildBounds {
            depth: 0,
            ..ChildBounds::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        ChildBounds {
            total: 17,
            ..ChildBounds::default()
        }
        .validate()
        .is_err()
    );
}

#[tokio::test]
async fn failed_child_cancels_and_joins_sibling_while_sibling_admission_is_blocked() {
    let (sessions, id) = completed().await;
    let context = Arc::new(
        CoordinatorContext::pin(sessions, &id, "owner", false)
            .await
            .unwrap(),
    );
    let engine = Arc::new(Engine::new(3).unwrap());
    let program = |name: &str, source: &str, trigger: &str, capabilities: Vec<&str>| {
        Definition::parse(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":name,"description":"test",
            "spec":{"source":source,"entrypoints":{"run":{"trigger":trigger}},
            "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
            "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":4096}},
            "capabilities":capabilities}}).to_string(), &*engine).unwrap()
    };
    let failing = program(
        "failing",
        "export default async function() { throw new Error('expected failure'); }",
        "inline",
        vec![],
    );
    let blocked = program(
        "blocked",
        "export default async function() { return 'must not run'; }",
        "inline",
        vec![],
    );
    let parent = program(
        "parent",
        "export default async function() { return JSON.stringify((await host.call('children.call', {calls:[{key:'failure',definition:'failing',entrypoint:'run',input:null},{key:'blocked',definition:'blocked',entrypoint:'run',input:null}]})).result); }",
        "manual",
        vec!["children.call"],
    );
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let database = root.join(format!("s4-child-blocked-{}.sqlite", uuid::Uuid::new_v4()));
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
    let slot = Arc::new(std::sync::Mutex::new(None));
    let probe = Arc::new(AdmissionProbe::default());
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        Arc::new(RoutineAgentHostFactory {
            bindings: Arc::new(TreeBindings {
                service: slot.clone(),
                context,
                definitions: Arc::new(BTreeMap::from([
                    ("failing".into(), failing),
                    ("blocked".into(), blocked),
                ])),
                scope: None,
                admission_probe: Some(probe.clone()),
            }),
        }),
        Arc::new(SystemClock),
        3,
    )
    .unwrap();
    *slot.lock().unwrap() = Some(service.clone());
    let handle = service
        .invoke(parent, "run".into(), caller(), Value::Null)
        .unwrap()
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(2), handle.join())
        .await
        .unwrap();
    assert_eq!(outcome.status, Some(Status::Succeeded));
    assert!(outcome.cleanup_complete);
    assert!(probe.started.load(std::sync::atomic::Ordering::SeqCst));
    assert!(probe.dropped.load(std::sync::atomic::Ordering::SeqCst));
    let results: Value = serde_json::from_str(outcome.result.unwrap().as_str().unwrap()).unwrap();
    assert_eq!(results.as_array().unwrap().len(), 2);
    for result in results.as_array().unwrap() {
        assert_eq!(result["succeeded"], false);
        assert_eq!(result["cleanupComplete"], true);
    }
    let history = store.list(0, 100).await.unwrap();
    assert_eq!(history.len(), 2);
    service.shutdown().await.unwrap();
    slot.lock().unwrap().take();
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn child_failure_deadline_and_parent_cancel_retain_successful_sibling_effects() {
    for mode in ["failure", "deadline", "cancel"] {
        let (sessions, id) = completed().await;
        let context = Arc::new(
            CoordinatorContext::pin(sessions.clone(), &id, "owner", false)
                .await
                .unwrap(),
        );
        let engine = Arc::new(Engine::new(3).unwrap());
        let program = |name: &str, source: &str, trigger: &str, capabilities: Vec<&str>| {
            Definition::parse(&json!({"apiVersion":"waltr.routine/v1","kind":"Routine","name":name,"description":"test",
                "spec":{"source":source,"entrypoints":{"run":{"trigger":trigger}},
                "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"null"}},
                "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":4096}},
                "capabilities":capabilities}}).to_string(), &*engine).unwrap()
        };
        let success = program(
            "success",
            "export default async function() { const snapshot = (await host.call('context.snapshot', {})).result; await host.call('context.transact', {source:snapshot.source, expectedRevision:snapshot.revision, predicates:[{key:'title',value:{state:'absent'}}], updates:[{key:'title',value:{state:'present',value:'retained sibling effect'}}]}); return 'committed'; }",
            "inline",
            vec!["context.snapshot", "context.transact"],
        );
        let second = if mode == "failure" {
            "failing"
        } else {
            "blocked"
        };
        let failing = program(
            second,
            "export default async function() { throw new Error('expected child failure'); }",
            "inline",
            vec![],
        );
        let parent = program(
            "parent",
            &format!(
                "export default async function() {{ const first = (await host.call('children.call', {{calls:[{{key:'success',definition:'success',entrypoint:'run',input:null}}]}})).result; const second = (await host.call('children.call', {{calls:[{{key:'second',definition:'{second}',entrypoint:'run',input:null}}]}})).result; return JSON.stringify([first[0], second[0]]); }}"
            ),
            "manual",
            vec!["children.call", "context.snapshot", "context.transact"],
        );
        let root = std::env::temp_dir().join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join(format!("s4-child-partial-{}.sqlite", uuid::Uuid::new_v4()));
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
        let probe = Arc::new(AdmissionProbe {
            wait_for_success: Some(store.clone()),
            deadline: (mode == "deadline").then_some(Duration::from_millis(500)),
            ..Default::default()
        });
        let slot = Arc::new(std::sync::Mutex::new(None));
        let service = InvocationService::new(
            engine.clone(),
            store.clone(),
            Arc::new(RoutineAgentHostFactory {
                bindings: Arc::new(TreeBindings {
                    service: slot.clone(),
                    context,
                    definitions: Arc::new(BTreeMap::from([
                        ("success".into(), success),
                        (second.into(), failing),
                    ])),
                    scope: None,
                    admission_probe: Some(probe.clone()),
                }),
            }),
            Arc::new(SystemClock),
            3,
        )
        .unwrap();
        *slot.lock().unwrap() = Some(service.clone());
        let handle = service
            .invoke(parent, "run".into(), caller(), Value::Null)
            .unwrap()
            .await
            .unwrap();
        if mode == "cancel" {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if probe.started.load(Ordering::SeqCst)
                        && store
                            .list(0, 16)
                            .await
                            .unwrap()
                            .iter()
                            .any(|entry| entry.status == Status::Succeeded)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            handle.cancel();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(3), handle.join())
            .await
            .unwrap();
        assert!(outcome.cleanup_complete, "{mode}");
        assert_eq!(
            sessions.store().load(&id).await.unwrap().metadata.values["title"],
            "retained sibling effect"
        );
        if mode == "cancel" {
            assert_eq!(outcome.status, Some(Status::Uncertain));
        } else {
            assert_eq!(outcome.status, Some(Status::Succeeded), "{mode}: {outcome:?}; operations={:?}; history={:?}", store.operations(handle.id.clone(), 0, 32).await, store.list(0, 16).await);
            let results: Value =
                serde_json::from_str(outcome.result.unwrap().as_str().unwrap()).unwrap();
            assert_eq!(results[0]["key"], "success");
            assert_eq!(results[0]["succeeded"], true, "{mode}: children={results}; history={:?}", store.list(0, 16).await.unwrap());
            assert_eq!(results[0]["result"], "committed");
            assert_eq!(results[1]["key"], "second");
            assert_eq!(results[1]["succeeded"], false);
            assert_eq!(results[1]["cleanupComplete"], true);
        }
        if mode != "failure" {
            assert!(probe.dropped.load(Ordering::SeqCst));
        }
        let history = store.list(0, 16).await.unwrap();
        assert_eq!(history.len(), if mode == "failure" { 3 } else { 2 });
        assert_eq!(
            history
                .iter()
                .filter(|entry| entry.status == Status::Succeeded)
                .count(),
            if mode == "cancel" { 1 } else { 2 }
        );
        service.shutdown().await.unwrap();
        slot.lock().unwrap().take();
        sessions.shutdown().await;
        engine.shutdown().await.unwrap();
        persistence.sqlite_pool().unwrap().close().await;
        std::fs::remove_file(database).unwrap();
    }
}

#[tokio::test]
async fn bounded_semantic_consumer_counts_only_completed_facts_and_reports_live_loss() {
    use crate::routine::triggers::{SemanticTriggers, TriggerSource, TriggerSourceError};
    let mut triggers = SemanticTriggers::default();
    let id = SessionId::new("source");
    triggers.watch(id.clone(), "incarnation".into()).unwrap();
    let event = |index: u64, kind| SessionSemanticEvent {
        id: format!("event{index}"),
        session_id: id.clone(),
        incarnation: Some("incarnation".into()),
        revision: waltr_agent::Revision::new(index + 1),
        cursor: None,
        kind,
    };
    for index in 0..19 {
        assert!(
            triggers
                .consume(event(
                    index,
                    SessionSemanticKind::TurnCompleted { turn: index }
                ))
                .is_none()
        );
    }
    let last = event(19, SessionSemanticKind::TurnCompleted { turn: 19 });
    assert_eq!(triggers.consume(last.clone()).unwrap().session_id, id);
    assert!(triggers.consume(last).is_none());
    assert!(triggers.consume(event(0, SessionSemanticKind::TurnCompleted { turn: 0 })).is_none());
    for index in 20..50 {
        assert!(
            triggers
                .consume(event(
                    index,
                    SessionSemanticKind::ToolResultCommitted {
                        call_id: "task".into(),
                        tool_name: "task".into(),
                        succeeded: true
                    }
                ))
                .is_none()
        );
    }
    let finished = event(50, SessionSemanticKind::RunFinished);
    assert!(triggers.consume(finished).is_none());
    for index in 51..53 {
        let mut task = event(
            index,
            SessionSemanticKind::RunFinished,
        );
        task.session_id = SessionId::new("real-child-contract");
        let origin = crate::agent_host::session_roles::TaskOrigin { parent: id.clone(), incarnation: "incarnation".into(), call: format!("call{index}"), turn: 0, depth: 1 };
        assert_eq!(triggers.task_completed(task, &origin).is_some(), index == 52);
    }
    let hub = SessionSemanticHub::new(1).unwrap();
    let mut source = hub.subscribe().unwrap();
    hub.publish(event(100, SessionSemanticKind::TurnCompleted { turn: 100 }))
        .unwrap();
    hub.publish(event(101, SessionSemanticKind::TurnCompleted { turn: 101 }))
        .unwrap();
    assert_eq!(
        TriggerSource::next(&mut source).await.unwrap_err(),
        TriggerSourceError::Lagged(1)
    );
    triggers.lost();
    assert!(
        triggers
            .consume(TriggerSource::next(&mut source).await.unwrap())
            .is_none()
    );
    for index in 0..31 {
        triggers
            .watch(
                SessionId::new(format!("bounded{index}")),
                "incarnation".into(),
            )
            .unwrap();
    }
    assert_eq!(
        triggers.watch(SessionId::new("overflow"), "incarnation".into()),
        Err(PortError::Unavailable)
    );
    triggers.forget(&id);
    triggers
        .watch(SessionId::new("released"), "incarnation".into())
        .unwrap();
}

#[tokio::test]
async fn invocation_coalescence_requires_exact_source_and_selection_with_bounded_waiters() {
    use crate::routine::triggers::{InvocationAdmission, InvocationCoalescer, InvocationIdentity};
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", true)
        .await
        .unwrap()
        .snapshot_identity()
        .unwrap();
    let gate = InvocationCoalescer::default();
    let invocation = InvocationIdentity { routine: "ordinary".into(), revision: "revision".into(), entrypoint: "run".into() };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let InvocationAdmission::Leader(mut leader) =
        gate.admit(id.clone(), context.clone(), invocation.clone(), deadline).unwrap()
    else {
        panic!("leader");
    };
    leader.bind_execution("execution").unwrap();
    let InvocationAdmission::Waiter(waiter) =
        gate.admit(id.clone(), context.clone(), invocation.clone(), deadline).unwrap()
    else {
        panic!("waiter");
    };
    let mut changed = invocation.clone();
    changed.revision = "another-revision".into();
    assert!(matches!(gate.admit(id.clone(), context.clone(), changed, deadline), Err(PortError::Conflict)));
    let mut changed = invocation.clone();
    changed.entrypoint = "another-entrypoint".into();
    assert!(matches!(gate.admit(id.clone(), context.clone(), changed, deadline), Err(PortError::Conflict)));
    let other = CoordinatorContext::pin(sessions.clone(), &id, "different-authority", true)
        .await
        .unwrap()
        .snapshot_identity()
        .unwrap();
    assert!(matches!(
        gate.admit(id.clone(), other.clone(), invocation.clone(), deadline),
        Err(PortError::Conflict)
    ));
    assert_eq!(
        leader.complete(crate::routine::triggers::CommittedInvocationReceipt {
            context: other.clone(),
            invocation: invocation.clone(),
            execution_id: "execution".into(),
            revision: waltr_agent::Revision::new(1),
            note: "not covered".into()
        }),
        Err(PortError::Conflict)
    );
    assert_eq!(waiter.await, Err(PortError::Unavailable));
    let InvocationAdmission::Leader(mut leader) =
        gate.admit(id.clone(), context.clone(), invocation.clone(), deadline).unwrap()
    else {
        panic!("leader");
    };
    leader.bind_execution("execution").unwrap();
    let mut waiters = Vec::new();
    for _ in 0..32 {
        let InvocationAdmission::Waiter(waiter) =
            gate.admit(id.clone(), context.clone(), invocation.clone(), deadline).unwrap()
        else {
            panic!("waiter");
        };
        waiters.push(waiter);
    }
    assert!(matches!(
        gate.admit(id.clone(), context.clone(), invocation.clone(), deadline),
        Err(PortError::Unavailable)
    ));
    leader
        .complete(crate::routine::triggers::CommittedInvocationReceipt {
            context: context.clone(),
            invocation: invocation.clone(),
            execution_id: "execution".into(),
            revision: waltr_agent::Revision::new(1),
            note: "exact committed note".into(),
        })
        .unwrap();
    for waiter in waiters {
        assert_eq!(waiter.await.unwrap(), "exact committed note");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
    let InvocationAdmission::Leader(leader) =
        gate.admit(id.clone(), context.clone(), invocation.clone(), deadline).unwrap()
    else {
        panic!("leader");
    };
    let InvocationAdmission::Waiter(waiter) = gate.admit(id, context, invocation, deadline).unwrap() else {
        panic!("waiter");
    };
    assert_eq!(waiter.await, Err(PortError::Denied));
    drop(leader);
}

#[tokio::test]
async fn generic_commit_receipt_rejects_missing_failed_and_externally_stale_work_without_replay() {
    use crate::routine::{service::Outcome, triggers::InvocationIdentity};
    let (sessions, id) = completed().await;
    let context = CoordinatorContext::pin(sessions.clone(), &id, "owner", false).await.unwrap();
    assert!(context.execute(operation("context.original", json!({})), CancellationToken::new()).await.is_err());
    let invocation = InvocationIdentity { routine: "ordinary".into(), revision: "revision".into(), entrypoint: "run".into() };
    let mut outcome = Outcome { id: "invocation".into(), status: Some(Status::Succeeded), result: Some(json!("committed result")), cleanup_complete: true };
    assert!(matches!(context.committed_receipt(invocation.clone(), &outcome).await, Err(PortError::Conflict)));
    let snapshot = context.execute(operation("context.snapshot", json!({})), CancellationToken::new()).await.unwrap();
    context.execute(operation("context.transact", json!({"source":"effective", "expectedRevision":snapshot["revision"],
        "predicates":[], "updates":[{"key":"opaque.checkpoint", "value":{"state":"present", "value":{"arbitrary":true}}}]})), CancellationToken::new()).await.unwrap();
    assert!(context.committed_receipt(invocation.clone(), &outcome).await.is_ok());
    outcome.status = Some(Status::Failed);
    assert!(matches!(context.committed_receipt(invocation.clone(), &outcome).await, Err(PortError::Conflict)));
    outcome.status = Some(Status::Interrupted);
    assert!(matches!(context.committed_receipt(invocation.clone(), &outcome).await, Err(PortError::Conflict)));
    outcome.status = Some(Status::Succeeded);
    outcome.cleanup_complete = false;
    assert!(matches!(context.committed_receipt(invocation.clone(), &outcome).await, Err(PortError::Conflict)));
    outcome.cleanup_complete = true;
    sessions.set_metadata_value(&id, "independent", json!("external commit")).await.unwrap();
    let after_external_commit = sessions.store().load(&id).await.unwrap();
    assert!(matches!(context.committed_receipt(invocation, &outcome).await, Err(PortError::Conflict)));
    assert_eq!(sessions.store().load(&id).await.unwrap(), after_external_commit);
}

#[test]
fn idle_requires_live_progress_and_never_replays_the_same_completed_boundary() {
    use crate::routine::triggers::SemanticTriggers;
    let id = SessionId::new("source");
    let mut triggers = SemanticTriggers::default();
    triggers.watch(id.clone(), "incarnation".into()).unwrap();
    let boundary = TimelineCursor::new(4);
    assert!(triggers.idle(&id, boundary).is_none());
    let event = SessionSemanticEvent { id: "committed-turn".into(), session_id: id.clone(), incarnation: Some("incarnation".into()),
        revision: waltr_agent::Revision::new(3), cursor: Some(boundary), kind: SessionSemanticKind::TurnCompleted { turn: 0 } };
    assert!(triggers.consume(event.clone()).is_none());
    assert!(triggers.idle(&id, boundary).is_some());
    assert!(triggers.idle(&id, boundary).is_none());
    assert!(triggers.consume(event).is_none());
    assert!(triggers.idle(&id, boundary).is_none());
    triggers.watch(id.clone(), "replacement".into()).unwrap();
    assert!(triggers.idle(&id, TimelineCursor::new(8)).is_none());
}
