use super::*;

pub(super) struct AdministrativeProbe {
    requests: AtomicUsize,
    first_write: Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>,
    arrived: tokio::sync::Notify,
}

pub(super) struct ObservedContext {
    pub context: Arc<CoordinatorContext<MemorySessionStore>>,
    pub probe: Arc<AdministrativeProbe>,
}

impl ContextPort for ObservedContext {
    fn snapshot_identity(&self) -> Option<waltr_agent::ContextIdentity> {
        self.context.snapshot_identity()
    }
    fn resolve_source(&self, source: &str) -> std::result::Result<SessionCopy, PortError> {
        self.context.resolve_source(source)
    }
    fn check(&self, operation: &ContextOperation) -> std::result::Result<(), PortError> {
        self.context.check(operation)
    }
    fn prepare(&self) -> PortFuture<()> {
        self.context.prepare()
    }
    fn execute(
        &self,
        operation: ContextOperation,
        cancellation: CancellationToken,
    ) -> PortFuture<Value> {
        if matches!(&operation, ContextOperation::Transact(args) if args.masks.is_some()) {
            let mut hold = self.probe.first_write.lock().unwrap();
            if self.probe.requests.load(Ordering::SeqCst) == 0 {
                *hold = Some(
                    self.context
                        .test_administrative_gate()
                        .try_lock_owned()
                        .unwrap(),
                );
            }
            self.probe.requests.fetch_add(1, Ordering::SeqCst);
            self.probe.arrived.notify_one();
        }
        self.context.execute(operation, cancellation)
    }
    fn cleanup(&self) -> PortFuture<()> {
        self.context.cleanup()
    }
}

struct ConcurrentDecisions {
    helpers: tokio::sync::Barrier,
    keep_left: Arc<std::sync::atomic::AtomicBool>,
}
impl LlmStream for ConcurrentDecisions {
    fn name(&self) -> &'static str {
        "concurrent-decisions"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let event = if request
                .tool_calls()
                .any(|call| call.name == "_helper_capture")
            {
                CompletionEvent::Delta("accepted".into())
            } else {
                let value = if request.system_prompt == Some("STRATEGY") {
                    json!({"kind":"edit","edits":[{"key":"left","instruction":"preserve left"},{"key":"right","instruction":"preserve right"}]})
                } else {
                    assert_eq!(request.system_prompt, Some("MASK"));
                    // Both helpers must have read their category before either mutation arrives.
                    self.helpers.wait().await;
                    if self.keep_left.load(Ordering::SeqCst)
                        && request.prompt.contains("\"key\":\"left\"")
                    {
                        json!({"kind":"keep"})
                    } else {
                        json!({"kind":"replace","summary":"acknowledged category summary"})
                    }
                };
                CompletionEvent::ToolCall(ToolCall::new("capture", "_helper_capture", value))
            };
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(event),
                Ok(CompletionEvent::Finished),
            ])) as BoxCompletionStream)
        })
    }
}

#[tokio::test]
async fn two_strategy_children_overlap_before_the_gate_and_both_publish_acknowledged_masks() {
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
    let mut updates = source["items"].as_array().unwrap().iter().enumerate().map(|(index,item)| json!({"itemId":item["item_id"],"key":"routine.category.v1","value":{"state":"present","value":if index == 0 {"left"} else {"right"}}})).collect::<Vec<_>>();
    updates.push(json!({"key":"routine.categories.v1","value":{"state":"present","value":{"version":1,"through":source["boundary"],"categories":[{"key":"left","description":"left"},{"key":"right","description":"right"}],"note":"pinned"}}}));
    context.execute(operation("context.transact", json!({"source":"original","expectedRevision":source["revision"],"predicates":[],"updates":updates})), CancellationToken::new()).await.unwrap();
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
        .await
        .unwrap();
    let keep_left = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(ConcurrentDecisions {
        helpers: tokio::sync::Barrier::new(2),
        keep_left: keep_left.clone(),
    });
    let catalog = named_workflow_catalog();
    let supplied = provider.clone();
    let factory = Arc::new(
        AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("unused-concurrent-auth.json")),
            catalog.resolved("source").unwrap().clone(),
        )
        .with_agent_catalog_provider(move || catalog.clone())
        .with_capability_policy(crate::permission::ServerPermissionPolicy::sessions(
            crate::interaction::InteractionBroker::new(),
        ))
        .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), supplied.clone()))),
    );
    let helper_sessions = sessions
        .clone()
        .with_runtime_factory(factory.session_factory(sessions.clone()));
    let effective = context.helper_copy();
    let helper_catalog = named_workflow_snapshot();
    let helpers: Helpers = Arc::new(move || {
        Arc::new(
            CoordinatorHelperRunner::new(
                helper_sessions.clone(),
                factory.clone(),
                inference(),
                BTreeMap::new(),
                capture_contracts(),
                Some(effective.clone()),
                "owner".into(),
            )
            .unwrap().with_catalog(helper_catalog.clone(), Some(inference())),
        )
    });
    let engine = Arc::new(Engine::new(4).unwrap());
    let definitions = Arc::new(definitions(&engine));
    // Anchor shared memory independently of guest-runtime connection turnover.
    let url = format!(
        "sqlite:file:s4-concurrent-{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4()
    );
    let anchor = <sqlx::SqliteConnection as sqlx::Connection>::connect(&url)
        .await
        .unwrap();
    let persistence =
        Persistence::initialize(DatabaseConfig::from_urls(&url, "sqlite::memory:").unwrap())
            .await
            .unwrap();
    let store = persistence.script_invocation_store();
    let slot = Arc::new(Mutex::new(None));
    let mut services = Vec::new();
    for keep in [false, true] {
        keep_left.store(keep, Ordering::SeqCst);
        let probe = Arc::new(AdministrativeProbe {
            requests: AtomicUsize::new(0),
            first_write: Mutex::new(None),
            arrived: tokio::sync::Notify::new(),
        });
        let service = InvocationService::new(
            engine.clone(),
            store.clone(),
            Arc::new(RoutineAgentHostFactory {
                bindings: Arc::new(WorkflowBindings {
                    context: Arc::new(context.child_context()),
                    helpers: helpers.clone(),
                    service: slot.clone(),
                    definitions: definitions.clone(),
                    scope: None,
                    administrative_probe: Some(probe.clone()),
                }),
            }),
            Arc::new(SystemClock),
            4,
        )
        .unwrap();
        *slot.lock().unwrap() = Some(service.clone());
        let mut caller = caller();
        caller.origin = Origin::Tool;
        caller.ownership = Ownership::Tool;
        let before = sessions.store().load(&id).await.unwrap();
        let handle = service
            .invoke(
                definitions["context-management"].clone(),
                "session-tool".into(),
                caller,
                json!({"instruction":"preserve both"}),
            )
            .unwrap()
            .await
            .unwrap();
        let overlapping = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = probe.arrived.notified();
                if probe.requests.load(Ordering::SeqCst) == 1 {
                    break;
                }
                notified.await;
            }
        })
        .await;
        assert!(
            overlapping.is_ok(),
            "requests={}",
            probe.requests.load(Ordering::SeqCst)
        );
        assert_eq!(
            sessions.store().load(&id).await.unwrap(),
            before,
            "both helpers finish their shared barrier before one aggregate write is released"
        );
        probe.first_write.lock().unwrap().take();
        let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
            .await
            .unwrap();
        assert_eq!(outcome.status, Some(Status::Succeeded), "{outcome:?}");
        assert!(outcome.cleanup_complete);
        let result = outcome.result.unwrap();
        let decisions = result["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 2);
        for decision in decisions {
            assert_eq!(decision["succeeded"], true);
            assert_eq!(decision["cleanupComplete"], true);
            let kept = keep && decision["key"] == "left";
            assert_eq!(decision["result"]["applied"], !kept);
            assert!(decision["result"]["maskId"].is_string());
            if kept {
                let old = before.active_session_masks().into_iter().find(|mask| mask.mask.key == "category.left").unwrap();
                assert_eq!(decision["result"]["maskId"], old.mask.id.as_str());
            }
        }
        assert_eq!(
            sessions
                .store()
                .load(&id)
                .await
                .unwrap()
                .active_session_masks()
                .len(),
            2
        );
        service.shutdown().await.unwrap();
        services.push(service);
    }
    let history = store.list(0, 32).await.unwrap();
    assert_eq!(history.len(), 8);
    assert!(
        history
            .iter()
            .all(|entry| entry.status == Status::Succeeded)
    );
    let current = sessions.store().load(&id).await.unwrap();
    let mut external = waltr_agent::SessionMaskMutation::new(current.last_timeline_cursor());
    external.subtract.keys.push("category.left".into());
    external.add.push(waltr_agent::SessionMaskAddition::items(
        "external",
        context.snapshot_identity().unwrap().boundary(),
        vec![serde_json::from_value(source["items"][0]["item_id"].clone()).unwrap()],
        waltr_agent::ReplacementUserContent::new("external change"),
    ));
    sessions.store().mutate_masks(&id, external).await.unwrap();
    let changed = sessions.store().load(&id).await.unwrap();
    for decision in [
        json!({"kind":"keep"}),
        json!({"kind":"replace","summary":"must not commit"}),
    ] {
        let child = context.child_context();
        assert!(matches!(
            child
                .execute(
                    operation(
                        "context.transact",
                        json!({"source":"original","expectedRevision":current.revision.get().to_string(),"predicates":[],"updates":[{"key":"test-decision","value":{"state":"present","value":decision}}]})
                    ),
                    CancellationToken::new()
                )
                .await,
            Err(PortError::Conflict)
        ));
        child.cleanup().await.unwrap();
        assert_eq!(sessions.store().load(&id).await.unwrap(), changed);
    }
    assert!(matches!(
        context.execute(operation("context.original", json!({})), CancellationToken::new()).await,
        Err(PortError::Conflict)
    ));
    context.cleanup().await.unwrap();
    slot.lock().unwrap().take();
    drop(services);
    sessions.shutdown().await;
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    sqlx::Connection::close(anchor).await.unwrap();
}
