use super::*;
use crate::agent_host::{McpFeatureConfig, runtime_factory::ServerAgentRuntimeFactory};
use serde_json::{Value, json};
use std::time::Duration;

static APP_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    app: ServerApplication,
    listener: tokio::net::TcpListener,
    public: String,
    root: PathBuf,
    state: AgentDefinitionState,
    factory: Arc<ServerAgentRuntimeFactory>,
    feature: McpFeatureConfig,
    credentials: CredentialStore,
    sessions: waltr_agent::SessionCoordinator<waltr_agent::MemorySessionStore>,
    provider_calls: Arc<std::sync::atomic::AtomicUsize>,
    persistence: Persistence,
}

impl Fixture {
    async fn new() -> Self {
        Self::build(true).await
    }

    async fn build(exact_target: bool) -> Self {
        let root = crate::test_support::temp_root();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public = format!("http://{}/mcp", listener.local_addr().unwrap());
        std::fs::create_dir_all(root.join(".waltr/resources/mcp-servers")).unwrap();
        std::fs::write(root.join("fixture.txt"), "harmless-self-mcp-fixture").unwrap();
        std::fs::write(
            root.join(".waltr/resources/mcp-servers/waltr.yaml"),
            resource(&public),
        )
        .unwrap();
        let loader = crate::test_support::default_catalog_loader();
        let config_service = ConfigService::new(root.clone(), loader.clone())
            .await
            .unwrap();
        let state = config_service.state().clone();
        let coding = state
            .snapshot()
            .catalog()
            .resolved("coding")
            .unwrap()
            .clone();
        let persistence =
            Persistence::initialize(crate::persistence::DatabaseConfig::for_tests().unwrap())
                .await
                .unwrap();
        let credentials = CredentialStore::shared(persistence.outbound_oauth_store());
        let retained_persistence = persistence.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let provider_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = provider_calls.clone();
        let provider = Arc::new(crate::host_tests::scripted_provider::ScriptedProvider::new(
            "anonymous-fixture",
            move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec![
                    Ok(waltr_agent::CompletionEvent::Delta(
                        "fixture-complete".into(),
                    )),
                    Ok(waltr_agent::CompletionEvent::Finished),
                ]
            },
        ));
        let app = assemble_app_with_self_mcp(AppServices {
            config_service, persistence, catalog_loader: loader,
            access_authenticator: AccessAuthenticator::disabled(), mcp_oauth: None,
            outbound_private_origins: Vec::new(), outbound_oauth: OutboundOAuthConfig::default(),
            automatic_self_mcp_resource: None, agent_state: None, local_admission: None,
            workspace_root: root.clone(), sandbox_socket: None, routine_authority: RoutineAuthority::default(),
            routine_private_parent: crate::test_support::temp_root(),
        }, move |routes, input| {
            let service = Arc::new(Arc::try_unwrap(session::SessionService::new(waltr_agent::MemorySessionStore::new(), input)).ok().unwrap().with_test_provider(provider));
            tx.send((service.self_mcp_test_factory(), service.self_mcp_test_sessions())).unwrap();
            routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
            Ok(service)
        }, Some(url::Url::parse(if exact_target { &public } else { "https://not-self.invalid/mcp" }).unwrap())).await.unwrap();
        let (factory, sessions) = rx.await.unwrap();
        Self {
            app,
            listener,
            public,
            root,
            state,
            factory,
            feature: coding.features.mcp.unwrap(),
            credentials,
            sessions,
            provider_calls,
            persistence: retained_persistence,
        }
    }

    fn binding(&self) -> crate::self_mcp::SelfMcpBinding {
        self.app.self_mcp.as_ref().unwrap().binding()
    }

    async fn runtime(&self) -> Arc<crate::agent_host::mcp::McpClientRuntime> {
        let session = self
            .app
            .session_host
            .as_ref()
            .unwrap()
            .create(Some("coding".into()))
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            self.factory
                .mcp_registry()
                .runtime(&self.factory, &session.record, &self.sessions),
        )
        .await
        .unwrap()
        .expect("ordinary coding self discovery")
        .unwrap()
    }
}

fn resource(url: &str) -> String {
    format!(
        "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata: {{name: waltr}}\nspec: {{transport: streamableHttp, url: '{url}'}}\n"
    )
}

async fn list(endpoint: &str, bearer: Option<&str>, forged: bool) -> reqwest::Response {
    let mut request = crate::self_mcp::private_client()
        .unwrap()
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/list");
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    if forged {
        request = request.header("x-waltr-local-origin", "true");
    }
    let mut meta = json!({"io.modelcontextprotocol/protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities":{}});
    if forged {
        meta["validatedLocalOrigin"] = json!(true);
        meta["origin"] = json!("local_process");
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        request
            .json(&json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/list", "params":{"_meta":meta}
            }))
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn payload(response: reqwest::Response) -> Value {
    let sse = response
        .headers()
        .get("content-type")
        .is_some_and(|value| value.to_str().unwrap().contains("text/event-stream"));
    let bytes = response
        .bytes()
        .await
        .unwrap_or_else(|_| panic!("MCP response body failed"));
    if !sse {
        return serde_json::from_slice(&bytes).unwrap();
    }
    let mut decoder = mcp::client::transport::SseDecoder::with_limits(1024 * 1024, 8 * 1024 * 1024);
    let mut frames = decoder.push(&bytes).unwrap();
    frames.extend(decoder.finish().unwrap());
    frames
        .into_iter()
        .filter_map(|frame| frame.data)
        .filter_map(|data| serde_json::from_str::<Value>(&data).ok())
        .find(|value| value.get("id") == Some(&json!(1)))
        .expect("terminal MCP response")
}

async fn public_call(endpoint: &str, name: &str, arguments: Value) -> Value {
    let response = crate::self_mcp::private_client()
        .unwrap()
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", name)
        .json(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":name,"arguments":arguments,"_meta":{
                "io.modelcontextprotocol/protocolVersion":mcp::protocol::MCP_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities":{}}}}),
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    payload(response).await
}

#[tokio::test]
async fn self_mcp_anonymous_disabled_production_dispatch_and_sessions() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let public = fixture.public.clone();
    let calls = fixture.provider_calls.clone();
    let sessions = fixture.sessions.clone();
    let persistence = fixture.persistence.clone();
    let cancel = CancellationToken::new();
    let server = tokio::spawn(
        fixture
            .app
            .serve_with_shutdown(fixture.listener, cancel.clone().cancelled_owned()),
    );
    let discovery = payload(list(&public, None, false).await).await;
    let tools = discovery["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "capability__read"));
    assert!(!tools.iter().any(|tool| tool["name"] == "code"));
    let read = public_call(
        &public,
        "capability__read",
        json!({"filePath":"fixture.txt"}),
    )
    .await;
    assert!(read.get("error").is_none(), "{read}");
    assert!(
        read.to_string().contains("harmless-self-mcp-fixture"),
        "{read}"
    );
    let (association, state): (String, String) =
        sqlx::query_as("SELECT association,state FROM external_capability_dispatch")
            .fetch_one(persistence.sqlite_pool().unwrap())
            .await
            .unwrap();
    let association: Value = serde_json::from_str(&association).unwrap();
    assert_eq!(association["principal"]["method"], "AnonymousDisabled");
    assert_eq!(association["principal"]["subject"], "shared-dev-domain");
    assert_eq!(association["principal"]["scopes"], json!([]));
    assert_eq!(association["call"]["name"], "read");
    assert_eq!(state, "complete");
    let unknown = public_call(&public, "capability__not_enabled", json!({})).await;
    assert!(unknown.get("error").is_some(), "{unknown}");
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"] == "capability__question")
    );
    let question = public_call(&public, "capability__question", json!({"questions":[{
        "question":"No unrelated session may answer", "header":"fixture", "options":[], "multiple":false, "custom":true
    }]})).await;
    assert!(
        question.get("error").is_some() || question["result"]["isError"] == true,
        "{question}"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let created = public_call(
        &public,
        "create",
        json!({"action":"session.create","input":{"agent_name":"coding","prompt":"fixture first"}}),
    )
    .await;
    let output = &created["result"]["structuredContent"];
    let id = output["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let record = sessions
                .load(&waltr_agent::SessionId::from(id))
                .await
                .unwrap()
                .record;
            if serde_json::to_string(&record)
                .unwrap()
                .contains("fixture-complete")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let sent = public_call(
        &public,
        "execute",
        json!({"action":"session.send","input":{"id":id,"prompt":"fixture second"}}),
    )
    .await;
    assert!(sent.get("error").is_none(), "{sent}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn self_mcp_bare_server_with_real_dispatcher_still_rejects_forged_origins() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::build(false).await;
    assert!(fixture.app.self_mcp.is_none());
    let bare = WaltrMcpServer::new(
        fixture.app.session_host.as_ref().unwrap().clone(),
        fixture.state.clone(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let server = tokio::spawn(crate::http_lifecycle::serve_owned_connections(
        listener,
        mcp::server::streamable_http_router(Arc::new(bare)),
        cancel.clone().cancelled_owned(),
    ));
    let denied = payload(list(&endpoint, Some("forged-local-bearer"), true).await).await;
    assert_eq!(denied["error"]["code"], -32602, "{denied}");
    for (name, arguments) in [
        ("capability__read", json!({"filePath":"fixture.txt"})),
        (
            "create",
            json!({"action":"session.create","input":{"prompt":"never admitted"}}),
        ),
        (
            "execute",
            json!({"action":"session.send","input":{"id":"foreign","prompt":"never admitted"}}),
        ),
    ] {
        let denied = public_call(&endpoint, name, arguments).await;
        assert!(
            matches!(denied["error"]["code"].as_i64(), Some(-32602 | -32020)),
            "{denied}"
        );
    }
    assert_eq!(
        fixture
            .provider_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.app.shutdown().await;
}

#[tokio::test]
async fn self_mcp_standalone_coding_discovery_and_dispatch() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let runtime = fixture.runtime().await;
    let binding = fixture.binding();
    let response =
        payload(list(binding.endpoint(), Some(&binding.token().unwrap()), false).await).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect(&format!("tool listing: {response}"));
    assert!(tools.iter().any(|tool| tool["name"] == "capability__read"));
    assert!(!tools.iter().any(|tool| tool["name"] == "code"));
    assert!(!tools.iter().any(|tool| {
        tool["name"]
            .as_str()
            .unwrap()
            .contains("capability__capability__")
    }));
    let call = waltr_agent::ToolCall::new(
        "self-read",
        "capability__read",
        json!({"filePath":"fixture.txt"}),
    );
    let (result, _) = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.call_app_tool(
            "waltr",
            "capability__read",
            &call,
            CancellationToken::new(),
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!result.is_error, "harmless read must succeed");
    assert!(
        result
            .content
            .as_str()
            .contains("harmless-self-mcp-fixture")
    );
    runtime.shutdown().await.unwrap();
    fixture.app.shutdown().await;
}

#[tokio::test]
async fn self_mcp_public_forgery_private_admission_and_native_status() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let binding = fixture.binding();
    let token = binding.token().unwrap();
    for bearer in [None, Some("wrong-generation")] {
        assert_eq!(
            list(binding.endpoint(), bearer, true).await.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    }
    assert!(
        payload(list(binding.endpoint(), Some(&token), false).await).await["result"]["tools"]
            .is_array()
    );
    let cancel = CancellationToken::new();
    let public = fixture.public.clone();
    let server = tokio::spawn(
        fixture
            .app
            .serve_with_shutdown(fixture.listener, cancel.clone().cancelled_owned()),
    );
    let anonymous = payload(list(&public, None, false).await).await;
    for bearer in [None, Some(token.as_str())] {
        let response = payload(list(&public, bearer, true).await).await;
        assert!(response["result"]["tools"].is_array(), "{response}");
        assert_eq!(response["result"]["tools"], anonymous["result"]["tools"]);
        assert!(
            !response["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "code")
        );
    }
    let root = public.strip_suffix("/mcp").unwrap();
    assert!(
        crate::self_mcp::private_client()
            .unwrap()
            .get(format!("{root}/health"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let channel = tonic::transport::Endpoint::from_shared(root.to_owned())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut status =
        waltr::grpc::waltr::status::v1::status_service_client::StatusServiceClient::new(channel);
    let mut stream = status
        .watch_status(waltr::grpc::waltr::status::v1::WatchStatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .status
            .unwrap()
            .ready
    );
    drop(stream);
    for path in ["/health", "/waltr.status.v1.StatusService/WatchStatus"] {
        let private_root = binding.endpoint().strip_suffix("/mcp").unwrap();
        assert_eq!(
            crate::self_mcp::private_client()
                .unwrap()
                .get(format!("{private_root}{path}"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!binding.active());
}

#[tokio::test]
async fn self_mcp_canceled_shutdown_retains_join_and_application_cleanup() {
    let _serial = APP_SERIAL.lock().await;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let fixture = Fixture::new().await;
    let binding = fixture.binding();
    let runtime = fixture.runtime().await;
    let routine_health = fixture.app.routine_engine.as_ref().unwrap().health();
    let address = url::Url::parse(binding.endpoint())
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
    connection.write_all(format!(
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nExpect: 100-continue\r\nContent-Length: 1024\r\n\r\n",
        binding.token().unwrap(),
    ).as_bytes()).await.unwrap();
    let mut interim = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !interim.ends_with(b"\r\n\r\n") {
            interim.push(connection.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");

    let mut shutdown = Box::pin(fixture.app.shutdown());
    assert!(futures_util::poll!(&mut shutdown).is_pending());
    assert!(
        !binding.active(),
        "shutdown must revoke admission immediately"
    );
    drop(shutdown);
    let mut finished = Box::pin(binding.cleanup_finished());
    assert!(
        futures_util::poll!(&mut finished).is_pending(),
        "canceled shutdown must not report completion while its service is pending"
    );
    assert!(!runtime.lifetime().is_cancelled());
    drop(connection);
    tokio::time::timeout(Duration::from_secs(10), finished)
        .await
        .unwrap();
    assert!(
        runtime.lifetime().is_cancelled(),
        "fallback must retire the actual client registry"
    );
    assert!(
        matches!(&*routine_health.borrow(), RuntimeHealth::Failed(reason) if reason == "shutdown")
    );
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn self_mcp_application_retires_and_joins_active_connection() {
    let _serial = APP_SERIAL.lock().await;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let first = Fixture::new().await;
    let a = first.binding();
    let addr = url::Url::parse(a.endpoint())
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    let mut connection = tokio::net::TcpStream::connect(addr).await.unwrap();
    connection
        .write_all(b"POST /mcp HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), first.app.shutdown())
        .await
        .unwrap();
    let mut byte = [0];
    let closed = tokio::time::timeout(Duration::from_secs(2), connection.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(closed, Ok(0)) || closed.is_err());
    assert!(!a.active());
    assert!(a.token().is_err());
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn self_mcp_exact_target_and_metadata_privacy() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let binding = fixture.binding();
    let secret = binding.token().unwrap();
    let runtime = fixture.runtime().await;
    let prepared = fixture
        .factory
        .prepare_mcp_runtime(&fixture.feature)
        .await
        .unwrap()
        .unwrap();
    let debug = format!("{:?} {:?} {:?}", fixture.app, binding, prepared);
    assert!(!debug.contains(&secret));
    assert!(!debug.contains(binding.endpoint()));
    let persisted =
        std::fs::read_to_string(fixture.root.join(".waltr/resources/mcp-servers/waltr.yaml"))
            .unwrap();
    assert_eq!(persisted, resource(&fixture.public));
    assert!(!persisted.contains(&secret));
    let catalog = format!("{:?}", fixture.state.resource_inspections(None).unwrap());
    assert!(!catalog.contains(&secret));
    assert!(!catalog.contains(binding.endpoint()));
    for summary in fixture.sessions.list().await.unwrap() {
        let record = fixture.sessions.load(&summary.id).await.unwrap().record;
        let retained = serde_json::to_string(&record).unwrap();
        assert!(!retained.contains(&secret));
        assert!(!retained.contains(binding.endpoint()));
    }
    assert!(
        fixture
            .credentials
            .read(&crate::auth::AuthTarget::mcp_server("waltr").credential_key())
            .await
            .unwrap()
            .is_none()
    );
    runtime.shutdown().await.unwrap();
    fixture.app.shutdown().await;
    let unrelated = Fixture::build(false).await;
    assert!(
        unrelated.app.self_mcp.is_none(),
        "a name alone cannot establish self identity"
    );
    unrelated.app.shutdown().await;
}

#[tokio::test]
async fn self_mcp_edited_resource_cannot_receive_generation_secret_or_retain_authority() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let runtime = fixture.runtime().await;
    let binding = fixture.binding();
    let received = Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    let headers = received.clone();
    struct EditedUpstream;
    impl mcp::server::ServerHandler for EditedUpstream {
        fn server_info(&self) -> mcp::server::ServerInfo {
            mcp::server::ServerInfo::new("edited-upstream", "1")
        }
        fn capabilities(&self) -> mcp::server::ServerCapabilities {
            mcp::server::ServerCapabilities::default()
        }
    }
    let router =
        mcp::server::streamable_http_router(Arc::new(EditedUpstream)).layer(middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let headers = headers.clone();
                async move {
                    headers.lock().unwrap().push(
                        request
                            .headers()
                            .get("authorization")
                            .map(|header| header.to_str().unwrap().to_owned()),
                    );
                    next.run(request).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let edited = format!("http://{}/mcp", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let server = tokio::spawn(crate::http_lifecycle::serve_owned_connections(
        listener,
        router,
        cancel.clone().cancelled_owned(),
    ));
    let current = fixture
        .state
        .resource_inspections(Some("mcp-servers/waltr.yaml"))
        .unwrap()
        .remove(0);
    fixture
        .state
        .replace(
            "mcp-servers/waltr.yaml",
            resource(&edited),
            &current.persisted_etag,
        )
        .await
        .unwrap();
    assert!(!runtime.app_authority_current("waltr").await);
    let next = fixture.runtime().await;
    assert!(!received.lock().unwrap().is_empty());
    assert!(
        received.lock().unwrap().iter().all(Option::is_none),
        "edited upstream must receive no bearer"
    );
    assert!(
        binding.active(),
        "editing resources does not change the owning application generation"
    );
    runtime.shutdown().await.unwrap();
    next.shutdown().await.unwrap();
    fixture.app.shutdown().await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(8), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn self_mcp_private_client_refuses_same_origin_and_cross_origin_redirects() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let target_hits = Arc::new(AtomicUsize::new(0));
    let hits = target_hits.clone();
    let target = Router::new().fallback(move || {
        hits.fetch_add(1, Ordering::SeqCst);
        async { axum::http::StatusCode::NO_CONTENT }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other = format!("http://{}/target", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let target_server = tokio::spawn(crate::http_lifecycle::serve_owned_connections(
        listener,
        target,
        cancel.clone().cancelled_owned(),
    ));
    let hits = target_hits.clone();
    let redirect = Router::new()
        .route(
            "/same",
            axum::routing::post(|| async { axum::response::Redirect::temporary("/target") }),
        )
        .route(
            "/other",
            axum::routing::post(move || {
                let other = other.clone();
                async move { axum::response::Redirect::temporary(&other) }
            }),
        )
        .route(
            "/target",
            axum::routing::any(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { axum::http::StatusCode::NO_CONTENT }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let redirect_server = tokio::spawn(crate::http_lifecycle::serve_owned_connections(
        listener,
        redirect,
        cancel.clone().cancelled_owned(),
    ));
    for path in ["same", "other"] {
        let response = crate::self_mcp::private_client()
            .unwrap()
            .post(format!("{root}/{path}"))
            .bearer_auth("disposable-test-bearer")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    }
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        0,
        "redirect destination must receive no request or credential"
    );
    cancel.cancel();
    for server in [target_server, redirect_server] {
        tokio::time::timeout(Duration::from_secs(8), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn self_mcp_private_client_bypasses_environment_proxy() {
    const CHILD: &str = "WALTR_SELF_MCP_PROXY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let response = reqwest::Client::new()
            .get("http://proxy-control.invalid/")
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
        let fixture = Fixture::new().await;
        let runtime = fixture.runtime().await;
        runtime.shutdown().await.unwrap();
        fixture.app.shutdown().await;
        return;
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let hits = proxy_hits.clone();
    let proxy = Router::new().fallback(move |headers: axum::http::HeaderMap| {
        hits.fetch_add(1, Ordering::SeqCst);
        assert!(
            headers.get("authorization").is_none(),
            "private bearer must never reach proxy"
        );
        async { axum::http::StatusCode::BAD_GATEWAY }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    let server = tokio::spawn(crate::http_lifecycle::serve_owned_connections(
        listener,
        proxy,
        cancel.clone().cancelled_owned(),
    ));
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "app::self_mcp_tests::self_mcp_private_client_bypasses_environment_proxy",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .kill_on_drop(true);
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        child.env(key, &proxy_url);
    }
    for key in ["NO_PROXY", "no_proxy"] {
        child.env_remove(key);
    }
    let output = tokio::time::timeout(Duration::from_secs(30), child.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "proxy child failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        1,
        "only the ambient-proxy control request reaches proxy"
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(8), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn self_mcp_public_bind_failure_rolls_back_and_drop_retires() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let binding = fixture.binding();
    let address = fixture.listener.local_addr().unwrap();
    assert!(fixture.app.bind(address).await.is_err());
    assert!(!binding.active());
    let addr = url::Url::parse(binding.endpoint())
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    let dropped = Fixture::new().await;
    let binding = dropped.binding();
    let runtime = dropped.runtime().await;
    drop(dropped.app);
    assert!(!binding.active());
    tokio::time::timeout(Duration::from_secs(10), binding.cleanup_finished())
        .await
        .unwrap();
    assert!(
        runtime.lifetime().is_cancelled(),
        "drop cleanup must retire the actual client registry"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.app_authority_current("waltr").await {
            tokio::task::yield_now().await;
        }
        let addr = url::Url::parse(binding.endpoint())
            .unwrap()
            .socket_addrs(|| None)
            .unwrap()[0];
        while tokio::net::TcpStream::connect(addr).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn self_mcp_assembly_failure_revokes_prebound_generation() {
    let _serial = APP_SERIAL.lock().await;
    let fixture = Fixture::new().await;
    fixture.app.shutdown().await;
    let loader = crate::test_support::default_catalog_loader();
    let config_service = ConfigService::new(fixture.root.clone(), loader.clone())
        .await
        .unwrap();
    let persistence =
        Persistence::initialize(crate::persistence::DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let result = assemble_app_with_self_mcp(AppServices {
        config_service, persistence, catalog_loader:loader,
        access_authenticator:AccessAuthenticator::disabled(), mcp_oauth:None,
        outbound_private_origins:Vec::new(), outbound_oauth:OutboundOAuthConfig::default(),
        automatic_self_mcp_resource:None, agent_state:None, local_admission:None,
        workspace_root:fixture.root.clone(), sandbox_socket:None, routine_authority:RoutineAuthority::default(),
        routine_private_parent:fixture.root.join("does-not-exist"),
    }, move |routes, input| {
        let service = session::SessionService::new(waltr_agent::MemorySessionStore::new(), input);
        tx.send(service.self_mcp_test_factory()).unwrap();
        routes.add_service(waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer::from_arc(service.clone()));
        Ok(service)
    }, Some(url::Url::parse(&fixture.public).unwrap())).await;
    assert!(
        result.is_err(),
        "missing private parent must fail normal Routine startup"
    );
    let factory = rx.await.unwrap();
    let binding = factory.self_mcp_test_binding().unwrap();
    assert!(!binding.active());
    assert!(binding.token().is_err());
    let address = url::Url::parse(binding.endpoint())
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}
