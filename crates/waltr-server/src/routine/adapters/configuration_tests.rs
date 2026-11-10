use super::*;
use crate::{
    agent_definition::AgentDefinitionState,
    session::{SessionCatalogSource, SessionRegistrationInput, SessionService},
};
use tokio_util::sync::CancellationToken;
struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        172_800_000
    }
}
struct SqlFiles(std::path::PathBuf);
impl Drop for SqlFiles {
    fn drop(&mut self) {
        for name in [
            "main.db",
            "main.db-wal",
            "main.db-shm",
            "mcp.db",
            "mcp.db-wal",
            "mcp.db-shm",
        ] {
            drop(std::fs::remove_file(self.0.join(name)));
        }
        drop(std::fs::remove_dir(&self.0));
    }
}
async fn sql_fixture() -> (SqlFiles, Persistence) {
    let root = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&root).unwrap();
    let dir = SqlFiles(root.join(format!("routine-configuration-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&dir.0).unwrap();
    let main = format!("sqlite://{}?mode=rwc", dir.0.join("main.db").display());
    let mcp = format!("sqlite://{}?mode=rwc", dir.0.join("mcp.db").display());
    let db = Persistence::initialize(DatabaseConfig::from_urls(&main, &mcp).unwrap())
        .await
        .unwrap();
    (dir, db)
}
struct NoDirectTools;
impl crate::agent_host::authorized_tools::AuthorizedToolExecutor for NoDirectTools {
    fn execute(
        &self,
        _: crate::agent_host::authorized_tools::AuthorizedToolRequest,
        _: CancellationToken,
    ) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<waltr_agent::ToolResult> {
        Box::pin(async { Err(crate::agent_host::authorized_tools::AuthorizedToolError::Denied) })
    }
    fn schemas(
        &self,
        _: Vec<crate::agent_host::authorized_tools::ConcreteToolTarget>,
        _: CancellationToken,
    ) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<Vec<waltr_agent::ToolDefinition>>
    {
        Box::pin(async { Err(crate::agent_host::authorized_tools::AuthorizedToolError::Denied) })
    }
    fn cleanup(&self) -> crate::agent_host::authorized_tools::AuthorizedToolFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
struct ConfigurationHostFactory {
    inner: Arc<dyn HostFactory>,
}
impl HostFactory for ConfigurationHostFactory {
    fn preflight(
        &self,
        _: crate::routine::history::InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<PreparedHost> {
        Box::pin(async { Err(PortError::Invalid) })
    }
    fn preflight_input(
        &self,
        binding: crate::routine::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
        input: Value,
    ) -> PortFuture<PreparedHost> {
        let inner = self.inner.clone();
        Box::pin(async move {
            if definition.envelope().name != "configuration"
                || selected.name != "tool"
                || caller.origin != Origin::Tool
                || input.as_object().is_none_or(|o| o.len() != 1)
                || input
                    .get("prompt")
                    .and_then(Value::as_str)
                    .is_none_or(|p| p.trim().is_empty() || p.len() > 12_288)
            {
                return Err(PortError::Invalid);
            }
            inner
                .preflight_input(binding, definition, selected, caller, input)
                .await
        })
    }
}
struct Events;
impl waltr_agent::SessionEventSink for Events {
    fn send(&self, _: waltr_agent::SessionEvent) -> waltr_agent::SessionEventSinkFuture {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn named_configuration_normal_mcp_preview_commit_and_complete_candidate_guards() {
    use crate::routine::adapters::{
        authorized_tools::{AdapterBindings, InvocationAdapterFactory, RoutineAgentHostFactory},
        helper::CoordinatorHelperRunner,
    };
    use crate::{
        agent_host::runtime_factory::{McpConnectionKind, McpResolvedConnection},
        permission::{PermissionLevel, PermissionPolicy},
    };
    struct Bind(Arc<dyn HelperRunner>);
    impl InvocationAdapterFactory for Bind {
        fn bind(
            &self,
            _: Arc<Definition>,
            _: SelectedEntrypoint,
            caller: Caller,
        ) -> PortFuture<AdapterBindings> {
            let helpers = self.0.clone();
            Box::pin(async move {
                Ok(AdapterBindings {
                    principal: caller.principal,
                    helpers,
                    tools: Arc::new(NoDirectTools),
                    capabilities: BTreeSet::from([
                        "helper.run".into(),
                        "helper.named.configuration".into(),
                    ]),
                    targets: BTreeMap::new(),
                })
            })
        }
    }
    for mode in [
        "preview",
        "commit",
        "stale",
        "invalid",
        "caller-deny",
        "named-deny",
        "nested-deny",
    ] {
        let root = std::env::temp_dir()
            .join("opencode")
            .join(format!("configuration-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".waltr/resources/mcp-servers")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/mcp", listener.local_addr().unwrap());
        std::fs::write(root.join(".waltr/resources/mcp-servers/waltr.yaml"),format!("apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: waltr}}\nspec:\n  transport: streamableHttp\n  url: {base}\n")).unwrap();
        let materializer = crate::catalog::CatalogMaterializer::load(
            &crate::test_support::default_catalog_loader(),
        )
        .await
        .unwrap();
        let state = AgentDefinitionState::load_unified(&root, materializer).unwrap();
        let before = state.resource_catalog().etag;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let provider = Arc::new(ScriptedProvider::new(
            "configuration-real-catalog",
            move |request| {
                let turn = count.fetch_add(1, Ordering::SeqCst);
                if mode == "named-deny" {
                    if turn == 0 {
                        assert!(
                            request
                                .tools
                                .iter()
                                .all(|tool| tool.name != "waltr__execute")
                        );
                        return vec![
                            Ok(CompletionEvent::ToolCall(ToolCall::new(
                                "forbidden",
                                "waltr__execute",
                                json!({"action":"session.interrupt","input":{"id":"not-authorized"}}),
                            ))),
                            Ok(CompletionEvent::Finished),
                        ];
                    }
                    let result = request
                        .turns
                        .iter()
                        .flat_map(|turn| &turn.items)
                        .find_map(|item| match item {
                            waltr_agent::TurnItem::ToolResult(result)
                                if result.call_id == "forbidden" =>
                            {
                                Some(result)
                            }
                            _ => None,
                        })
                        .unwrap();
                    assert!(result.is_error);
                    return vec![
                        Ok(CompletionEvent::Delta(
                            "Named policy denied the control operation.".into(),
                        )),
                        Ok(CompletionEvent::Finished),
                    ];
                }
                if turn == 0 {
                    return vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "metadata",
                            "read_mcp_resource",
                            json!({"server":"waltr","uri":"waltr://resources/agents%2Fcoding.yaml"}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ];
                }
                if turn == 1 {
                    if mode == "preview" {
                        return vec![
                            Ok(CompletionEvent::Delta(
                                "Preview only: append a narrow comment; no mutation.".into(),
                            )),
                            Ok(CompletionEvent::Finished),
                        ];
                    }
                    let revision = if mode == "caller-deny" {
                        "denied".to_owned()
                    } else {
                        let result = request
                            .turns
                            .iter()
                            .flat_map(|t| &t.items)
                            .find_map(|item| match item {
                                waltr_agent::TurnItem::ToolResult(result)
                                    if result.call_id == "metadata" =>
                                {
                                    Some(result)
                                }
                                _ => None,
                            })
                            .unwrap();
                        let envelope: Value =
                            serde_json::from_str(result.text_projection()).unwrap();
                        let metadata: Value = serde_json::from_str(
                            envelope["data"]["contents"][0]["text"].as_str().unwrap(),
                        )
                        .unwrap();
                        metadata["revision"].as_str().unwrap().to_owned()
                    };
                    let expected = if mode == "stale" {
                        "stale-revision"
                    } else {
                        &revision
                    };
                    let edit = if mode == "invalid" {
                        json!({"operation":"replace","old_text":"kind: Agent","new_text":"kind: InvalidKind"})
                    } else {
                        json!({"operation":"insert","text":"\n# configuration guarded change\n","placement":"end"})
                    };
                    vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "guarded-edit",
                            "waltr__edit",
                            json!({"uri":"waltr://resources/agents%2Fcoding.yaml","expected_revision":expected,"edits":[edit]}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ]
                } else {
                    assert!(format!("{request:?}").contains("guarded-edit"));
                    vec![
                        Ok(CompletionEvent::Delta(
                            "The guarded attempt is complete; no automatic retry.".into(),
                        )),
                        Ok(CompletionEvent::Finished),
                    ]
                }
            },
        ));
        let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
            .await
            .unwrap();
        let catalog = state.clone();
        let endpoint = base.clone();
        let supplied = provider.clone();
        let runtime = Arc::new(
            AgentSessionRuntimeFactory::new(
                sandbox.handle(),
                root.clone(),
                FileCredentialStore::new(root.join("credentials.json")),
                state
                    .snapshot()
                    .catalog()
                    .resolved("configuration")
                    .unwrap()
                    .clone(),
            )
            .with_agent_catalog_provider(move || catalog.snapshot().catalog().clone())
            .with_mcp_connection_resolver(move |_| {
                let url = endpoint.clone();
                Box::pin(async move {
                    Ok(vec![McpResolvedConnection {
                        key: "waltr".into(),
                        kind: McpConnectionKind::StreamableHttp,
                        command: String::new(),
                        url,
                    }])
                })
            })
            .with_capability_policy(crate::permission::ServerPermissionPolicy::sessions(
                crate::interaction::InteractionBroker::new(),
            ))
            .with_runtime_provider(move |_| {
                Ok(AgentRuntimeProvider::new(model(), supplied.clone()))
            }),
        );
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(runtime.session_factory(sessions));
        let host = SessionService::from_sessions(
            sessions.clone(),
            SessionRegistrationInput {
                workspace_root: root.clone(),
                config: crate::config::ConfigHandle::from_resources(
                    crate::test_support::resource_state_with_overrides(&[]),
                ),
                credential_store: FileCredentialStore::new(root.join("host-credentials.json"))
                    .into(),
                trusted_mcp_oauth_origins: vec![],
                catalog: SessionCatalogSource::Static(AgentDefinitionCatalog::from_sources(
                    vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: default}\nspec: {}\n".into()],
                    "default",
                ).unwrap()),
                event_sink: Arc::new(Events),
                sandbox: sandbox.handle(),
            },
        );
        let server =
            Arc::new(crate::mcp_service::WaltrMcpServer::new(host, state.clone()).unwrap());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let http = tokio::spawn(async move {
            axum::serve(
                listener,
                ::mcp::server::streamable_http_router(server).layer(axum::Extension(
                    crate::local_admission::ValidatedLocalOrigin(
                        Arc::new(CancellationToken::new()),
                    ),
                )),
            )
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
        });
        let restriction = PermissionPolicy::new(if mode == "caller-deny" {
            PermissionLevel::Deny
        } else {
            PermissionLevel::Allow
        });
        let restriction = if mode == "nested-deny" {
            use crate::permission::{
                PermissionEffect, PermissionInputCondition, PermissionInputRule,
                PermissionInputScalar, PermissionInputValueMatcher, PermissionJsonPointer,
                PermissionToolSelector,
            };
            restriction.with_input_rule(
                PermissionToolSelector::exact("waltr__edit"),
                PermissionInputRule::new(
                    vec![PermissionInputCondition {
                        pointer: PermissionJsonPointer::parse("/edits/0/placement").unwrap(),
                        value: PermissionInputValueMatcher::Exact(PermissionInputScalar::String(
                            "end".into(),
                        )),
                    }],
                    PermissionEffect::deny(),
                )
                .unwrap(),
            )
        } else {
            restriction
        };
        let helpers = CoordinatorHelperRunner::new(
            sessions.clone(),
            runtime,
            inference(),
            BTreeMap::new(),
            BTreeMap::new(),
            None,
            "owner".into(),
        )
        .unwrap()
        .with_permission_restriction(restriction);
        let engine = Arc::new(Engine::new(1).unwrap());
        let definition = Definition::parse(
            include_str!("../../../tests/fixtures/routines/configuration.json"),
            engine.as_ref(),
        )
        .unwrap();
        let (_files, db) = sql_fixture().await;
        let service = InvocationService::new(
            engine.clone(),
            db.script_invocation_store(),
            Arc::new(ConfigurationHostFactory {
                inner: Arc::new(RoutineAgentHostFactory {
                    bindings: Arc::new(Bind(Arc::new(helpers))),
                }),
            }),
            Arc::new(FixedClock),
            2,
        )
        .unwrap();
        let mut input = caller();
        input.principal = "owner".into();
        input.origin = Origin::Tool;
        input.ownership = crate::routine::history::Ownership::Tool;
        let outcome = service
            .invoke(
                definition,
                "tool".into(),
                input,
                json!({"prompt":"Preview or attempt the one guarded resource change."}),
            )
            .unwrap()
            .await
            .unwrap()
            .join()
            .await;
        assert!(outcome.cleanup_complete, "{mode}: {outcome:?}");
        let after = state.resource_catalog();
        let changed = after.etag != before;
        assert_eq!(changed, mode == "commit", "{mode}: {outcome:?}");
        if mode == "commit" {
            assert!(
                after
                    .resources
                    .iter()
                    .find(|r| r.path == "agents/coding.yaml")
                    .unwrap()
                    .active_source
                    .contains("configuration guarded change")
            );
        }
        service.shutdown().await.unwrap();
        sessions.shutdown().await;
        engine.shutdown().await.unwrap();
        stop.cancel();
        http.await.unwrap();
        sandbox.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
