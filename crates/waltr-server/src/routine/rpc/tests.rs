use super::catalog::Catalog as _;
use super::*;
use crate::{
    access_auth::*,
    persistence::{DatabaseConfig, Persistence},
    resource_catalog::speech::{ResolvedSpeechModel, SpeechSettings},
    routine::{
        adapters::{helper::CoordinatorHelperRunner, media::*},
        blob::{
            self, BlobStore,
            filesystem::{FilesystemBlobStore, FilesystemConfig},
        },
        history::InvocationBinding,
        media::{
            self,
            speech::GoogleSpeech,
            stream::{Pool, Streams},
        },
        ports::*,
        service::InvocationService,
    },
};
use crate::{
    agent_host::{
        AgentDefinitionCatalog,
        runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory,
    },
    auth::FileCredentialStore,
    interaction::InteractionBroker,
    permission::ServerPermissionPolicy,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, transport::Endpoint};
use waltr::grpc::waltr::{
    config::v1 as config,
    routine::v2::{self as p, routine_service_client::RoutineServiceClient},
};
use waltr_agent::*;
use waltr_script::{Engine, ScriptEngine};

#[allow(dead_code)]
#[path = "../../../../waltr-audio/tests/support/mod.rs"]
mod speech_fixture;
#[path = "../../../../waltr-script/tests/support/watchdog.rs"]
mod watchdog;

fn isolated(name: &str) -> bool {
    if std::env::var("WALTR_ROUTINE_RPC_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("routine::rpc::tests::{name}"),
            "--nocapture",
        ])
        .env("WALTR_ROUTINE_RPC_CHILD", name);
    let result = watchdog::run(command, Duration::from_secs(90), 65536);
    assert!(
        result.success && !result.killed && !result.overflow,
        "{}",
        String::from_utf8_lossy(&result.output)
    );
    true
}

struct Cold;
impl MediaDriver for Cold {
    fn start(&self) -> PortFuture<()> {
        Box::pin(async { Err(PortError::Denied) })
    }
    fn cancel(&self) {}
    fn finish(&self) -> PortFuture<()> {
        Box::pin(async { Err(PortError::Denied) })
    }
    fn join(&self) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
struct Bind {
    blobs: Arc<FilesystemBlobStore>,
    text: Arc<dyn crate::routine::text_results::TextResultStore>,
    speech: Arc<GoogleSpeech>,
    sessions: SessionCoordinator<MemorySessionStore>,
    factory: Arc<AgentSessionRuntimeFactory>,
    inference: InferenceSettings,
    workspace_principal: media::snapshot::WorkspacePool,
    workspace_process: media::snapshot::WorkspacePool,
    result_principal: ResultPool,
    result_process: ResultPool,
}
impl MediaBindingFactory for Bind {
    fn bind(
        &self,
        binding: InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<MediaBindings> {
        let helpers = CoordinatorHelperRunner::new(
            self.sessions.clone(),
            self.factory.clone(),
            self.inference.clone(),
            BTreeMap::new(),
            BTreeMap::new(),
            None,
            binding.principal().into(),
        )
        .map(|h| {
            Arc::new(h.with_input_models(BTreeMap::from([(
                (
                    self.inference.provider.key.clone(),
                    self.inference.provider.model_id.clone(),
                ),
                TransientModelCapabilities {
                    png: true,
                    jpeg: true,
                    webp: true,
                    pdf: true,
                    text: true,
                    max_body_bytes: 32 * 1024 * 1024,
                },
            )]))) as Arc<dyn HelperRunner>
        });
        let result = helpers.and_then(|helpers| {
            Ok(MediaBindings {
                driver: Arc::new(Cold),
                snapshot: false,
                principal: binding.principal().into(),
                streams: Streams::new(
                    Default::default(),
                    Pool::principal(),
                    Pool::process(),
                    CancellationToken::new(),
                )
                .map_err(|_| PortError::Invalid)?,
                inputs: BTreeMap::new(),
                outputs: BTreeMap::new(),
                uploads: BTreeMap::new(),
                speech: self.speech.clone(),
                helpers,
                context: None,
                blobs: self.blobs.clone(),
                text: self.text.clone(),
                attachments: TransientInputBudget::process(),
                workspace_principal: self.workspace_principal.clone(),
                workspace_process: self.workspace_process.clone(),
                result_principal: self.result_principal.clone(),
                result_process: self.result_process.clone(),
                leases: Arc::new(Mutex::new(Vec::new())),
            })
        });
        Box::pin(async move { result })
    }
}
struct Policy(Arc<Bind>);
impl StartPolicy for Policy {
    fn source_access(&self, _: String, source: Option<String>) -> PortFuture<()> {
        Box::pin(async move {
            if source.as_deref().is_none_or(|s| s == "allowed-source") {
                Ok(())
            } else {
                Err(PortError::Denied)
            }
        })
    }
    fn authorize(&self, request: StartRequest) -> PortFuture<AuthorizedStart> {
        let bind = self.0.clone();
        Box::pin(async move {
            if request
                .source_session
                .as_deref()
                .is_some_and(|s| s != "allowed-source")
            {
                return Err(PortError::Denied);
            }
            let factory: Arc<dyn HostFactory> = match request.transport {
                Some(pending) => Arc::new(MediaHostFactory {
                    bindings: pending.bind_factory(bind),
                }),
                None => Arc::new(Pure),
            };
            Ok(AuthorizedStart {
                factory,
                source_session: request.source_session,
            })
        })
    }
}
struct Never;
impl HostFactory for Never {
    fn preflight(
        &self,
        _: InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<PreparedHost> {
        Box::pin(async { Err(PortError::Denied) })
    }
}
struct Pure;
impl HostFactory for Pure {
    fn preflight(
        &self,
        _: InvocationBinding,
        definition: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<PreparedHost> {
        Box::pin(async move {
            if !definition.envelope().spec.capabilities.is_empty()
                || !definition.envelope().spec.ports.is_empty()
            {
                return Err(PortError::Denied);
            }
            Ok(PreparedHost {
                dispatcher: Arc::new(Pure),
                policy: Arc::new(Pure),
                reservation: Box::new(Pure),
                snapshot: None,
            })
        })
    }
}
impl waltr_script::HostDispatcher for Pure {
    fn dispatch(
        &self,
        _: waltr_script::HostRequest,
        _: waltr_script::HostContext,
    ) -> waltr_script::HostFuture {
        Box::pin(async { Err(waltr_script::Error::Denied) })
    }
    fn cleanup(&self, _: waltr_script::InvocationId) -> waltr_script::CleanupFuture {
        Box::pin(async { Ok(()) })
    }
}
impl OperationPolicy for Pure {
    fn before(
        &self,
        _: &waltr_script::HostRequest,
        _: &waltr_script::HostContext,
    ) -> PortFuture<serde_json::Value> {
        Box::pin(async { Err(PortError::Denied) })
    }
    fn project_result(
        &self,
        _: &str,
        _: &waltr_script::HostReply,
    ) -> std::result::Result<Option<serde_json::Value>, PortError> {
        Err(PortError::Denied)
    }
}
impl Reservation for Pure {
    fn finalize(&self, _: bool) -> PortFuture<()> {
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    running: Arc<RunningBarrier>,
    clock: Arc<TestClock>,
    attachments: Arc<attachments::AttachmentRegistry>,
    root: service::RoutineRpcService,
    catalog: Arc<catalog::MemoryCatalog>,
    engine: Arc<Engine>,
    blobs: Arc<FilesystemBlobStore>,
    sessions: SessionCoordinator<MemorySessionStore>,
    blob_registry: Arc<blobs::BlobRegistry>,
    persistence: Persistence,
    auth: AccessAuthenticator,
    token: String,
    foreign: String,
    cookie: String,
    url: String,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    provider_stop: CancellationToken,
    provider_task: tokio::task::JoinHandle<()>,
    issuer_stop: CancellationToken,
    issuer_task: tokio::task::JoinHandle<()>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    _sandbox: crate::sandbox::LocalSandbox,
    _speech: speech_fixture::Server,
}
#[derive(Default)]
struct RunningBarrier {
    enabled: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: CancellationToken,
}

#[tokio::test]
async fn routine_cancel_after_admission_before_engine_joins_cold_reservation() {
    if isolated("routine_cancel_after_admission_before_engine_joins_cold_reservation") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/dictate.yaml"
    )])
    .await;
    fixture
        .running
        .enabled
        .store(true, std::sync::atomic::Ordering::Release);
    let mut client = fixture.client().await;
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    drop(definitions);
    let opened = client
        .open_attachment(fixture.request(p::OpenAttachmentRequest {
            open: Some(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    revision: definition.revision,
                    ..selection("dictate", "dictate")
                }),
                inputs: vec![p::InputDeclaration {
                    port: "audio".into(),
                    format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })),
                }],
            }),
        }))
        .await
        .unwrap()
        .into_inner();
    let Some(p::open_attachment_response::Admission::Accepted(admitted)) = opened.admission else {
        panic!("admission rejected");
    };
    tokio::time::timeout(Duration::from_secs(5), fixture.running.entered.notified())
        .await
        .unwrap();
    let id = admitted.invocation.unwrap().invocation_id;
    client
        .cancel_invocation(fixture.request(p::CancelInvocationRequest {
            invocation_id: id.clone(),
        }))
        .await
        .unwrap();
    fixture.running.release.cancel();
    let mut output = client
        .watch_live_output(fixture.request(p::WatchLiveOutputRequest {
            credentials: admitted.credentials,
        }))
        .await
        .unwrap()
        .into_inner();
    let mut terminal = None;
    while let Some(frame) = output.message().await.unwrap() {
        if let Some(p::output_event::Event::Terminal(value)) = frame.output.unwrap().event {
            terminal = Some(value);
        }
    }
    assert!(terminal.unwrap().cleanup_complete);
    assert!(
        client
            .list_operations(fixture.request(p::ListOperationsRequest {
                invocation_id: id,
                after_cursor: 0,
                limit: 100
            }))
            .await
            .unwrap()
            .into_inner()
            .operations
            .is_empty()
    );
    fixture.close().await;
}
struct GatedStore {
    inner: Arc<dyn InvocationStore>,
    barrier: Arc<RunningBarrier>,
}
macro_rules! forward_store {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $output:ty;)+) => {
        $(fn $name(&self, $($arg: $ty),*) -> PortFuture<$output> {
            self.inner.$name($($arg),*)
        })+
    };
}
impl InvocationStore for GatedStore {
    forward_store! {
        admit(admission: crate::routine::history::Admission) -> ();
        admit_with_source(admission: crate::routine::history::Admission, source: Option<String>) -> ();
        admit_with_projection(admission: crate::routine::history::Admission, source: Option<String>, projection: Option<serde_json::Value>) -> ();
        dispatch_intent(intent: crate::routine::history::Operation) -> ();
        operation_done(id: String, status: crate::routine::history::OperationStatus, result: Option<serde_json::Value>, now: i64) -> ();
        terminal(id: String, status: crate::routine::history::Status, result: Option<serde_json::Value>, now: i64) -> crate::routine::history::Status;
        get(id: String) -> Option<crate::routine::history::Invocation>;
        list(after: i64, limit: u32) -> Vec<crate::routine::history::Invocation>;
        operations(invocation: String, after: i64, limit: u32) -> Vec<crate::routine::history::Operation>;
        reconcile(now: i64) -> u64;
    }
    fn running(&self, id: String, now: i64) -> PortFuture<()> {
        let inner = self.inner.clone();
        let barrier = self.barrier.clone();
        Box::pin(async move {
            if barrier.enabled.load(std::sync::atomic::Ordering::Acquire) {
                barrier.entered.notify_one();
                barrier.release.cancelled().await;
            }
            inner.running(id, now).await
        })
    }
}
#[derive(Default)]
struct TestClock {
    base: blob::MonotonicClock,
    advance: std::sync::atomic::AtomicU64,
}
impl blob::Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.base.now_ms() + self.advance.load(std::sync::atomic::Ordering::Acquire)
    }
}
impl Fixture {
    async fn new(programs: &[&str]) -> Self {
        let directory = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join("opencode")
            .join(format!("routine-rpc-{}", uuid::Uuid::new_v4()));
        let private = directory.join("private");
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let persistence = Persistence::initialize(
            DatabaseConfig::from_urls(
                &format!("sqlite:{}?mode=rwc", directory.join("db.sqlite").display()),
                "sqlite::memory:",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (auth, access_token, issuer, issuer_stop, issuer_task) =
            crate::test_support::composition_auth(&persistence, listener.local_addr().unwrap())
                .await;
        let cookie = format!("routine-owner-{}", uuid::Uuid::new_v4());
        let foreign = format!("routine-foreign-{}", uuid::Uuid::new_v4());
        let now = std::time::SystemTime::now();
        for (value, subject) in [(&cookie, "api-subject"), (&foreign, "other-subject")] {
            AccessAuthStore::new(&persistence)
                .insert_browser_session(&BrowserSession {
                    token_digest: BrowserSessionTokenDigest::from_token(value),
                    issuer: issuer.clone(),
                    client_id: "waltr-test-client".into(),
                    subject: subject.into(),
                    created_at: now,
                    expires_at: now + Duration::from_secs(3600),
                    last_seen_at: now,
                })
                .await
                .unwrap();
        }
        let provider_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_url = format!("http://{}/", provider_listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let provider_app = axum::Router::new().route("/", axum::routing::post(move |body: axum::body::Bytes| { let seen = seen.clone(); async move {
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let captured = body["messages"].as_array().unwrap().iter().any(|message| message["role"] == "tool");
            seen.lock().unwrap().push(body);
            let reply = if captured { serde_json::json!({"choices":[{"index":0,"delta":{"content":"accepted"},"finish_reason":"stop"}]}) } else { serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"inspection-capture","type":"function","function":{"name":"_helper_capture","arguments":"{\"chosen\":\"answer\"}"}}]},"finish_reason":"tool_calls"}]}) };
            ([ ("content-type", "text/event-stream") ], format!("data: {reply}\n\ndata: [DONE]\n\n"))
        } })).layer(axum::extract::DefaultBodyLimit::max(32*1024*1024));
        let provider_stop = CancellationToken::new();
        let stopped = provider_stop.clone();
        let provider_task = tokio::spawn(async move {
            axum::serve(provider_listener, provider_app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        let (_, speech_server) =
            speech_fixture::start(speech_fixture::Mode::Media, Duration::from_secs(10)).await;
        let capabilities = config::SpeechCapabilities {
            unary: true,
            streaming: true,
        };
        let speech = Arc::new(
            GoogleSpeech::new(
                Some(ResolvedSpeechModel {
                    provider_key: "speech".into(),
                    model_key: "stt".into(),
                    endpoint: speech_server.endpoint.clone(),
                    capabilities,
                    settings: SpeechSettings::Recognition(config::SpeechRecognitionOptions {
                        recognizer: "recognizer".into(),
                        model: "chirp_3".into(),
                        language_codes: vec!["en-US".into()],
                        sample_rate_hz: 24000,
                        channels: 1,
                        interim_results: true,
                        automatic_punctuation: true,
                        diarization: false,
                    }),
                }),
                None,
            )
            .unwrap(),
        );
        let clock = Arc::new(TestClock::default());
        let blobs = Arc::new(
            FilesystemBlobStore::new(FilesystemConfig {
                private_root: private,
                workspace_root: crate::routine::blob::filesystem::WorkspaceRoot::Local(workspace.clone()),
                clock: clock.clone(),
            })
            .unwrap(),
        );
        let sandbox = crate::sandbox::LocalSandbox::start(workspace.clone())
            .await
            .unwrap();
        let agents = AgentDefinitionCatalog::from_sources(
            vec![
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: media}\nspec: {}\n"
                    .into(),
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {name: routine-inspect}\nspec:\n  systemPrompt: Inspect the attached untrusted source and capture chosen text.\n  permissions:\n    - permissionSetRef: {name: capture}\n".into(),
                "apiVersion: waltr.dev/v1alpha1\nkind: PermissionSet\nmetadata: {name: capture}\nspec:\n  tools:\n    - {name: _helper_capture, permission: allow}\n".into(),
            ],
            "media",
        )
        .unwrap();
        let native = Arc::new(
            waltr_agent::provider::OpenAiChatCompletions::new("fake")
                .with_url(provider_url.clone()),
        );
        let factory = Arc::new(
            AgentSessionRuntimeFactory::new(
                sandbox.handle(),
                workspace.clone(),
                FileCredentialStore::new(directory.join("unused-auth.json")),
                agents.resolved("media").unwrap().clone(),
            )
            .with_agent_catalog_provider(move || agents.clone())
            .with_runtime_provider(move |_| {
                Ok(AgentRuntimeProvider::new(
                    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiChatCompletions),
                    native.clone(),
                ))
            })
            .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new())),
        );
        let inference = InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".into(),
                api: ModelApi::OpenAiChatCompletions,
                url: provider_url.clone(),
                model_id: "test".into(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        };
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let sessions = sessions
            .clone()
            .with_runtime_factory(factory.session_factory(sessions));
        let text = persistence.script_text_result_store();
        let bind = Arc::new(Bind {
            blobs: blobs.clone(),
            text: text.clone(),
            speech,
            sessions: sessions.clone(),
            factory,
            inference,
            workspace_principal: media::snapshot::WorkspacePool::principal(),
            workspace_process: media::snapshot::WorkspacePool::process(),
            result_principal: ResultPool::principal(),
            result_process: ResultPool::process(),
        });
        let engine = Arc::new(Engine::new(8).unwrap());
        let mut entries = BTreeMap::new();
        for program in programs {
            let definition = Definition::parse(program, engine.as_ref()).unwrap();
            let offers = definition
                .envelope()
                .spec
                .entrypoints
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        BTreeSet::from([
                            catalog::StartContext::Global,
                            catalog::StartContext::Session,
                        ]),
                    )
                })
                .collect();
            entries.insert(
                definition.envelope().name.clone(),
                catalog::CatalogEntry { definition, offers },
            );
        }
        let catalog = Arc::new(
            catalog::MemoryCatalog::new(
                entries,
                BTreeSet::from([
                    "media.execute".into(),
                    "helper.run".into(),
                    "helper.named.routine-inspect".into(),
                    "helper.capture.inspection".into(),
                ]),
            )
            .unwrap(),
        );
        let history = persistence.script_invocation_store();
        let running = Arc::new(RunningBarrier::default());
        let invocation = InvocationService::new(
            engine.clone(),
            Arc::new(GatedStore {
                inner: history.clone(),
                barrier: running.clone(),
            }),
            Arc::new(Never),
            Arc::new(SystemClock),
            8,
        )
        .unwrap();
        let attachments = attachments::AttachmentRegistry::new(history.clone(), blobs.clone());
        let blob_registry = blobs::BlobRegistry::new(
            blobs.clone(),
            clock.clone(),
            history.clone(),
            attachments.clone(),
        );
        let root = service::RoutineRpcService::new(
            api::ReadAdapter {
                service: invocation,
                history,
                catalog: catalog.clone(),
                policy: Arc::new(Policy(bind)),
                text,
            },
            attachments.clone(),
            blob_registry.clone(),
        );
        let url = format!("http://{}", listener.local_addr().unwrap());
        let routes = tonic::service::Routes::new(root.clone().into_server());
        let grpc = tower::ServiceBuilder::new()
            .layer(GrpcTransportLayer)
            .layer(tonic_web::GrpcWebLayer::new())
            .layer(GrpcAccessAuthLayer::new(auth.clone()))
            .service(routes);
        let app = axum::Router::new().fallback_service(grpc);
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            running,
            clock,
            attachments,
            root,
            catalog,
            engine,
            blobs,
            sessions,
            blob_registry,
            persistence,
            auth,
            token: access_token,
            foreign,
            cookie,
            url,
            stop,
            task,
            provider_stop,
            provider_task,
            issuer_stop,
            issuer_task,
            bodies,
            _sandbox: sandbox,
            _speech: speech_server,
        }
    }
    async fn client(&self) -> RoutineServiceClient<tonic::transport::Channel> {
        RoutineServiceClient::new(
            Endpoint::from_shared(self.url.clone())
                .unwrap()
                .connect()
                .await
                .unwrap(),
        )
    }
    fn request<T>(&self, value: T) -> Request<T> {
        let mut request = Request::new(value);
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        request
    }
    async fn web<M: prost::Message, R: prost::Message + Default>(
        &self,
        method: &str,
        value: M,
        foreign: bool,
    ) -> std::result::Result<R, tonic::Code> {
        let bytes = value.encode_to_vec();
        let mut frame = vec![0];
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend(bytes);
        let response = reqwest::Client::new()
            .post(format!(
                "{}/waltr.routine.v2.RoutineService/{method}",
                self.url
            ))
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .header(
                "cookie",
                format!(
                    "waltr-session-local={}",
                    if foreign { &self.foreign } else { &self.cookie }
                ),
            )
            .body(frame)
            .send()
            .await
            .unwrap();
        if let Some(status) = response.headers().get("grpc-status") {
            if status != "0" {
                return Err(tonic::Code::from_i32(
                    status.to_str().unwrap().parse().unwrap(),
                ));
            }
        }
        let body = response.bytes().await.unwrap();
        let mut offset = 0;
        let mut result = None;
        while offset + 5 <= body.len() {
            let flags = body[offset];
            let length =
                u32::from_be_bytes(body[offset + 1..offset + 5].try_into().unwrap()) as usize;
            offset += 5;
            let data = &body[offset..offset + length];
            offset += length;
            if flags & 128 != 0 {
                let trailer = std::str::from_utf8(data).unwrap();
                for line in trailer.lines() {
                    if let Some(code) = line.strip_prefix("grpc-status:") {
                        let code = code.trim().parse::<i32>().unwrap();
                        if code != 0 {
                            return Err(tonic::Code::from_i32(code));
                        }
                    }
                }
            } else {
                result = Some(R::decode(data).unwrap());
            }
        }
        result.ok_or(tonic::Code::Unknown)
    }
    async fn close(self) {
        self.root.shutdown().await.unwrap();
        self.engine.shutdown().await.unwrap();
        self.sessions.shutdown().await;
        self.blobs.shutdown().await.unwrap();
        self.stop.cancel();
        self.task.await.unwrap();
        self.provider_stop.cancel();
        self.provider_task.await.unwrap();
        self.issuer_stop.cancel();
        self.issuer_task.await.unwrap();
    }
}

#[tokio::test]
async fn routine_http_auth_loss_before_watch_and_pinned_history() {
    if isolated("routine_http_auth_loss_before_watch_and_pinned_history") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/dictate.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    assert_eq!(
        client
            .list_invocations(p::ListInvocationsRequest {
                after_cursor: 0,
                limit: 1,
                source_session: None
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let principal = fixture
        .auth
        .validate_api_access_token(&fixture.token, &[])
        .await
        .unwrap();
    assert_ne!(
        principal_id(&principal),
        principal.mcp_principal_id().as_str()
    );
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    drop(definitions);
    let selected = p::InvocationSelection {
        revision: definition.revision.clone(),
        ..selection("dictate", "dictate")
    };
    let bad: p::OpenAttachmentResponse = fixture
        .web(
            "OpenAttachment",
            p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(selected.clone()),
                    inputs: vec![],
                }),
            },
            false,
        )
        .await
        .unwrap();
    assert!(matches!(
        bad.admission,
        Some(p::open_attachment_response::Admission::Rejected(_))
    ));
    let audio = p::InputDeclaration {
        port: "audio".into(),
        format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
            sample_rate: 24000,
            channels: 1,
        })),
    };
    for inputs in [
        vec![audio.clone(), audio.clone()],
        vec![p::InputDeclaration {
            port: "unknown".into(),
            ..audio.clone()
        }],
        vec![p::InputDeclaration {
            port: "audio".into(),
            format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                sample_rate: 16000,
                channels: 1,
            })),
        }],
    ] {
        let response: p::OpenAttachmentResponse = fixture
            .web(
                "OpenAttachment",
                p::OpenAttachmentRequest {
                    open: Some(p::InteractionOpen {
                        selection: Some(selected.clone()),
                        inputs,
                    }),
                },
                false,
            )
            .await
            .unwrap();
        assert!(matches!(
            response.admission,
            Some(p::open_attachment_response::Admission::Rejected(_))
        ));
    }
    assert!(
        client
            .list_invocations(fixture.request(p::ListInvocationsRequest {
                after_cursor: 0,
                limit: 1,
                source_session: None
            }))
            .await
            .unwrap()
            .into_inner()
            .invocations
            .is_empty()
    );
    let response: p::OpenAttachmentResponse = fixture
        .web(
            "OpenAttachment",
            p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(selected),
                    inputs: vec![p::InputDeclaration {
                        port: "audio".into(),
                        format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })),
                    }],
                }),
            },
            false,
        )
        .await
        .unwrap();
    let p::open_attachment_response::Admission::Accepted(admission) = response.admission.unwrap()
    else {
        panic!("admission rejected");
    };
    let credentials = admission.credentials.unwrap();
    let invocation_id = admission.invocation.unwrap().invocation_id;
    assert_eq!(
        fixture
            .web::<_, p::GetInvocationResponse>(
                "GetInvocation",
                p::GetInvocationRequest {
                    invocation_id: invocation_id.clone()
                },
                true
            )
            .await
            .err()
            .unwrap(),
        tonic::Code::PermissionDenied
    );
    assert_eq!(
        fixture
            .web::<_, p::CancelInvocationResponse>(
                "CancelInvocation",
                p::CancelInvocationRequest {
                    invocation_id: invocation_id.clone()
                },
                true
            )
            .await
            .err()
            .unwrap(),
        tonic::Code::PermissionDenied
    );
    assert!(
        fixture
            .web::<_, p::ListInvocationsResponse>(
                "ListInvocations",
                p::ListInvocationsRequest {
                    after_cursor: 0,
                    limit: 1,
                    source_session: None
                },
                true
            )
            .await
            .unwrap()
            .invocations
            .is_empty()
    );
    let mut forged = credentials.clone();
    forged.secret[0] ^= 1;
    assert_eq!(
        client
            .renew_attachment(fixture.request(p::RenewAttachmentRequest {
                credentials: Some(forged)
            }))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    fixture
        .catalog
        .replace(BTreeMap::new(), BTreeSet::from(["media.execute".into()]))
        .unwrap();
    let pinned = client
        .get_invocation(fixture.request(p::GetInvocationRequest {
            invocation_id: invocation_id.clone(),
        }))
        .await
        .unwrap()
        .into_inner()
        .pinned_definition
        .unwrap();
    assert_eq!(pinned, definition);
    AccessAuthStore::new(&fixture.persistence)
        .revoke_browser_session(
            &BrowserSessionTokenDigest::from_token(&fixture.cookie),
            std::time::SystemTime::now(),
        )
        .await
        .unwrap();
    let record = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let record = client
                .get_invocation(fixture.request(p::GetInvocationRequest {
                    invocation_id: invocation_id.clone(),
                }))
                .await
                .unwrap()
                .into_inner()
                .invocation
                .unwrap();
            if record.status != p::InvocationStatus::Admitted as i32
                && record.status != p::InvocationStatus::Running as i32
            {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_ne!(record.status, p::InvocationStatus::Succeeded as i32);
    assert!(
        client
            .watch_live_output(fixture.request(p::WatchLiveOutputRequest {
                credentials: Some(credentials)
            }))
            .await
            .is_err()
    );
    fixture.close().await;
}

fn selection(name: &str, entrypoint: &str) -> p::InvocationSelection {
    p::InvocationSelection {
        routine: name.into(),
        entrypoint: entrypoint.into(),
        revision: String::new(),
        parameters: Some(p::StructuredValue {
            json: b"{}".to_vec(),
        }),
        correlation: "not-an-owner".into(),
        source_session: None,
    }
}
fn event(event: p::input_event::Event) -> p::InputEvent {
    p::InputEvent { event: Some(event) }
}

#[tokio::test]
async fn routine_manual_start_pins_catalog_and_rejects_exact_json_before_admission() {
    if isolated("routine_manual_start_pins_catalog_and_rejects_exact_json_before_admission") {
        return;
    }
    let program = "apiVersion: waltr.routine/v1\nkind: Routine\nname: pure\nspec:\n  source: 'export default async function () { return {completed: true}; }'\n  entrypoints:\n    run: {trigger: manual}\n  parameters: {schemaVersion: waltr.value/v1, schema: {type: object, properties: {}, required: []}}\n  result: {schemaVersion: waltr.value/v1, schema: {type: object, properties: {completed: {type: boolean}}, required: [completed]}}\n";
    let program = program.replace("name: pure\n", "name: pure\ndescription: Pure JS\n");
    let fixture = Fixture::new(&[&program]).await;
    let mut client = fixture.client().await;
    let mut watch = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let first = watch.message().await.unwrap().unwrap();
    let definition = first.definitions[0].clone();
    for invalid in [
        br#"{"a":1,"a":2}"#.to_vec(),
        b"{} {}".to_vec(),
        br#"{"n":9007199254740993}"#.to_vec(),
    ] {
        let mut selected = selection("pure", "run");
        selected.revision = definition.revision.clone();
        selected.parameters = Some(p::StructuredValue { json: invalid });
        assert_eq!(
            client
                .start_invocation(fixture.request(p::StartInvocationRequest {
                    selection: Some(selected)
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
    }
    assert!(
        client
            .list_invocations(fixture.request(p::ListInvocationsRequest {
                after_cursor: 0,
                limit: 100,
                source_session: None
            }))
            .await
            .unwrap()
            .into_inner()
            .invocations
            .is_empty()
    );
    let mut selected = selection("pure", "run");
    selected.revision = definition.revision.clone();
    selected.source_session = Some("allowed-source".into());
    let id = client
        .start_invocation(fixture.request(p::StartInvocationRequest {
            selection: Some(selected),
        }))
        .await
        .unwrap()
        .into_inner()
        .invocation
        .unwrap()
        .id;
    let record = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = client
                .get_invocation(fixture.request(p::GetInvocationRequest {
                    invocation_id: id.clone(),
                }))
                .await
                .unwrap()
                .into_inner();
            if value.invocation.as_ref().unwrap().status == p::InvocationStatus::Succeeded as i32 {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        record.invocation.unwrap().source_session.as_deref(),
        Some("allowed-source")
    );
    assert_eq!(record.pinned_definition.unwrap(), definition);
    fixture
        .catalog
        .replace(BTreeMap::new(), BTreeSet::new())
        .unwrap();
    let replacement = watch.message().await.unwrap().unwrap();
    assert!(replacement.definitions.is_empty());
    assert_ne!(replacement.catalog_revision, first.catalog_revision);
    drop(watch);
    assert_eq!(
        client
            .get_invocation(fixture.request(p::GetInvocationRequest { invocation_id: id }))
            .await
            .unwrap()
            .into_inner()
            .pinned_definition
            .unwrap(),
        definition
    );
    fixture.close().await;
}

struct WebOutput {
    response: reqwest::Response,
    bytes: Vec<u8>,
    ended: bool,
}
impl WebOutput {
    async fn next(&mut self) -> Option<p::WatchLiveOutputResponse> {
        loop {
            if self.bytes.len() >= 5 {
                let length = u32::from_be_bytes(self.bytes[1..5].try_into().unwrap()) as usize;
                if self.bytes.len() >= length + 5 {
                    let frame: Vec<_> = self.bytes.drain(..length + 5).collect();
                    if frame[0] & 128 != 0 {
                        assert!(
                            std::str::from_utf8(&frame[5..])
                                .unwrap()
                                .contains("grpc-status:0")
                                || std::str::from_utf8(&frame[5..])
                                    .unwrap()
                                    .contains("grpc-status: 0")
                        );
                        self.ended = true;
                        continue;
                    }
                    use prost::Message as _;
                    return Some(p::WatchLiveOutputResponse::decode(&frame[5..]).unwrap());
                }
            }
            match self.response.chunk().await.unwrap() {
                Some(bytes) => self.bytes.extend(bytes),
                None => {
                    assert!(self.ended && self.bytes.is_empty());
                    return None;
                }
            }
        }
    }
}

#[tokio::test]
async fn routine_cold_queue_cancel_bypasses_backpressure_without_admission() {
    if isolated("routine_cold_queue_cancel_bypasses_backpressure_without_admission") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/dictate.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    let principal = fixture
        .auth
        .validate_api_access_token(&fixture.token, &[])
        .await
        .unwrap();
    let owner = principal_id(&principal);
    let catalog = fixture.catalog.subscribe(&owner).unwrap();
    let definition = catalog.borrow().entries["dictate"].definition.clone();
    let (pending, slots) = fixture
        .attachments
        .prepare(
            owner,
            definition,
            vec![p::InputDeclaration {
                port: "audio".into(),
                format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })),
            }],
        )
        .await
        .unwrap();
    pending
        .send(
            0,
            vec![event(p::input_event::Event::Open(p::StreamOpen {
                port: "audio".into(),
                stream_id: "capture".into(),
                format: Some(p::stream_open::Format::Pcm16le(p::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })),
            }))],
        )
        .await
        .unwrap();
    for sequence in 1..8 {
        pending
            .send(
                sequence,
                vec![event(p::input_event::Event::Chunk(p::StreamChunk {
                    stream_id: "capture".into(),
                    sequence: sequence - 1,
                    payload: Some(p::stream_chunk::Payload::Audio(vec![0, 0])),
                }))],
            )
            .await
            .unwrap();
    }
    let blocked = pending.send(
        8,
        vec![event(p::input_event::Event::Chunk(p::StreamChunk {
            stream_id: "capture".into(),
            sequence: 7,
            payload: Some(p::stream_chunk::Payload::Audio(vec![0, 0])),
        }))],
    );
    tokio::pin!(blocked);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut blocked)
            .await
            .is_err()
    );
    pending.control.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), &mut blocked)
            .await
            .unwrap()
            .unwrap_err()
            .code(),
        tonic::Code::Cancelled
    );
    assert!(
        client
            .list_invocations(fixture.request(p::ListInvocationsRequest {
                after_cursor: 0,
                limit: 100,
                source_session: None
            }))
            .await
            .unwrap()
            .into_inner()
            .invocations
            .is_empty()
    );
    drop(slots);
    fixture.close().await;
}

#[tokio::test]
async fn routine_browser_server_stream_live_output_and_independent_finish_control() {
    if isolated("routine_browser_server_stream_live_output_and_independent_finish_control") {
        return;
    }
    use prost::Message as _;
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/dictate.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    drop(definitions);
    let open: p::OpenAttachmentResponse = fixture
        .web(
            "OpenAttachment",
            p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(p::InvocationSelection {
                        revision: definition.revision,
                        ..selection("dictate", "dictate")
                    }),
                    inputs: vec![p::InputDeclaration {
                        port: "audio".into(),
                        format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                            sample_rate: 24000,
                            channels: 1,
                        })),
                    }],
                }),
            },
            false,
        )
        .await
        .unwrap();
    let Some(p::open_attachment_response::Admission::Accepted(admitted)) = open.admission else {
        panic!("browser admission rejected");
    };
    let credentials = admitted.credentials.unwrap();
    let renewed: p::RenewAttachmentResponse = fixture
        .web(
            "RenewAttachment",
            p::RenewAttachmentRequest {
                credentials: Some(credentials.clone()),
            },
            false,
        )
        .await
        .unwrap();
    assert!(renewed.lease_expires_ms <= renewed.absolute_expires_ms);
    let body = p::WatchLiveOutputRequest {
        credentials: Some(credentials.clone()),
    }
    .encode_to_vec();
    let mut frame = vec![0];
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend(body);
    let response = reqwest::Client::new()
        .post(format!(
            "{}/waltr.routine.v2.RoutineService/WatchLiveOutput",
            fixture.url
        ))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header("cookie", format!("waltr-session-local={}", fixture.cookie))
        .body(frame)
        .send()
        .await
        .unwrap();
    let mut output = WebOutput {
        response,
        bytes: Vec::new(),
        ended: false,
    };
    assert!(matches!(
        output.next().await.unwrap().output.unwrap().event,
        Some(p::output_event::Event::Accepted(_))
    ));
    assert_eq!(
        fixture
            .web::<_, p::SendInputBatchResponse>(
                "SendInputBatch",
                p::SendInputBatchRequest {
                    credentials: Some(credentials.clone()),
                    sequence: 0,
                    events: vec![
                        event(p::input_event::Event::Open(p::StreamOpen {
                            port: "audio".into(),
                            stream_id: "capture".into(),
                            format: Some(p::stream_open::Format::Pcm16le(p::Pcm16Le {
                                sample_rate: 24000,
                                channels: 1
                            }))
                        })),
                        event(p::input_event::Event::Chunk(p::StreamChunk {
                            stream_id: "capture".into(),
                            sequence: 0,
                            payload: Some(p::stream_chunk::Payload::Audio(vec![0, 0, 1, 0]))
                        }))
                    ]
                },
                false
            )
            .await
            .unwrap()
            .accepted_sequence,
        0
    );
    loop {
        let value = tokio::time::timeout(Duration::from_secs(5), output.next())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            value.output.unwrap().event,
            Some(p::output_event::Event::Chunk(_))
        ) {
            break;
        }
    }
    fixture
        .web::<_, p::SendInputBatchResponse>(
            "SendInputBatch",
            p::SendInputBatchRequest {
                credentials: Some(credentials.clone()),
                sequence: 1,
                events: vec![event(p::input_event::Event::Close(p::StreamClose {
                    stream_id: "capture".into(),
                    next_sequence: 1,
                }))],
            },
            false,
        )
        .await
        .unwrap();
    fixture
        .web::<_, p::CloseAttachmentResponse>(
            "CloseAttachment",
            p::CloseAttachmentRequest {
                credentials: Some(credentials),
                mode: p::AttachmentCloseMode::FinishInput as i32,
            },
            false,
        )
        .await
        .unwrap();
    let mut terminals = 0;
    while let Some(value) = tokio::time::timeout(Duration::from_secs(5), output.next())
        .await
        .unwrap()
    {
        if let Some(p::output_event::Event::Terminal(terminal)) = value.output.unwrap().event {
            terminals += 1;
            assert!(terminal.cleanup_complete);
            assert_eq!(
                terminal.status,
                p::InvocationStatus::Succeeded as i32,
                "terminal: {terminal:?}"
            );
        }
    }
    assert_eq!(terminals, 1);
    fixture.close().await;
}

#[tokio::test]
async fn routine_native_live_output_explicit_eof_and_drop_cancellation() {
    if isolated("routine_native_live_output_explicit_eof_and_drop_cancellation") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/dictate.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    drop(definitions);
    for cancel in [false, true] {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(p::InteractRequest {
            frame: Some(p::interact_request::Frame::Open(p::InteractionOpen {
                selection: Some(p::InvocationSelection {
                    revision: definition.revision.clone(),
                    ..selection("dictate", "dictate")
                }),
                inputs: vec![p::InputDeclaration {
                    port: "audio".into(),
                    format: Some(p::input_declaration::Format::Pcm16le(p::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })),
                }],
            })),
        })
        .await
        .unwrap();
        let mut output = client
            .interact(fixture.request(tokio_stream::wrappers::ReceiverStream::new(rx)))
            .await
            .unwrap()
            .into_inner();
        let first = output
            .message()
            .await
            .unwrap()
            .unwrap()
            .output
            .unwrap()
            .event
            .unwrap();
        let p::output_event::Event::Accepted(accepted) = first else {
            panic!("native admission rejected");
        };
        for input in [
            event(p::input_event::Event::Open(p::StreamOpen {
                port: "audio".into(),
                stream_id: "capture".into(),
                format: Some(p::stream_open::Format::Pcm16le(p::Pcm16Le {
                    sample_rate: 24000,
                    channels: 1,
                })),
            })),
            event(p::input_event::Event::Chunk(p::StreamChunk {
                stream_id: "capture".into(),
                sequence: 0,
                payload: Some(p::stream_chunk::Payload::Audio(vec![0, 0, 1, 0])),
            })),
        ] {
            tx.send(p::InteractRequest {
                frame: Some(p::interact_request::Frame::Input(input)),
            })
            .await
            .unwrap();
        }
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Some(p::output_event::Event::Chunk(chunk)) = frame.output.unwrap().event {
                assert!(matches!(
                    chunk.payload,
                    Some(p::stream_chunk::Payload::Text(_))
                ));
                break;
            }
        }
        if cancel {
            drop(output);
            drop(tx);
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let row = client
                        .get_invocation(fixture.request(p::GetInvocationRequest {
                            invocation_id: accepted.invocation_id.clone(),
                        }))
                        .await
                        .unwrap()
                        .into_inner()
                        .invocation
                        .unwrap();
                    if row.status != 1 && row.status != 2 {
                        assert_ne!(row.status, p::InvocationStatus::Succeeded as i32);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            for input in [
                event(p::input_event::Event::Close(p::StreamClose {
                    stream_id: "capture".into(),
                    next_sequence: 1,
                })),
                event(p::input_event::Event::Finish(p::InputFinish {})),
            ] {
                tx.send(p::InteractRequest {
                    frame: Some(p::interact_request::Frame::Input(input)),
                })
                .await
                .unwrap();
            }
            drop(tx);
            let mut terminal = 0;
            let mut closed = false;
            while let Some(frame) = tokio::time::timeout(Duration::from_secs(5), output.message())
                .await
                .unwrap()
                .unwrap()
            {
                match frame.output.unwrap().event.unwrap() {
                    p::output_event::Event::Close(_) => closed = true,
                    p::output_event::Event::Terminal(value) => {
                        terminal += 1;
                        assert!(closed && value.cleanup_complete);
                        assert_eq!(value.status, p::InvocationStatus::Succeeded as i32);
                    }
                    _ => {}
                }
            }
            assert_eq!(terminal, 1);
        }
    }
    fixture.close().await;
}

#[tokio::test]
async fn routine_http_snapshot_resume_mutation_order_and_atomic_get() {
    if isolated("routine_http_snapshot_resume_mutation_order_and_atomic_get") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/dictate.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    let principal = fixture
        .auth
        .validate_api_access_token(&fixture.token, &[])
        .await
        .unwrap();
    let owner = principal_id(&principal);
    let store = fixture.persistence.script_invocation_store();
    let catalog = fixture.catalog.subscribe(&owner).unwrap();
    let snapshot = catalog.borrow().clone();
    let entry = snapshot.entries.get("dictate").unwrap();
    let definition = &entry.definition;
    for (id, principal, source) in [
        ("foreign-first", "other", "allowed-source"),
        ("owner-first", owner.as_str(), "allowed-source"),
        ("other-source", owner.as_str(), "different-source"),
        ("owner-second", owner.as_str(), "allowed-source"),
    ] {
        store
            .admit_with_projection(
                crate::routine::history::Admission {
                    id: id.into(),
                    revision: crate::routine::history::Revision {
                        id: definition.revision().into(),
                        api_version: definition.envelope().api_version.clone(),
                        engine: definition.engine().into(),
                        envelope: definition.canonical().into(),
                        name: "dictate".into(),
                    },
                    caller: Caller {
                        principal: principal.into(),
                        origin: crate::routine::history::Origin::Interactive,
                        ownership: crate::routine::history::Ownership::Leased,
                        correlation: "not-source".into(),
                    },
                    entrypoint: "dictate".into(),
                    snapshot: None,
                    admitted_ms: 100,
                },
                Some(source.into()),
                Some(
                    serde_json::to_value(catalog::PublicDefinition::capture(entry).unwrap())
                        .unwrap(),
                ),
            )
            .await
            .unwrap();
    }
    let list = client
        .list_invocations(fixture.request(p::ListInvocationsRequest {
            after_cursor: 0,
            limit: 1,
            source_session: Some("allowed-source".into()),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.invocations[0].id, "owner-first");
    let mut watch = client
        .watch_invocations(fixture.request(p::WatchInvocationsRequest {
            after_cursor: String::new(),
            limit: 1,
            source_session: Some("allowed-source".into()),
        }))
        .await
        .unwrap()
        .into_inner();
    let first = watch.message().await.unwrap().unwrap();
    assert!(!first.snapshot_complete);
    assert_eq!(first.invocations.len(), 1);
    drop(watch);
    store.running("owner-first".into(), 100).await.unwrap();
    let mut resumed = client
        .watch_invocations(fixture.request(p::WatchInvocationsRequest {
            after_cursor: first.cursor,
            limit: 1,
            source_session: Some("allowed-source".into()),
        }))
        .await
        .unwrap()
        .into_inner();
    let final_snapshot = resumed.message().await.unwrap().unwrap();
    assert!(final_snapshot.snapshot_complete);
    assert_eq!(final_snapshot.invocations[0].id, "owner-second");
    assert_eq!(final_snapshot.cursor, first.snapshot_high_water);
    let changed = resumed.message().await.unwrap().unwrap();
    assert_eq!(changed.invocations[0].id, "owner-first");
    assert_eq!(
        changed.invocations[0].status,
        p::InvocationStatus::Running as i32
    );
    assert!(
        changed.invocations[0].observation_revision > first.invocations[0].observation_revision
    );
    drop(resumed);
    let get = client
        .get_invocation(fixture.request(p::GetInvocationRequest {
            invocation_id: "owner-first".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(get.invocation.unwrap(), changed.invocations[0]);
    assert!(
        client
            .watch_invocations(fixture.request(p::WatchInvocationsRequest {
                after_cursor: changed.cursor.clone(),
                limit: 1,
                source_session: None
            }))
            .await
            .is_err()
    );
    let key = crate::routine::observation::ObservationStore::cursor_key(store.as_ref())
        .await
        .unwrap();
    assert!(
        cursor::Codec::new(key)
            .decode(
                &crate::routine::observation::ObservationScope {
                    principal: owner,
                    source_session: Some("allowed-source".into())
                },
                &changed.cursor
            )
            .is_ok()
    );
    assert_eq!(
        fixture
            .web::<_, p::GetInvocationResponse>(
                "GetInvocation",
                p::GetInvocationRequest {
                    invocation_id: "owner-first".into()
                },
                true
            )
            .await
            .err()
            .unwrap(),
        tonic::Code::PermissionDenied
    );
    store
        .terminal(
            "owner-first".into(),
            crate::routine::history::Status::Interrupted,
            None,
            100,
        )
        .await
        .unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn routine_http_open_upload_native_helpers_and_durable_text() {
    if isolated("routine_http_open_upload_native_helpers_and_durable_text") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/native-inspect.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    assert_eq!(definition.name, "native-inspect");
    drop(definitions);
    for (bytes, mime, purpose, marker) in [
        (
            include_bytes!("../../../tests/fixtures/routines/images/rgba.png").to_vec(),
            "image/png",
            p::binary_input_declaration::Purpose::Image(1),
            "data:image/png;base64",
        ),
        (
            b"%PDF-1.7\nnative opaque PDF\n%%EOF".to_vec(),
            "application/pdf",
            p::binary_input_declaration::Purpose::File(p::FilePurpose {}),
            "application/pdf",
        ),
        (
            "native café".as_bytes().to_vec(),
            "text/plain",
            p::binary_input_declaration::Purpose::File(p::FilePurpose {}),
            "native café",
        ),
    ] {
        let metadata = p::BinaryInputDeclaration {
            display_name: "source".into(),
            content_type: mime.into(),
            declared_bytes: Some(bytes.len() as u64),
            expected_sha256: None,
            purpose: Some(purpose),
        };
        let mut selected = selection("native-inspect", "inspect");
        selected.revision = definition.revision.clone();
        let response = client
            .open_attachment(fixture.request(p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(selected),
                    inputs: vec![p::InputDeclaration {
                        port: "source".into(),
                        format: Some(p::input_declaration::Format::Binary(metadata.clone())),
                    }],
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        let admitted = match response.admission.unwrap() {
            p::open_attachment_response::Admission::Accepted(value) => value,
            p::open_attachment_response::Admission::Rejected(value) => {
                panic!("admission rejected {} {}", value.code, value.reason)
            }
        };
        let credentials = admitted.credentials.unwrap();
        let invocation_id = admitted.invocation.unwrap().invocation_id;
        let mut output = client
            .watch_live_output(fixture.request(p::WatchLiveOutputRequest {
                credentials: Some(credentials.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            output
                .message()
                .await
                .unwrap()
                .unwrap()
                .output
                .unwrap()
                .event,
            Some(p::output_event::Event::Accepted(_))
        ));
        let purpose = metadata.purpose.map(|p| match p {
            p::binary_input_declaration::Purpose::File(v) => {
                p::begin_blob_upload_request::Purpose::File(v)
            }
            p::binary_input_declaration::Purpose::Image(v) => {
                p::begin_blob_upload_request::Purpose::Image(v)
            }
        });
        for (port, size) in [("unknown", metadata.declared_bytes), ("source", None)] {
            assert_eq!(
                client
                    .begin_blob_upload(fixture.request(p::BeginBlobUploadRequest {
                        invocation_id: invocation_id.clone(),
                        port: port.into(),
                        display_name: metadata.display_name.clone(),
                        content_type: metadata.content_type.clone(),
                        declared_bytes: size,
                        expected_sha256: metadata.expected_sha256.clone(),
                        purpose: purpose.clone()
                    }))
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::InvalidArgument
            );
        }
        let begin = client
            .begin_blob_upload(fixture.request(p::BeginBlobUploadRequest {
                invocation_id: invocation_id.clone(),
                port: "source".into(),
                display_name: metadata.display_name,
                content_type: metadata.content_type,
                declared_bytes: metadata.declared_bytes,
                expected_sha256: metadata.expected_sha256,
                purpose,
            }))
            .await
            .unwrap()
            .into_inner();
        let byte_count = bytes.len() as u64;
        client
            .append_blob_upload(fixture.request(p::AppendBlobUploadRequest {
                invocation_id: invocation_id.clone(),
                upload_id: begin.upload_id.clone(),
                sequence: 0,
                offset: 0,
                data: bytes,
            }))
            .await
            .unwrap();
        let committed = client
            .commit_blob_upload(fixture.request(p::CommitBlobUploadRequest {
                invocation_id: invocation_id.clone(),
                upload_id: begin.upload_id,
                next_sequence: 1,
                bytes: byte_count,
            }))
            .await
            .unwrap()
            .into_inner();
        client
            .send_input_batch(fixture.request(p::SendInputBatchRequest {
                credentials: Some(credentials.clone()),
                sequence: 0,
                events: vec![
                    event(p::input_event::Event::Blob(p::BlobInput {
                        port: "source".into(),
                        blob_id: committed.blob.unwrap().blob_id,
                    })),
                    event(p::input_event::Event::Finish(p::InputFinish {})),
                ],
            }))
            .await
            .unwrap();
        let mut text = String::new();
        let mut terminal = None;
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(15), output.message())
            .await
            .unwrap()
            .unwrap()
        {
            match frame.output.unwrap().event.unwrap() {
                p::output_event::Event::Chunk(chunk) => {
                    if let Some(p::stream_chunk::Payload::Text(value)) = chunk.payload {
                        assert!(value.committed);
                        text.push_str(&value.text);
                    }
                }
                p::output_event::Event::Terminal(value) => {
                    assert!(terminal.is_none());
                    terminal = Some(value);
                }
                _ => {}
            }
        }
        assert_eq!(text, "answer");
        let terminal = terminal.unwrap();
        assert!(terminal.cleanup_complete);
        assert_eq!(terminal.status, p::InvocationStatus::Succeeded as i32);
        let history = client
            .get_invocation(fixture.request(p::GetInvocationRequest {
                invocation_id: invocation_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let record = history.invocation.unwrap();
        assert_eq!(record.text_results.len(), 1);
        assert_eq!(record.helpers.len(), 1);
        let read = client
            .read_text_result(fixture.request(p::ReadTextResultRequest {
                invocation_id,
                result_id: record.text_results[0].id.clone(),
                offset: 0,
                max_bytes: 65536,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(read.data, b"answer");
        assert!(read.eof);
        assert!(
            fixture
                .bodies
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .to_string()
                .contains(marker)
        );
    }
    fixture.close().await;
}

#[tokio::test]
async fn routine_binary_output_survives_teardown_but_history_and_logout_do_not_grant_reads() {
    if isolated("routine_binary_output_survives_teardown_but_history_and_logout_do_not_grant_reads")
    {
        return;
    }
    binary_output_control(false, false).await;
}
#[tokio::test]
async fn routine_blob_shutdown_drains_open_after_grant_lookup_and_dropped_waiters() {
    if isolated("routine_blob_shutdown_drains_open_after_grant_lookup_and_dropped_waiters") {
        return;
    }
    binary_output_control(true, false).await;
}
#[tokio::test]
async fn routine_abandoned_open_capacity_recovers_without_shutdown() {
    if isolated("routine_abandoned_open_capacity_recovers_without_shutdown") {
        return;
    }
    binary_output_control(false, true).await;
}
async fn binary_output_control(shutdown_race: bool, capacity_race: bool) {
    let program = include_str!("../../../tests/fixtures/routines/native-inspect.yaml")
        .replace("const helper = await host.call('helper.run', {selection: {kind: 'named', name: 'routine-inspect'}, task: 'Inspect the attached source. Capture the answer in chosen.', capture: 'inspection', maxTurns: 3, deadlineMs: 20000, responseBytes: 65536, completionBytes: 4194304}, null, null, source);", "await host.call('media.lease', {output: 'answer'}, null, null, source);")
        .replace("const answer = await host.call('media.select', {field: '/chosen'}, null, null, helper);", "")
        .replace("await host.call('media.publish', {name: 'answer'}, null, null, answer);", "")
        .replace("await host.call('media.emit', {output: 'answer'}, null, null, answer);", "")
        .replace("media: text, required: true, maxBytes: 1048576, formats: [utf8]", "media: file, required: true, maxBytes: 1048576, formats: [blob]");
    let fixture = Fixture::new(&[&program]).await;
    let mut client = fixture.client().await;
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    drop(definitions);
    for mode in if capacity_race {
        vec![5, 0]
    } else if shutdown_race {
        vec![4]
    } else {
        vec![0, 3, 2, 1]
    } {
        let logout = mode == 2;
        let bytes = b"leased bytes after script teardown".to_vec();
        let metadata = p::BinaryInputDeclaration {
            display_name: "source".into(),
            content_type: "application/octet-stream".into(),
            declared_bytes: Some(bytes.len() as u64),
            expected_sha256: None,
            purpose: Some(p::binary_input_declaration::Purpose::File(
                p::FilePurpose {},
            )),
        };
        let open = client
            .open_attachment(fixture.request(p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(p::InvocationSelection {
                        revision: definition.revision.clone(),
                        ..selection("native-inspect", "inspect")
                    }),
                    inputs: vec![p::InputDeclaration {
                        port: "source".into(),
                        format: Some(p::input_declaration::Format::Binary(metadata.clone())),
                    }],
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        let Some(p::open_attachment_response::Admission::Accepted(admitted)) = open.admission
        else {
            panic!("binary admission rejected");
        };
        let credentials = admitted.credentials.unwrap();
        let invocation = admitted.invocation.unwrap().invocation_id;
        let mut output = client
            .watch_live_output(fixture.request(p::WatchLiveOutputRequest {
                credentials: Some(credentials.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        output.message().await.unwrap().unwrap();
        let begin = client
            .begin_blob_upload(fixture.request(p::BeginBlobUploadRequest {
                invocation_id: invocation.clone(),
                port: "source".into(),
                display_name: metadata.display_name,
                content_type: metadata.content_type,
                declared_bytes: metadata.declared_bytes,
                expected_sha256: None,
                purpose: Some(p::begin_blob_upload_request::Purpose::File(
                    p::FilePurpose {},
                )),
            }))
            .await
            .unwrap()
            .into_inner();
        client
            .append_blob_upload(fixture.request(p::AppendBlobUploadRequest {
                invocation_id: invocation.clone(),
                upload_id: begin.upload_id.clone(),
                sequence: 0,
                offset: 0,
                data: bytes.clone(),
            }))
            .await
            .unwrap();
        let commit = client
            .commit_blob_upload(fixture.request(p::CommitBlobUploadRequest {
                invocation_id: invocation.clone(),
                upload_id: begin.upload_id,
                next_sequence: 1,
                bytes: bytes.len() as u64,
            }))
            .await
            .unwrap()
            .into_inner();
        client
            .send_input_batch(fixture.request(p::SendInputBatchRequest {
                credentials: Some(credentials),
                sequence: 0,
                events: vec![
                    event(p::input_event::Event::Blob(p::BlobInput {
                        port: "source".into(),
                        blob_id: commit.blob.unwrap().blob_id,
                    })),
                    event(p::input_event::Event::Finish(p::InputFinish {})),
                ],
            }))
            .await
            .unwrap();
        let mut grant = None;
        let mut terminal = false;
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(5), output.message())
            .await
            .unwrap()
            .unwrap()
        {
            match frame.output.unwrap().event.unwrap() {
                p::output_event::Event::Blob(blob) => grant = blob.output_grant,
                p::output_event::Event::Terminal(value) => {
                    assert_eq!(value.status, p::InvocationStatus::Succeeded as i32);
                    assert!(value.cleanup_complete);
                    terminal = true;
                }
                _ => {}
            }
        }
        assert!(terminal);
        let grant = grant.unwrap();
        let history = client
            .get_invocation(fixture.request(p::GetInvocationRequest {
                invocation_id: invocation.clone(),
            }))
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        assert_eq!(history.binary_results.len(), 1);
        assert_eq!(
            client
                .open_blob_read(fixture.request(p::OpenBlobReadRequest {
                    invocation_id: invocation.clone(),
                    access: Some(p::open_blob_read_request::Access::InputBlobId(
                        history.binary_results[0].blob_id.clone()
                    ))
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        let mut forged = grant.clone();
        forged.secret[0] ^= 1;
        assert_eq!(
            client
                .open_blob_read(fixture.request(p::OpenBlobReadRequest {
                    invocation_id: invocation.clone(),
                    access: Some(p::open_blob_read_request::Access::OutputGrant(forged))
                }))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        let request = p::OpenBlobReadRequest {
            invocation_id: invocation.clone(),
            access: Some(p::open_blob_read_request::Access::OutputGrant(
                grant.clone(),
            )),
        };
        assert_eq!(
            fixture
                .web::<_, p::OpenBlobReadResponse>("OpenBlobRead", request.clone(), true)
                .await
                .err()
                .unwrap(),
            tonic::Code::PermissionDenied
        );
        if mode == 5 {
            let auth = fixture
                .auth
                .validate_api_access_token(&fixture.token, &[])
                .await
                .unwrap();
            fixture
                .blob_registry
                .test_reclaim_abandoned_opens(principal_id(&auth), auth, request)
                .await;
            continue;
        }
        if shutdown_race {
            let auth = fixture
                .auth
                .validate_api_access_token(&fixture.token, &[])
                .await
                .unwrap();
            fixture
                .blob_registry
                .test_shutdown_during_open(principal_id(&auth), auth, request)
                .await;
            continue;
        }
        let opened = if logout {
            fixture
                .web::<_, p::OpenBlobReadResponse>("OpenBlobRead", request.clone(), false)
                .await
                .unwrap()
        } else {
            client
                .open_blob_read(fixture.request(request.clone()))
                .await
                .unwrap()
                .into_inner()
        };
        let reader = opened.credentials.unwrap();
        assert_eq!(
            client
                .open_blob_read(fixture.request(request))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        if mode == 3 {
            client
                .close_blob_read(fixture.request(p::CloseBlobReadRequest {
                    credentials: Some(reader.clone()),
                }))
                .await
                .unwrap();
            assert!(
                client
                    .read_blob(fixture.request(p::ReadBlobRequest {
                        credentials: Some(reader),
                        max_bytes: 3
                    }))
                    .await
                    .is_err()
            );
        } else if mode == 1 {
            fixture
                .clock
                .advance
                .fetch_add(900000, std::sync::atomic::Ordering::AcqRel);
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(900)).await;
            tokio::time::resume();
            assert!(
                client
                    .read_blob(fixture.request(p::ReadBlobRequest {
                        credentials: Some(reader),
                        max_bytes: 3
                    }))
                    .await
                    .is_err()
            );
            assert!(
                client
                    .open_blob_read(fixture.request(p::OpenBlobReadRequest {
                        invocation_id: invocation.clone(),
                        access: Some(p::open_blob_read_request::Access::OutputGrant(grant))
                    }))
                    .await
                    .is_err()
            );
            let after = client
                .get_invocation(fixture.request(p::GetInvocationRequest {
                    invocation_id: invocation,
                }))
                .await
                .unwrap()
                .into_inner()
                .invocation
                .unwrap();
            assert_eq!(after, history);
        } else if logout {
            AccessAuthStore::new(&fixture.persistence)
                .revoke_browser_session(
                    &BrowserSessionTokenDigest::from_token(&fixture.cookie),
                    std::time::SystemTime::now(),
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert_eq!(
                client
                    .read_blob(fixture.request(p::ReadBlobRequest {
                        credentials: Some(reader),
                        max_bytes: 3
                    }))
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::PermissionDenied
            );
        } else {
            let mut read = client
                .read_blob(fixture.request(p::ReadBlobRequest {
                    credentials: Some(reader.clone()),
                    max_bytes: 3,
                }))
                .await
                .unwrap()
                .into_inner();
            let mut collected = Vec::new();
            let mut sequence = 0;
            let mut eof = 0;
            while let Some(chunk) = read.message().await.unwrap() {
                assert_eq!(chunk.sequence, sequence);
                sequence += 1;
                assert!(chunk.data.len() <= 3);
                collected.extend(chunk.data);
                if chunk.eof {
                    eof += 1;
                }
            }
            assert_eq!(collected, bytes);
            assert_eq!(eof, 1);
            assert!(
                client
                    .read_blob(fixture.request(p::ReadBlobRequest {
                        credentials: Some(reader),
                        max_bytes: 3
                    }))
                    .await
                    .is_err()
            );
        }
    }
    assert!(fixture.bodies.lock().unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn routine_upload_quarantine_cancel_and_uncommitted_reference_join_original_owner() {
    if isolated("routine_upload_quarantine_cancel_and_uncommitted_reference_join_original_owner") {
        return;
    }
    let fixture = Fixture::new(&[include_str!(
        "../../../tests/fixtures/routines/native-inspect.yaml"
    )])
    .await;
    let mut client = fixture.client().await;
    let mut definitions = client
        .watch_definitions(fixture.request(p::WatchDefinitionsRequest {}))
        .await
        .unwrap()
        .into_inner();
    let definition = definitions
        .message()
        .await
        .unwrap()
        .unwrap()
        .definitions
        .remove(0);
    drop(definitions);
    for mode in 0..5 {
        let response = client
            .open_attachment(fixture.request(p::OpenAttachmentRequest {
                open: Some(p::InteractionOpen {
                    selection: Some(p::InvocationSelection {
                        revision: definition.revision.clone(),
                        ..selection("native-inspect", "inspect")
                    }),
                    inputs: vec![p::InputDeclaration {
                        port: "source".into(),
                        format: Some(p::input_declaration::Format::Binary(
                            p::BinaryInputDeclaration {
                                display_name: "bad.png".into(),
                                content_type: "image/png".into(),
                                declared_bytes: Some(4),
                                expected_sha256: None,
                                purpose: Some(p::binary_input_declaration::Purpose::Image(1)),
                            },
                        )),
                    }],
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        let Some(p::open_attachment_response::Admission::Accepted(admitted)) = response.admission
        else {
            panic!("admission rejected");
        };
        let credentials = admitted.credentials.unwrap();
        let id = admitted.invocation.unwrap().invocation_id;
        let mut output = client
            .watch_live_output(fixture.request(p::WatchLiveOutputRequest {
                credentials: Some(credentials.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        output.message().await.unwrap().unwrap();
        let begin = client
            .begin_blob_upload(fixture.request(p::BeginBlobUploadRequest {
                invocation_id: id.clone(),
                port: "source".into(),
                display_name: "bad.png".into(),
                content_type: "image/png".into(),
                declared_bytes: Some(4),
                expected_sha256: None,
                purpose: Some(p::begin_blob_upload_request::Purpose::Image(1)),
            }))
            .await
            .unwrap()
            .into_inner();
        if mode == 0 {
            client
                .append_blob_upload(fixture.request(p::AppendBlobUploadRequest {
                    invocation_id: id.clone(),
                    upload_id: begin.upload_id.clone(),
                    sequence: 0,
                    offset: 0,
                    data: b"fake".to_vec(),
                }))
                .await
                .unwrap();
            assert!(
                client
                    .commit_blob_upload(fixture.request(p::CommitBlobUploadRequest {
                        invocation_id: id.clone(),
                        upload_id: begin.upload_id,
                        next_sequence: 1,
                        bytes: 4
                    }))
                    .await
                    .is_err()
            );
        } else if mode == 1 {
            client
                .cancel_blob_upload(fixture.request(p::CancelBlobUploadRequest {
                    invocation_id: id.clone(),
                    upload_id: begin.upload_id,
                }))
                .await
                .unwrap();
        } else if mode == 2 {
            assert!(
                client
                    .send_input_batch(fixture.request(p::SendInputBatchRequest {
                        credentials: Some(credentials),
                        sequence: 0,
                        events: vec![event(p::input_event::Event::Blob(p::BlobInput {
                            port: "source".into(),
                            blob_id: begin.upload_id
                        }))]
                    }))
                    .await
                    .is_err()
            );
        } else if mode == 3 {
            let closed = client
                .close_attachment(fixture.request(p::CloseAttachmentRequest {
                    credentials: Some(credentials),
                    mode: p::AttachmentCloseMode::Cancel as i32,
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(closed.cleanup_complete);
        } else {
            client
                .cancel_invocation(fixture.request(p::CancelInvocationRequest {
                    invocation_id: id.clone(),
                }))
                .await
                .unwrap();
        }
        let mut terminals = 0;
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(5), output.message())
            .await
            .unwrap()
            .unwrap()
        {
            if let Some(p::output_event::Event::Terminal(value)) = frame.output.unwrap().event {
                assert_ne!(value.status, p::InvocationStatus::Succeeded as i32);
                assert!(value.cleanup_complete);
                terminals += 1;
            }
        }
        assert_eq!(terminals, 1);
        let record = client
            .get_invocation(fixture.request(p::GetInvocationRequest { invocation_id: id }))
            .await
            .unwrap()
            .into_inner()
            .invocation
            .unwrap();
        assert!(record.helpers.is_empty() && record.text_results.is_empty());
    }
    assert!(fixture.bodies.lock().unwrap().is_empty());
    fixture.close().await;
}
