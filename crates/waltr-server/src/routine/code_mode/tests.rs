use super::*;
use crate::agent_host::runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory;
use crate::permission::PermissionLevel;
use crate::persistence::{DatabaseConfig, Persistence};
use crate::{
    agent_host::{
        AgentDefinitionCatalog,
        runtime_factory::{McpConnectionKind, McpResolvedConnection},
    },
    auth::FileCredentialStore,
    interaction::{Interaction, InteractionBroker, InteractionResponse},
    permission::{PermissionResponse, ServerPermissionPolicy},
};
use ::mcp;
use futures_util::StreamExt as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use waltr_agent::*;
use waltr_script::Engine;

impl<S: SessionStore + Clone + Send + Sync + 'static> CodeModeAdapter<S> {
    // Exercise the underlying leased invocation fences without public transport narrowing.
    async fn call_internal(
        &self,
        input: CodeInput,
        cancellation: CancellationToken,
    ) -> std::result::Result<Value, PortError> {
        let binding = self.prepare(&input, Origin::Mcp)?;
        self.run(
            input,
            binding,
            (Origin::Mcp, uuid::Uuid::new_v4().to_string()),
            cancellation,
        )
        .await
    }
}

struct CodeProvider<F>(F);
impl<F> LlmStream for CodeProvider<F>
where
    F: for<'a> Fn(&CompletionRequest<'a>) -> Vec<waltr_agent::Result<CompletionEvent>>
        + Send
        + Sync,
{
    fn name(&self) -> &'static str {
        "code-model"
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::iter((self.0)(&request))) as BoxCompletionStream)
        })
    }
}

fn inference() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".into(),
            api: ModelApi::OpenAiResponses,
            url: "https://unused.invalid".into(),
            model_id: "test".into(),
            model_cost: None,
            headers: BTreeMap::new(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}
fn model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses)
}
struct RejectHost;
impl super::super::ports::HostFactory for RejectHost {
    fn preflight(
        &self,
        _: super::super::history::InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<super::super::ports::PreparedHost> {
        Box::pin(async { Err(PortError::Denied) })
    }
}
struct Upstream {
    skill_reads: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
    second: Arc<AtomicUsize>,
    revoked: Arc<AtomicBool>,
    block: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
}
fn skill_catalog() -> mcp::SkillCatalog {
    mcp::SkillCatalog::new().with_skill("ops/inspect", &[("SKILL.md", b"---\nname: inspect\ndescription: Inspect safely.\n---\nPerform one selected tool operation.\n".as_slice())]).unwrap()
}
impl mcp::server::ServerHandler for Upstream {
    fn server_info(&self) -> mcp::server::ServerInfo {
        mcp::server::ServerInfo::new("code-fixture", "1")
    }
    fn capabilities(&self) -> mcp::server::ServerCapabilities {
        mcp::server::ServerCapabilities::new().tools().skills()
    }
    fn list_skills(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpSkillList>> {
        let result = skill_catalog().list(None);
        Box::pin(async move { result })
    }
    fn list_resources(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpResourceList>> {
        Box::pin(async {
            Ok(mcp::McpResourceList {
                resources: vec![],
                next_cursor: None,
            })
        })
    }
    fn list_resource_templates(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpResourceTemplateList>> {
        Box::pin(async {
            Ok(mcp::McpResourceTemplateList {
                resource_templates: vec![],
                next_cursor: None,
            })
        })
    }
    fn get_skill(
        &self,
        uri: String,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpSkillResult>> {
        let result = skill_catalog().get(&uri);
        Box::pin(async move { result })
    }
    fn read_resource(
        &self,
        uri: String,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpResourceResult>> {
        self.skill_reads.fetch_add(1, Ordering::SeqCst);
        let result = skill_catalog()
            .read(&uri)
            .ok_or_else(|| mcp::server::ServerError::invalid_params("unknown skill"));
        Box::pin(async move { result })
    }
    fn list_tools(
        &self,
        _: Option<String>,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpToolList>> {
        let revoked = self.revoked.load(Ordering::SeqCst);
        Box::pin(async move {
            Ok(mcp::McpToolList {
                tools: if revoked {
                    vec![]
                } else {
                    ["effect","second","app_only","alias-collision","alias_collision"].into_iter().map(|name|mcp::parse_tool_definition(&json!({"name":name,"description":format!("{name} exact fixture description"),"inputSchema":{"type":"object","properties":{"selected":{"type":"boolean"}},"additionalProperties":false},"_meta":if name=="app_only" {json!({"ui":{"resourceUri":"ui://fixture/app","visibility":["app"]}})}else{json!({})}})).unwrap()).collect()
                },
                next_cursor: None,
            })
        })
    }
    fn call_tool(
        &self,
        call: mcp::McpToolCall,
        _: mcp::server::ServerContext,
    ) -> mcp::server::BoxFuture<mcp::server::ServerResult<mcp::McpToolResult>> {
        let effects = self.effects.clone();
        let second = self.second.clone();
        let block = self.block.clone();
        let release = self.release.clone();
        Box::pin(async move {
            match call.name.as_str() {
                "effect" | "alias-collision" => {
                    effects.fetch_add(1, Ordering::SeqCst);
                }
                "second" | "alias_collision" => {
                    second.fetch_add(1, Ordering::SeqCst);
                }
                "app_only" => panic!("App-only tool must never dispatch"),
                _ => {}
            }
            if block.load(Ordering::SeqCst) {
                release.notified().await;
            }
            Ok(mcp::McpToolResult::new(
                json!({"content":[{"type":"text","text":"immutable raw tool log"}],"structuredContent":{"selected":true}}),
            ))
        })
    }
}
struct FaultDispatchStore {
    inner: Arc<dyn CodeDispatchStore>,
    mode: Arc<AtomicUsize>,
}
struct AdmissionAckStore {
    inner: Arc<dyn super::super::ports::InvocationStore>,
    lost: Arc<AtomicBool>,
}
impl super::super::ports::InvocationStore for AdmissionAckStore {
    fn admit(&self, admission: super::super::history::Admission) -> PortFuture<()> {
        self.inner.admit(admission)
    }
    fn admit_with_projection(
        &self,
        admission: super::super::history::Admission,
        source: Option<String>,
        projection: Option<Value>,
    ) -> PortFuture<()> {
        let pending = self
            .inner
            .admit_with_projection(admission, source, projection);
        let lost = self.lost.clone();
        Box::pin(async move {
            pending.await?;
            if lost.load(Ordering::SeqCst) {
                Err(PortError::Unavailable)
            } else {
                Ok(())
            }
        })
    }
    fn running(&self, id: String, now: i64) -> PortFuture<()> {
        self.inner.running(id, now)
    }
    fn dispatch_intent(&self, operation: super::super::history::Operation) -> PortFuture<()> {
        self.inner.dispatch_intent(operation)
    }
    fn operation_done(
        &self,
        id: String,
        status: super::super::history::OperationStatus,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<()> {
        self.inner.operation_done(id, status, result, now)
    }
    fn terminal(
        &self,
        id: String,
        status: super::super::history::Status,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<super::super::history::Status> {
        self.inner.terminal(id, status, result, now)
    }
    fn get(&self, id: String) -> PortFuture<Option<super::super::history::Invocation>> {
        self.inner.get(id)
    }
    fn list(&self, after: i64, limit: u32) -> PortFuture<Vec<super::super::history::Invocation>> {
        self.inner.list(after, limit)
    }
    fn operations(
        &self,
        id: String,
        after: i64,
        limit: u32,
    ) -> PortFuture<Vec<super::super::history::Operation>> {
        self.inner.operations(id, after, limit)
    }
    fn reconcile(&self, now: i64) -> PortFuture<u64> {
        self.inner.reconcile(now)
    }
}
impl CodeDispatchStore for FaultDispatchStore {
    fn associate(&self, dispatch: CodeDispatch) -> PortFuture<()> {
        let inner = self.inner.clone();
        let mode = self.mode.load(Ordering::SeqCst);
        Box::pin(async move {
            if mode == 1 {
                return Err(PortError::Unavailable);
            }
            inner.associate(dispatch).await?;
            if mode == 2 {
                Err(PortError::Unavailable)
            } else {
                Ok(())
            }
        })
    }
    fn complete(&self, dispatch: CodeDispatch, outcome: Value) -> PortFuture<()> {
        let inner = self.inner.clone();
        let mode = self.mode.load(Ordering::SeqCst);
        Box::pin(async move {
            if mode == 3 {
                return Err(PortError::Unavailable);
            }
            inner.complete(dispatch, outcome).await?;
            if mode == 4 {
                Err(PortError::Unavailable)
            } else {
                Ok(())
            }
        })
    }
}
pub(crate) struct Fixture {
    pub(crate) requests: Arc<AtomicUsize>,
    pub(crate) resolutions: Arc<AtomicUsize>,
    skill_reads: Arc<AtomicUsize>,
    dir: std::path::PathBuf,
    sandbox: crate::sandbox::LocalSandbox,
    host: Arc<CodeModeHost<MemorySessionStore>>,
    service: InvocationService,
    sessions: SessionCoordinator<MemorySessionStore>,
    record: SessionRecord,
    principal: AuthenticatedPrincipal,
    pub(crate) effects: Arc<AtomicUsize>,
    second: Arc<AtomicUsize>,
    revoked: Arc<AtomicBool>,
    block: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
    fault: Arc<AtomicUsize>,
    admission_lost: Arc<AtomicBool>,
    stop: CancellationToken,
    http: tokio::task::JoinHandle<()>,
    broker: InteractionBroker,
}
impl Fixture {
    pub(crate) async fn new(expose: bool, permission: &str, source: &str) -> Self {
        Self::options(expose, permission, source, false, false).await
    }
    async fn options(
        expose: bool,
        permission: &str,
        source: &str,
        skills: bool,
        worker_code: bool,
    ) -> Self {
        let dir = crate::test_support::temp_root();
        let root = dir.clone();
        let effects = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let skill_reads = Arc::new(AtomicUsize::new(0));
        let revoked = Arc::new(AtomicBool::new(false));
        let block = Arc::new(AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let upstream = Arc::new(Upstream {
            skill_reads: skill_reads.clone(),
            effects: effects.clone(),
            second: second.clone(),
            revoked: revoked.clone(),
            block: block.clone(),
            release: release.clone(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let http = tokio::spawn(async move {
            let router =
                mcp::server::streamable_http_router(upstream).layer(axum::middleware::from_fn(
                    move |request: axum::extract::Request, next: axum::middleware::Next| {
                        observed.fetch_add(1, Ordering::SeqCst);
                        async move { next.run(request).await }
                    },
                ));
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        let yaml = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: selected }}\nspec:\n  systemPrompt: Use only the code API.\n  features:\n    - codeMode: true\n    - mcp: {{ exposeTools: {expose}, defaultPermissions: true }}\n  permissions:\n    - {{ name: upstream__second, permission: {permission} }}\n"
        );
        let worker = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: worker }}\nspec:\n  systemPrompt: Perform one concrete operation then finish.\n  features:\n    - codeMode: {worker_code}\n    - mcp: {{ exposeTools: true, defaultPermissions: true, skills: {skills} }}\n  permissions:\n    - {{ name: upstream__second, permission: ask }}\n"
        );
        let catalog = AgentDefinitionCatalog::from_sources(vec![yaml, worker], "selected").unwrap();
        let body = source.to_string();
        let turns = AtomicUsize::new(0);
        let helper_turns = AtomicUsize::new(0);
        let provider = Arc::new(CodeProvider(move |request: &CompletionRequest<'_>| {
            if !request.tools.iter().any(|tool| tool.name == "code") {
                let turn = helper_turns.fetch_add(1, Ordering::SeqCst);
                let event = if skills && turn == 0 {
                    CompletionEvent::ToolCall(ToolCall::new(
                        "helper-load",
                        "load_skill",
                        json!({"source":"mcp","server":"upstream","uri":"skill://ops/inspect/SKILL.md"}),
                    ))
                } else if turn == usize::from(skills) {
                    CompletionEvent::ToolCall(ToolCall::new(
                        "helper-effect",
                        "upstream__second",
                        json!({}),
                    ))
                } else {
                    CompletionEvent::Delta("helper finished".into())
                };
                return vec![Ok(event), Ok(CompletionEvent::Finished)];
            }
            let turn = turns.fetch_add(1, Ordering::SeqCst);
            assert!(request.tools.iter().any(|tool| tool.name == "code"));
            assert!(
                !request
                    .tools
                    .iter()
                    .any(|tool| tool.name.starts_with("upstream__")),
                "no eager concrete schemas"
            );
            let event = if turn.is_multiple_of(2) {
                CompletionEvent::ToolCall(ToolCall::new(
                    "stable-code-call",
                    "code",
                    json!({"action":"execute","source":body}),
                ))
            } else {
                CompletionEvent::Delta("done".into())
            };
            vec![Ok(event), Ok(CompletionEvent::Finished)]
        }));
        let sandbox = crate::sandbox::LocalSandbox::start(root.clone())
            .await
            .unwrap();
        let broker = InteractionBroker::new();
        let resolutions = Arc::new(AtomicUsize::new(0));
        let resolved = resolutions.clone();
        let base = AgentSessionRuntimeFactory::new(
            sandbox.handle(),
            root.clone(),
            FileCredentialStore::new(root.join("unused-auth.json")),
            catalog.resolved("selected").unwrap().clone(),
        )
        .with_agent_catalog(catalog.clone())
        .with_capability_policy(ServerPermissionPolicy::sessions(broker.clone()))
        .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(model(), provider.clone())))
        .with_mcp_connection_resolver(move |_| {
            resolved.fetch_add(1, Ordering::SeqCst);
            let url = url.clone();
            Box::pin(async move {
                Ok(vec![McpResolvedConnection {
                    key: "upstream".into(),
                    kind: McpConnectionKind::StreamableHttp,
                    command: String::new(),
                    url,
                }])
            })
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(base.session_factory(sessions));
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = Some(inference());
        request.agent_definition =
            Some(serde_json::to_value(catalog.bundle("selected").unwrap()).unwrap());
        let id = sessions.create(request).await.unwrap().id().clone();
        let record = sessions.store().load(&id).await.unwrap();
        let db = Persistence::initialize(
            DatabaseConfig::from_urls(
                &format!("sqlite:{}?mode=rwc", root.join("code.sqlite").display()),
                "sqlite::memory:",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let admission_lost = Arc::new(AtomicBool::new(false));
        let service = InvocationService::new(
            Arc::new(Engine::new(2).unwrap()),
            Arc::new(AdmissionAckStore {
                inner: db.script_invocation_store(),
                lost: admission_lost.clone(),
            }),
            Arc::new(RejectHost),
            Arc::new(super::super::ports::SystemClock),
            4,
        )
        .unwrap();
        let fault = Arc::new(AtomicUsize::new(0));
        let dispatches = Arc::new(FaultDispatchStore {
            inner: db.script_code_dispatch_store(),
            mode: fault.clone(),
        });
        let host = Arc::new(CodeModeHost::new(
            service.clone(),
            dispatches,
            sessions.clone(),
            Arc::new(base.clone()),
        ));
        let principal = AuthenticatedPrincipal::apps_test_principal("code-owner");
        let targets = [
            "effect",
            "second",
            "app_only",
            "alias-collision",
            "alias_collision",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            (
                format!("tool.t{i}"),
                ConcreteToolTarget::new("upstream", name).unwrap(),
            )
        })
        .collect();
        host.grant(
            principal.clone(),
            &record,
            CodeProfile {
                restriction: PermissionPolicy::new(PermissionLevel::Allow),
                targets,
                helpers: BTreeSet::new(),
            },
        )
        .unwrap();
        let installed = host.install(base, principal.clone());
        let sessions = sessions
            .clone()
            .with_runtime_factory(installed.session_factory(sessions));
        Self {
            requests,
            resolutions,
            broker,
            dir,
            sandbox,
            host,
            service,
            sessions,
            record,
            principal,
            effects,
            second,
            skill_reads,
            revoked,
            block,
            release,
            fault,
            admission_lost,
            stop,
            http,
        }
    }
    pub(crate) async fn adapter(&self) -> Arc<CodeModeAdapter<MemorySessionStore>> {
        let (runtime, dispatcher) = self
            .host
            .helpers
            .as_ref()
            .clone()
            .for_concrete_calls()
            .runtime_for_record(&self.record, self.sessions.clone())
            .await
            .unwrap();
        self.host
            .bind(&self.principal, &self.record, runtime, dispatcher)
            .unwrap()
    }
    pub(crate) async fn public_adapter(&self) -> Arc<CodeModeAdapter<MemorySessionStore>> {
        let (runtime, dispatcher) = self
            .host
            .helpers
            .as_ref()
            .clone()
            .for_concrete_calls()
            .runtime_with_dispatcher(&self.record, self.sessions.clone(), None)
            .await
            .unwrap();
        self.host
            .bind(&self.principal, &self.record, runtime, dispatcher)
            .unwrap()
    }
    pub(crate) async fn finish(self) {
        self.service.shutdown().await.unwrap();
        self.sessions.shutdown().await;
        self.stop.cancel();
        self.http.await.unwrap();
        self.sandbox.shutdown().await;
    }
    async fn pending(&self) -> (SessionId, crate::interaction::InteractionSnapshot) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                for summary in self.sessions.store().list().await.unwrap() {
                    let snapshot = self.broker.snapshot(&summary.id);
                    if !snapshot.pending.is_empty() {
                        return (summary.id, snapshot);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("helper did not enter original live consent within five seconds")
    }
    fn approve(
        &self,
        id: &SessionId,
        snapshot: &crate::interaction::InteractionSnapshot,
    ) -> std::result::Result<(), crate::interaction::InteractionError> {
        let request = &snapshot.pending[0];
        self.broker.respond(
            id,
            &snapshot.generation,
            &request.id,
            &request.operation_id,
            InteractionResponse::Permission(PermissionResponse::AllowOnce),
        )
    }
}
fn one_effect() -> &'static str {
    "const effect=await host.call('tools.execute',{target:{server:'upstream',tool:'effect'},callId:'effect',arguments:{selected:true}}); if(effect.status!=='completed'||effect.result?.is_error) throw new Error(effect.status); return {selected:true};"
}
fn partial() -> &'static str {
    "await host.call('tools.execute',{target:{server:'upstream',tool:'effect'},callId:'effect',arguments:{}}); const second=await host.call('tools.execute',{target:{server:'upstream',tool:'second'},callId:'second',arguments:{}}); if(second.status!=='completed'||second.result?.is_error) throw new Error(second.status); return {selected:true};"
}

#[tokio::test]
async fn expanded_capability_limit_rejects_without_installing_or_replacing_authority() {
    let fixture = Fixture::new(true, "deny", one_effect()).await;
    let profile = |count| CodeProfile {
        restriction: PermissionPolicy::new(PermissionLevel::Allow),
        targets: (0..count)
            .map(|n| {
                (
                    format!("tool.t{n}"),
                    ConcreteToolTarget::new("upstream", format!("t{n}")).unwrap(),
                )
            })
            .collect(),
        helpers: BTreeSet::new(),
    };
    let original = fixture.adapter().await;
    assert!(matches!(
        fixture
            .host
            .grant(fixture.principal.clone(), &fixture.record, profile(63)),
        Err(PortError::Invalid)
    ));
    original.check().unwrap();
    fixture
        .host
        .grant(fixture.principal.clone(), &fixture.record, profile(62))
        .unwrap();
    let installed = fixture.adapter().await;
    assert_eq!(installed.grant.capabilities().len(), 64);
    assert!(installed.definition("return null;", Origin::Inline).is_ok());
    assert!(matches!(
        fixture
            .host
            .grant(fixture.principal.clone(), &fixture.record, profile(63)),
        Err(PortError::Invalid)
    ));
    installed.check().unwrap();
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn stale_adapter_denies_every_action_after_session_delete_and_recreate() {
    let fixture = Fixture::new(true, "deny", one_effect()).await;
    let adapter = fixture.adapter().await;
    let other_owner = SessionCoordinator::new(fixture.sessions.store().clone());
    other_owner.delete(&fixture.record.id).await.unwrap();
    let mut request = SessionCreateRequest::new(fixture.record.id.clone());
    request.inference_settings = Some(inference());
    let recreated = other_owner.create(request).await.unwrap();
    assert_ne!(
        recreated.record.metadata.incarnation,
        fixture.record.metadata.incarnation
    );
    for input in [
        CodeInput::Discover { after: None },
        CodeInput::Schemas {
            targets: vec![ConcreteToolTarget::new("upstream", "effect").unwrap()],
        },
        CodeInput::Execute {
            source: one_effect().into(),
        },
    ] {
        assert!(matches!(
            adapter.call_internal(input, CancellationToken::new()).await,
            Err(PortError::Denied)
        ));
    }
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        fixture.dir.join("code.sqlite").display()
    ))
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM script_code_dispatch")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
    fixture.finish().await;
}

#[tokio::test]
async fn guest_schema_batch_limit_precedes_schema_reads_and_following_effect() {
    let source = "await host.call('tools.schemas',{targets:Array.from({length:9},()=>({server:'upstream',tool:'effect'}))}); await host.call('tools.execute',{target:{server:'upstream',tool:'effect'},callId:'effect',arguments:{}}); return true;";
    let fixture = Fixture::new(true, "deny", source).await;
    let adapter = fixture.adapter().await;
    let output = adapter
        .call_internal(
            CodeInput::Execute {
                source: source.into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        fixture.dir.join("code.sqlite").display()
    ))
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM script_operations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
    fixture.finish().await;
}

#[tokio::test]
async fn exact_alias_collision_targets_both_execute_without_conflation() {
    let source = "for(const tool of ['alias-collision','alias_collision']) { const r=await host.call('tools.execute',{target:{server:'upstream',tool},callId:tool,arguments:{}}); if(r.status!=='completed')throw new Error(r.status); } return true;";
    let fixture = Fixture::new(true, "deny", source).await;
    let adapter = fixture.adapter().await;
    let output = adapter
        .call_internal(
            CodeInput::Execute {
                source: source.into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "succeeded");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.second.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn reserved_admission_commit_with_lost_ack_never_starts_or_replays_source() {
    let fixture = Fixture::new(true, "deny", one_effect()).await;
    fixture.admission_lost.store(true, Ordering::SeqCst);
    let adapter = fixture.adapter().await;
    let input = CodeInput::Execute {
        source: one_effect().into(),
    };
    let binding = adapter.prepare(&input, Origin::Inline).unwrap();
    let result = adapter
        .run(
            input.clone(),
            binding.clone(),
            (Origin::Inline, "admission-lost".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(result["status"], "succeeded");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        fixture.dir.join("code.sqlite").display()
    ))
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM script_invocations WHERE id = $1")
            .bind(&binding.invocation_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM script_operations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    fixture.admission_lost.store(false, Ordering::SeqCst);
    let rebuilt = fixture.adapter().await;
    assert!(matches!(
        rebuilt
            .run(
                input,
                binding,
                (Origin::Inline, "admission-lost".into()),
                CancellationToken::new()
            )
            .await,
        Err(PortError::Conflict)
    ));
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    pool.close().await;
    fixture.finish().await;
}

#[tokio::test]
async fn public_code_named_helper_retains_internal_upstream_and_live_permission_owner() {
    let source = "const r=await host.call('helper.run',{selection:{kind:'named',name:'worker'},task:'Perform one operation',maxTurns:3,deadlineMs:10000,responseBytes:65536,completionBytes:65536}); if(r.status!=='accepted')throw new Error(r.status); return r.result.response;";
    let fixture = Fixture::new(true, "deny", source).await;
    let mut profile = fixture.public_adapter().await.grant.profile.clone();
    profile.helpers.insert("worker".into());
    fixture
        .host
        .grant(fixture.principal.clone(), &fixture.record, profile)
        .unwrap();
    let adapter = fixture.public_adapter().await;
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.resolutions.load(Ordering::SeqCst), 0);
    let approval = async {
        let (original, snapshot) = fixture.pending().await;
        assert_ne!(original, fixture.record.id);
        assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
        fixture.approve(&original, &snapshot).unwrap();
        assert!(fixture.approve(&original, &snapshot).is_err());
        original
    };
    let (helper_id, result) = tokio::join!(
        approval,
        PublicCodePort::call(
            &adapter,
            CodeInput::Execute {
                source: source.into()
            },
            CancellationToken::new()
        )
    );
    let result = result.unwrap();
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["result"], "helper finished");
    assert_eq!(fixture.second.load(Ordering::SeqCst), 1);
    assert!(fixture.requests.load(Ordering::SeqCst) > 0);
    assert_eq!(
        fixture
            .sessions
            .store()
            .load(&helper_id)
            .await
            .unwrap()
            .snapshot()
            .status,
        SessionStatus::Finished
    );
    drop(adapter);
    fixture.finish().await;
}

async fn stale_origin_cannot_approve_helper_effect(skills: bool, approve: bool) {
    let source = "return await host.call('helper.run',{selection:{kind:'named',name:'worker'},task:'Load the skill and perform one operation',maxTurns:4,deadlineMs:10000,responseBytes:65536,completionBytes:65536});";
    let fixture = Fixture::options(true, "deny", source, skills, false).await;
    let mut profile = fixture.adapter().await.grant.profile.clone();
    profile.helpers.insert("worker".into());
    fixture
        .host
        .grant(fixture.principal.clone(), &fixture.record, profile)
        .unwrap();
    let adapter = fixture.adapter().await;
    let other = SessionCoordinator::new(fixture.sessions.store().clone());
    let mut replaced = false;
    let mut initial_reads = 0;
    let call = adapter.call_internal(
        CodeInput::Execute {
            source: source.into(),
        },
        CancellationToken::new(),
    );
    tokio::pin!(call);
    let result = loop {
        tokio::select! {
            result=&mut call=>break result.unwrap(),
            (id, snapshot)=fixture.pending()=> {
                    assert_ne!(id,fixture.record.id);
                    if !replaced {
                        initial_reads=fixture.skill_reads.load(Ordering::SeqCst);
                        assert_eq!(fixture.second.load(Ordering::SeqCst),0);
                        other.delete(&fixture.record.id).await.unwrap();
                        other.create(SessionCreateRequest::new(fixture.record.id.clone())).await.unwrap();
                        replaced=true;
                    }
                    if approve { let _ = fixture.approve(&id, &snapshot); }
                    tokio::task::yield_now().await;
            }
        }
    };
    let effects = fixture.second.load(Ordering::SeqCst);
    let reads = fixture.skill_reads.load(Ordering::SeqCst);
    other.shutdown().await;
    assert!(matches!(
        fixture.service.shutdown().await,
        Ok(()) | Err(PortError::Uncertain)
    ));
    fixture.sessions.shutdown().await;
    fixture.stop.cancel();
    fixture.http.await.unwrap();
    fixture.sandbox.shutdown().await;
    assert!(replaced);
    assert_eq!(
        effects, 0,
        "approval of an old helper cannot authorize effects for a replacement origin"
    );
    assert_eq!(
        reads, initial_reads,
        "stale origin must not read Skills after consent"
    );
    assert_ne!(result["status"], "succeeded", "{result}");
}

#[tokio::test]
async fn helper_permission_approval_after_outer_replacement_cannot_dispatch() {
    stale_origin_cannot_approve_helper_effect(false, true).await;
}

#[tokio::test]
async fn helper_skills_approval_after_outer_replacement_cannot_dispatch() {
    stale_origin_cannot_approve_helper_effect(true, true).await;
}

#[tokio::test]
async fn helper_pending_consent_cancels_on_durable_origin_replacement_without_response() {
    stale_origin_cannot_approve_helper_effect(false, false).await;
    stale_origin_cannot_approve_helper_effect(true, false).await;
}

#[tokio::test]
async fn helper_nesting_requires_independent_host_authority_and_keeps_parent_call_budget() {
    let source = "return await host.call('helper.run',{selection:{kind:'named',name:'worker'},task:'work',maxTurns:1,deadlineMs:1000,responseBytes:65536,completionBytes:65536});";
    let fixture = Fixture::options(true, "deny", source, false, true).await;
    let mut profile = fixture.adapter().await.grant.profile.clone();
    profile.helpers.insert("worker".into());
    fixture
        .host
        .grant(fixture.principal.clone(), &fixture.record, profile)
        .unwrap();
    let adapter = fixture.adapter().await;
    let result = adapter
        .call_internal(
            CodeInput::Execute {
                source: source.into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        result["status"], "uncertain",
        "nested code must not gain the parent's host provider: {result}"
    );
    assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
    let source = "for(let i=0;i<256;i++)await host.call('tools.schemas',{targets:[]}); return await host.call('helper.run',{selection:{kind:'named',name:'worker'},task:'work',maxTurns:1,deadlineMs:1000,responseBytes:65536,completionBytes:65536});";
    let result = adapter
        .call_internal(
            CodeInput::Execute {
                source: source.into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "failed");
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        fixture.dir.join("code.sqlite").display()
    ))
    .await
    .unwrap();
    let operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM script_operations WHERE invocation = $1")
            .bind(result["invocationId"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        operations, 256,
        "the helper cannot acquire a fresh host-call budget"
    );
    pool.close().await;
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn helper_deadline_closes_pending_original_consent_without_effect_or_late_approval() {
    let source = "return await host.call('helper.run',{selection:{kind:'named',name:'worker'},task:'work',maxTurns:2,deadlineMs:100,responseBytes:65536,completionBytes:65536});";
    let fixture = Fixture::new(true, "deny", source).await;
    let mut profile = fixture.adapter().await.grant.profile.clone();
    profile.helpers.insert("worker".into());
    fixture
        .host
        .grant(fixture.principal.clone(), &fixture.record, profile)
        .unwrap();
    let adapter = fixture.adapter().await;
    let ((id, snapshot), result) = tokio::join!(
        fixture.pending(),
        adapter.call_internal(
            CodeInput::Execute {
                source: source.into()
            },
            CancellationToken::new()
        )
    );
    let result = result.unwrap();
    assert_eq!(result["status"], "uncertain", "{result}");
    assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
    assert!(fixture.approve(&id, &snapshot).is_err());
    assert!(matches!(
        fixture.service.shutdown().await,
        Err(PortError::Uncertain)
    ));
    fixture.sessions.shutdown().await;
    fixture.stop.cancel();
    fixture.http.await.unwrap();
    fixture.sandbox.shutdown().await;
}

#[tokio::test]
async fn real_js_named_helper_keeps_skills_and_permission_consent_in_original_run() {
    let source = "const r=await host.call('helper.run',{selection:{kind:'named',name:'worker'},task:'Load the selected skill and perform one operation',maxTurns:4,deadlineMs:10000,responseBytes:65536,completionBytes:65536}); return r.result.response;";
    let fixture = Fixture::options(true, "deny", source, true, false).await;
    let mut profile = fixture.adapter().await.grant.profile.clone();
    profile.helpers.insert("worker".into());
    fixture
        .host
        .grant(fixture.principal.clone(), &fixture.record, profile)
        .unwrap();
    let adapter = fixture.adapter().await;
    let approvals = async {
        let mut owner: Option<SessionId> = None;
        let mut skills = 0;
        loop {
            for summary in fixture.sessions.store().list().await.unwrap() {
                if owner.as_ref() == Some(&summary.id) && summary.status == SessionStatus::Finished
                {
                    return (summary.id, skills);
                }
                let snapshot = fixture.broker.snapshot(&summary.id);
                let Some(request) = snapshot.pending.first() else {
                    continue;
                };
                assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
                if let Some(owner) = &owner {
                    assert_eq!(owner, &summary.id);
                }
                owner = Some(summary.id.clone());
                skills += usize::from(
                    matches!(&request.request, Interaction::Permission { identity, .. } if identity.contains("skill")),
                );
                fixture.approve(&summary.id, &snapshot).unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    let ((owner, skills), result) = tokio::join!(
        approvals,
        PublicCodePort::call(
            &adapter,
            CodeInput::Execute {
                source: source.into()
            },
            CancellationToken::new()
        )
    );
    assert_ne!(owner, fixture.record.id);
    assert!(
        skills >= 2,
        "load and concrete execution retain distinct Skills consent"
    );
    assert_eq!(result.unwrap()["status"], "succeeded");
    assert_eq!(fixture.second.load(Ordering::SeqCst), 1);
    drop(adapter);
    fixture.finish().await;
}

#[tokio::test]
async fn normal_opt_in_runtime_runs_real_js_and_preserves_partial_effects_without_replay() {
    for permission in ["ask", "deny"] {
        let fixture = Fixture::new(true, permission, partial()).await;
        let runtime = fixture
            .sessions
            .resolve_runtime(&fixture.record.id)
            .await
            .unwrap();
        let turn = fixture
            .sessions
            .prompt(&runtime, fixture.record.id.clone(), "run code")
            .stream()
            .collect::<Vec<_>>();
        let events = if permission == "ask" {
            let answer = async {
                let (id, snapshot) = fixture.pending().await;
                let request = &snapshot.pending[0];
                fixture
                    .broker
                    .respond(
                        &id,
                        &snapshot.generation,
                        &request.id,
                        &request.operation_id,
                        InteractionResponse::Permission(PermissionResponse::Deny {
                            message: None,
                            mode: crate::permission::PermissionDenyMode::ReturnToolResult,
                        }),
                    )
                    .unwrap();
            };
            tokio::join!(turn, answer).0
        } else {
            turn.await
        };
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
        let record = fixture
            .sessions
            .store()
            .load(&fixture.record.id)
            .await
            .unwrap();
        let result = record
            .snapshot()
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .find_map(|item| match item {
                TurnItem::ToolResult(result) if result.name == "code" => {
                    Some(result.content.clone())
                }
                _ => None,
            })
            .unwrap();
        let output: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(output["status"], "failed");
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite:{}",
            fixture.dir.join("code.sqlite").display()
        ))
        .await
        .unwrap();
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT invocation_id,source_revision,outcome FROM script_code_dispatch",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        pool.close().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, output["invocationId"].as_str().unwrap());
        let runtime = fixture
            .sessions
            .resolve_runtime(&fixture.record.id)
            .await
            .unwrap();
        let repeat = fixture
            .sessions
            .prompt(
                &runtime,
                fixture.record.id.clone(),
                "repeat stable original call",
            )
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(repeat.iter().any(Result::is_err), "{repeat:?}");
        assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn discovery_is_bounded_schema_on_demand_and_hidden_inventory_is_not_a_grant() {
    let fixture = Fixture::new(true, "deny", one_effect()).await;
    let adapter = fixture.adapter().await;
    let page = adapter
        .call_internal(
            CodeInput::Discover { after: None },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let text = page.to_string();
    assert!(!text.contains("input_schema"));
    assert!(!text.contains("app_only"));
    assert!(!text.contains("second"));
    assert!(
        text.contains("alias-collision") && text.contains("alias_collision"),
        "{page}"
    );
    let schemas = adapter
        .call_internal(
            CodeInput::Schemas {
                targets: vec![ConcreteToolTarget::new("upstream", "effect").unwrap()],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(schemas["schemas"].as_array().unwrap().len(), 1);
    assert!(
        adapter
            .call_internal(
                CodeInput::Schemas {
                    targets: vec![ConcreteToolTarget::new("upstream", "app_only").unwrap()]
                },
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    fixture.finish().await;
    let hidden = Fixture::new(false, "deny", one_effect()).await;
    let adapter = hidden.adapter().await;
    let page = adapter
        .call_internal(
            CodeInput::Discover { after: None },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(page["tools"].as_array().unwrap().is_empty());
    let output = adapter
        .call_internal(
            CodeInput::Execute {
                source: one_effect().into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(hidden.effects.load(Ordering::SeqCst), 0);
    hidden.finish().await;
}

#[tokio::test]
async fn sql_association_refuses_same_identity_and_different_revision_after_ack_loss() {
    let fixture = Fixture::new(true, "allow", one_effect()).await;
    let adapter = fixture.adapter().await;
    let input = CodeInput::Execute {
        source: one_effect().into(),
    };
    let binding = adapter.prepare(&input, Origin::Inline).unwrap();
    let output = adapter
        .run(
            input.clone(),
            binding.clone(),
            (Origin::Inline, "same-call".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "succeeded");
    assert!(
        adapter
            .run(
                input,
                binding,
                (Origin::Inline, "same-call".into()),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    let changed = CodeInput::Execute {
        source: format!("{}\n// new revision", one_effect()),
    };
    let binding = adapter.prepare(&changed, Origin::Inline).unwrap();
    assert!(
        adapter
            .run(
                changed,
                binding,
                (Origin::Inline, "same-call".into()),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    fixture
        .host
        .revoke(&fixture.principal, &fixture.record.id)
        .unwrap();
    assert!(
        adapter
            .call_internal(
                CodeInput::Execute {
                    source: one_effect().into()
                },
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn current_remote_declaration_revocation_blocks_the_pinned_concrete_tool() {
    let fixture = Fixture::new(true, "allow", one_effect()).await;
    let adapter = fixture.adapter().await;
    fixture.revoked.store(true, Ordering::SeqCst);
    let output = adapter
        .call_internal(
            CodeInput::Execute {
                source: one_effect().into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn sql_faults_before_admission_and_after_effects_never_authorize_replay() {
    for mode in 1..=4 {
        let fixture = Fixture::new(true, "allow", one_effect()).await;
        let adapter = fixture.adapter().await;
        let input = CodeInput::Execute {
            source: one_effect().into(),
        };
        let binding = adapter.prepare(&input, Origin::Inline).unwrap();
        fixture.fault.store(mode, Ordering::SeqCst);
        let result = adapter
            .run(
                input.clone(),
                binding.clone(),
                (Origin::Inline, "fault-call".into()),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err(), "mode {mode}: {result:?}");
        assert_eq!(
            fixture.effects.load(Ordering::SeqCst),
            usize::from(mode >= 3),
            "mode {mode}"
        );
        if mode >= 3 {
            assert!(matches!(result, Err(PortError::Uncertain)));
        }
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite:{}",
            fixture.dir.join("code.sqlite").display()
        ))
        .await
        .unwrap();
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT state,outcome FROM script_code_dispatch")
                .fetch_all(&pool)
                .await
                .unwrap();
        pool.close().await;
        assert_eq!(rows.len(), usize::from(mode != 1));
        if mode != 1 {
            assert_eq!(rows[0].0, if mode == 4 { "terminal" } else { "associated" });
            assert_eq!(rows[0].1.is_some(), mode == 4);
            fixture.fault.store(0, Ordering::SeqCst);
            let rebuilt = fixture.adapter().await;
            assert!(
                rebuilt
                    .run(
                        input,
                        binding,
                        (Origin::Inline, "fault-call".into()),
                        CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                fixture.effects.load(Ordering::SeqCst),
                usize::from(mode >= 3)
            );
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn cancellation_and_revocation_join_the_original_owner_without_replaying_effects() {
    for revoke in [false, true] {
        let fixture = Fixture::new(true, "allow", one_effect()).await;
        let adapter = fixture.adapter().await;
        fixture.block.store(true, Ordering::SeqCst);
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        let call = tokio::spawn(async move {
            adapter
                .call_internal(
                    CodeInput::Execute {
                        source: one_effect().into(),
                    },
                    cancelled,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while fixture.effects.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if revoke {
            fixture
                .host
                .revoke(&fixture.principal, &fixture.record.id)
                .unwrap();
        } else {
            cancellation.cancel();
        }
        fixture.block.store(false, Ordering::SeqCst);
        fixture.release.notify_one();
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_ne!(output["status"], "succeeded", "{output}");
        assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
        fixture.finish().await;
    }
}

#[tokio::test]
async fn source_and_selected_schema_bounds_fail_before_any_effect() {
    let fixture = Fixture::new(true, "allow", one_effect()).await;
    let adapter = fixture.adapter().await;
    for source in [
        String::new(),
        "x".repeat(32769),
        "import x from 'ambient';".into(),
    ] {
        assert!(
            adapter
                .call_internal(CodeInput::Execute { source }, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(
        adapter
            .call_internal(
                CodeInput::Schemas {
                    targets: vec![ConcreteToolTarget::new("upstream", "effect").unwrap(); 9]
                },
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn authenticated_bearer_expiry_cancels_the_actual_child_before_the_next_effect() {
    use crate::access_auth::{AccessAuthHttpClient, OidcConfig, OidcVerifier};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use p256::ecdsa::{Signature, SigningKey, signature::Signer as _};
    let fixture = Fixture::new(true, "allow", partial()).await;
    let signing = SigningKey::from_bytes((&[7_u8; 32]).into()).unwrap();
    let point = signing.verifying_key().to_encoded_point(false);
    let jwks = json!({"keys":[{"kty":"EC","crv":"P-256","kid":"s8-key","use":"sig","alg":"ES256","x":URL_SAFE_NO_PAD.encode(point.x().unwrap()),"y":URL_SAFE_NO_PAD.encode(point.y().unwrap())}]});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let metadata = json!({"issuer":issuer,"jwks_uri":format!("{issuer}/jwks"),"access_token_signing_alg_values_supported":["ES256"]});
    let router = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || {
                let body = metadata.clone();
                async move { axum::Json(body) }
            }),
        )
        .route(
            "/jwks",
            axum::routing::get(move || {
                let body = jwks.clone();
                async move { axum::Json(body) }
            }),
        );
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let http = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
    });
    let config = OidcConfig::new(
        &issuer,
        "s8-client",
        None,
        format!("{issuer}/oidc/callback"),
        "s8-api",
        vec!["openid".into()],
        std::time::Duration::from_secs(60),
        true,
        true,
    )
    .unwrap();
    let verifier = OidcVerifier::discover_access(
        AccessAuthHttpClient::build(reqwest::Client::builder()).unwrap(),
        &issuer,
        "s8-api",
        None,
        config.transport_policy(),
    )
    .await
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let header = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&json!({"alg":"ES256","kid":"s8-key","typ":"at+jwt"})).unwrap());
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"iss":issuer,"sub":"code-owner","aud":"s8-api","iat":now,"exp":now+3,"scope":"waltr:read"})).unwrap());
    let message = format!("{header}.{claims}");
    let signature: Signature = signing.sign(message.as_bytes());
    let token = format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    let principal = verifier
        .validate_access_token(&token, &["waltr:read".into()])
        .await
        .unwrap();
    let old = fixture.adapter().await;
    fixture
        .host
        .grant(
            principal.clone(),
            &fixture.record,
            old.grant.profile.clone(),
        )
        .unwrap();
    let adapter = fixture
        .host
        .bind(
            &principal,
            &fixture.record,
            old.runtime.clone(),
            old.dispatcher.clone(),
        )
        .unwrap();
    fixture.block.store(true, Ordering::SeqCst);
    let call = tokio::spawn(async move {
        adapter
            .call_internal(
                CodeInput::Execute {
                    source: partial().into(),
                },
                CancellationToken::new(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while fixture.effects.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), principal.access_ended())
        .await
        .unwrap();
    fixture.block.store(false, Ordering::SeqCst);
    fixture.release.notify_one();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_ne!(output["status"], "succeeded", "{output}");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
    stop.cancel();
    http.await.unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn dropped_leased_waiter_leaves_service_owned_drain_and_never_replays_the_association() {
    let fixture = Fixture::new(true, "allow", one_effect()).await;
    let adapter = fixture.adapter().await;
    let input = CodeInput::Execute {
        source: one_effect().into(),
    };
    let binding = adapter.prepare(&input, Origin::Mcp).unwrap();
    fixture.block.store(true, Ordering::SeqCst);
    let run = adapter.clone();
    let reserved = binding.clone();
    let source = input.clone();
    let waiter = tokio::spawn(async move {
        run.run(
            source,
            reserved,
            (Origin::Mcp, "dropped-waiter".into()),
            CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while fixture.effects.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    fixture.block.store(false, Ordering::SeqCst);
    fixture.release.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        fixture.service.shutdown(),
    )
    .await
    .unwrap()
    .unwrap();
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        fixture.dir.join("code.sqlite").display()
    ))
    .await
    .unwrap();
    let states: Vec<(String,)> = sqlx::query_as("SELECT status FROM script_invocations")
        .fetch_all(&pool)
        .await
        .unwrap();
    let association: (String, Option<String>) =
        sqlx::query_as("SELECT state,outcome FROM script_code_dispatch")
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(states.len(), 1);
    assert!(
        matches!(states[0].0.as_str(), "interrupted" | "uncertain"),
        "{states:?}"
    );
    assert_eq!(association, ("associated".into(), None));
    assert!(
        adapter
            .run(
                input,
                binding,
                (Origin::Mcp, "dropped-waiter".into()),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn interrupted_normal_agent_sql_js_call_refuses_restart_approval_and_templates() {
    let fixture = Fixture::new(true, "ask", partial()).await;
    let runtime = fixture
        .sessions
        .resolve_runtime(&fixture.record.id)
        .await
        .unwrap();
    fixture.block.store(true, Ordering::SeqCst);
    let sessions = fixture.sessions.clone();
    let active_runtime = runtime.clone();
    let id = fixture.record.id.clone();
    let turn = tokio::spawn(async move {
        sessions
            .prompt(&active_runtime, id, "execute")
            .stream()
            .collect::<Vec<_>>()
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while fixture.effects.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    turn.abort();
    assert!(turn.await.unwrap_err().is_cancelled());
    fixture.block.store(false, Ordering::SeqCst);
    fixture.release.notify_one();
    fixture.service.shutdown().await.unwrap();
    let record = fixture
        .sessions
        .store()
        .load(&fixture.record.id)
        .await
        .unwrap();
    assert!(record.journal.iter().any(|event|matches!(event,SessionJournalEvent::MiddlewareStateUpdated{namespace,state}if namespace=="host-dispatch" && state["active"]==true)));
    let restarted = SessionCoordinator::new(fixture.sessions.store().clone());
    let factory = fixture
        .host
        .install((*fixture.host.helpers).clone(), fixture.principal.clone());
    let restarted = restarted
        .clone()
        .with_runtime_factory(factory.session_factory(restarted));
    assert!(restarted.resolve_runtime(&fixture.record.id).await.is_err());
    for input in [
        ResumeInput::Continue,
        ResumeInput::Prompt(
            "{{ code({\"action\":\"execute\",\"source\":\"return null;\"}) }}".into(),
        ),
    ] {
        let events = restarted
            .resume(&runtime, fixture.record.id.clone(), input)
            .stream()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(Result::is_err), "{events:?}");
    }
    assert!(
        fixture
            .broker
            .snapshot(&fixture.record.id)
            .pending
            .is_empty()
    );
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        fixture.dir.join("code.sqlite").display()
    ))
    .await
    .unwrap();
    let associations: i64 = sqlx::query_scalar("SELECT count(*) FROM script_code_dispatch")
        .fetch_one(&pool)
        .await
        .unwrap();
    let invocations: i64 = sqlx::query_scalar("SELECT count(*) FROM script_invocations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((associations, invocations), (1, 1));
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
    pool.close().await;
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn session_replacement_during_an_effect_denies_the_next_dispatch() {
    let fixture = Fixture::new(true, "allow", partial()).await;
    let adapter = fixture.adapter().await;
    fixture.block.store(true, Ordering::SeqCst);
    let call = tokio::spawn(async move {
        adapter
            .call_internal(
                CodeInput::Execute {
                    source: partial().into(),
                },
                CancellationToken::new(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while fixture.effects.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let other = SessionCoordinator::new(fixture.sessions.store().clone());
    other.delete(&fixture.record.id).await.unwrap();
    other
        .create(SessionCreateRequest::new(fixture.record.id.clone()))
        .await
        .unwrap();
    fixture.block.store(false, Ordering::SeqCst);
    fixture.release.notify_one();
    let result = call.await.unwrap().unwrap();
    assert_ne!(result["status"], "succeeded");
    assert_eq!(fixture.effects.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.second.load(Ordering::SeqCst), 0);
    other.shutdown().await;
    fixture.finish().await;
}
