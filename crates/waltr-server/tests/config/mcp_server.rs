use crate::support::grpc::{config_client, get_config};
use waltr::grpc::waltr::config::v1::{InferenceDefaults, McpServerType};
use waltr_agent::{
    InferenceProviderSettings, InferenceSettings, MemorySessionStore, ModelApi,
    PendingTimelineEvent, SessionCoordinator, SessionId, SessionStore, TimelineContent,
    TimelineEventKind, TimelineStatus,
};
use waltr_server::{
    ServerApplication, build_app_with_services,
    config::ConfigStore as _,
    config_service::ConfigService,
    persistence::{DatabaseConfig, Persistence},
    session,
    test_support::{
        blocked_background_application, default_config_builder, setup_server, temp_root,
    },
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::SocketAddr,
    sync::atomic::Ordering,
    time::Duration,
};
use tokio::sync::oneshot;

#[tokio::test]
async fn mcp_configuration_actions_call_config_rpc_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let endpoint = server.url("/mcp");

    let inspected = call_exec(&endpoint, json!({ "action": "configuration.inspect" })).await?;
    let inspected_etag = inspected["structuredContent"]["config"]["etag"]
        .as_str()
        .expect("inspected etag");
    assert!(!inspected_etag.is_empty());

    let patched = call_exec(
        &endpoint,
        json!({
            "action": "configuration.patch",
            "input": {
                "expected_etag": inspected_etag,
                "mcp_servers": {
                    "mcp_servers": {
                        "demo": {
                            "name": "Demo MCP",
                            "type": McpServerType::Stdio as i32,
                            "command": "demo-mcp",
                            "url": ""
                        }
                    }
                }
            }
        }),
    )
    .await?;
    assert!(
        patched["structuredContent"].is_object(),
        "patch response should include structured content: {patched}"
    );

    let mut client = config_client(server).await?;
    let current = get_config(&mut client).await?;
    assert_eq!(current.mcp_servers["demo"].name, "Demo MCP");
    assert_eq!(current.mcp_servers["demo"].command, "demo-mcp");
    assert_eq!(
        current.mcp_servers["demo"].r#type,
        McpServerType::Stdio as i32
    );

    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the integration test exercises one shared coordinator across the complete MCP lifecycle"
)]
async fn mcp_sessions_share_the_registered_grpc_coordinator()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temp_root();
    let persistence = Persistence::initialize(DatabaseConfig::from_url(&format!(
        "sqlite://{}",
        root.join("waltr.sqlite3").display()
    ))?)
    .await?;
    let store = MemorySessionStore::new();
    let coordinator = SessionCoordinator::new(store.clone());
    let id = SessionId::new("mcp-shared-session");
    coordinator
        .create_with_id_and_metadata_values(id.clone(), BTreeMap::default())
        .await?;
    append_status(&store, &id, TimelineStatus::Finished).await?;
    let runnable_id = SessionId::new("mcp-runnable-session");
    coordinator
        .create_with_id_and_inference_settings(
            runnable_id.clone(),
            Some(InferenceSettings {
                provider: InferenceProviderSettings {
                    key: "test".to_owned(),
                    api: ModelApi::OpenAiResponses,
                    url: "http://127.0.0.1:9".to_owned(),
                    model_id: "test-model".to_owned(),
                    model_cost: None,
                    headers: BTreeMap::default(),
                    oauth: None,
                    reasoning_effort: None,
                },
            }),
            None,
            BTreeMap::default(),
        )
        .await?;
    let config_service = ConfigService::new(root.clone(), default_config_builder())?;
    let mut config = config_service.store().load().await?;
    config.mcp_servers.clear();
    config_service.store().save(config).await?;
    let app = build_app_with_services(config_service, persistence, {
        let coordinator = coordinator.clone();
        move |routes, config, credential_store| {
            session::add_sessions_service(
                routes,
                coordinator,
                config,
                credential_store,
                None,
                waltr_server::test_support::default_agent_catalog(),
            )
        }
    })
    .await?;
    let router = TestRouter::start(app).await?;

    let tools = call_method(&router.endpoint, "tools/list", json!({})).await?;
    let sessions_tool = tools["result"]["tools"]
        .as_array()
        .expect("tools list")
        .iter()
        .find(|tool| tool["name"] == "sessions")
        .expect("sessions tool");
    let schema = sessions_tool["inputSchema"].to_string();
    for field in ["create", "agent_name", "prompt", "wipe", "confirm", "id"] {
        assert!(schema.contains(field), "missing {field} in {schema}");
    }
    assert!(!schema.contains("render"), "unexpected render in {schema}");
    assert!(
        !schema.contains("timeline"),
        "unexpected timeline in {schema}"
    );
    assert!(
        !schema.contains("provider"),
        "unexpected provider in {schema}"
    );
    assert!(!schema.contains("model"), "unexpected model in {schema}");

    let help = call_tool(&router.endpoint, "sessions", json!({ "action": "help" })).await?;
    let help_actions = help["structuredContent"]["actions"]
        .as_array()
        .expect("session help actions");
    assert_eq!(
        help_actions
            .iter()
            .map(|action| action["action"].as_str().expect("action name"))
            .collect::<Vec<_>>(),
        ["list", "create", "get", "wipe", "interrupt", "send", "wait"]
    );
    assert!(
        help_actions
            .iter()
            .all(|action| !matches!(action["action"].as_str(), Some("render" | "timeline"))),
        "render and timeline help actions should be absent"
    );
    let wipe_help = help_actions
        .iter()
        .find(|action| action["action"] == "wipe")
        .expect("wipe help action");
    assert!(wipe_help["input_schema"].to_string().contains("confirm"));

    let listed = call_tool(&router.endpoint, "sessions", json!({ "action": "list" })).await?;
    assert_eq!(
        listed["structuredContent"]["sessions"]
            .as_array()
            .expect("session list")
            .len(),
        2
    );

    let fetched = call_tool(
        &router.endpoint,
        "sessions",
        json!({ "action": "get", "input": { "id": id.as_str() } }),
    )
    .await?;
    assert_eq!(fetched["structuredContent"]["id"], id.as_str());
    assert_eq!(fetched["structuredContent"]["running"], false);
    assert!(fetched["structuredContent"]["pending_work"].is_array());
    assert!(fetched["structuredContent"]["active_external_requests"].is_array());
    assert!(fetched["structuredContent"].get("runtime").is_none());
    assert!(fetched["structuredContent"].get("mcp_servers").is_none());
    assert!(
        fetched["structuredContent"]
            .get("mcp_trace_propagation")
            .is_none()
    );

    let waited = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "wait",
            "input": { "id": id.as_str(), "jq": ".running == false and .status == \"finished\"", "timeout_seconds": 1 }
        }),
    )
    .await?;
    assert_eq!(waited["structuredContent"]["reason"], "condition_met");
    assert_eq!(waited["structuredContent"]["jq_output"], true);

    let sent = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "send",
            "input": { "id": runnable_id.as_str(), "prompt": "hello" }
        }),
    )
    .await?;
    assert_eq!(
        sent["structuredContent"]["session_id"],
        runnable_id.as_str()
    );
    assert_eq!(sent["structuredContent"]["disposition"], "started");
    assert_eq!(sent["structuredContent"]["queue_position"], Value::Null);

    call_tool(
        &router.endpoint,
        "sessions",
        json!({ "action": "interrupt", "input": { "id": runnable_id.as_str() } }),
    )
    .await?;
    assert!(coordinator.active_state(&runnable_id).await.is_none());

    call_tool(
        &router.endpoint,
        "sessions",
        json!({ "action": "interrupt", "input": { "id": id.as_str() } }),
    )
    .await?;
    let durable = coordinator.load(&id).await?;
    assert!(matches!(
        durable.record.snapshot().status,
        waltr_agent::SessionStatus::Finished
    ));

    assert_mcp_error(
        &call_tool_response(
            &router.endpoint,
            "sessions",
            json!({ "action": "wipe", "input": { "confirm": false } }),
        )
        .await?,
        -32602,
        "confirm must be true",
    );
    assert_eq!(coordinator.list().await?.len(), 2);
    let wiped = call_tool(
        &router.endpoint,
        "sessions",
        json!({ "action": "wipe", "input": { "confirm": true } }),
    )
    .await?;
    assert_eq!(wiped["structuredContent"], json!({ "wiped": true }));
    assert!(coordinator.list().await?.is_empty());
    let wiped_again = call_tool(
        &router.endpoint,
        "sessions",
        json!({ "action": "wipe", "input": { "confirm": true } }),
    )
    .await?;
    assert_eq!(wiped_again["structuredContent"], json!({ "wiped": true }));

    router.shutdown().await?;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the integration test verifies creation validation and prompt admission through MCP"
)]
async fn mcp_session_create_selects_defaults_validates_before_create_and_starts_prompt()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temp_root();
    let persistence = Persistence::initialize(DatabaseConfig::from_url(&format!(
        "sqlite://{}",
        root.join("waltr.sqlite3").display()
    ))?)
    .await?;
    let coordinator = SessionCoordinator::new(MemorySessionStore::new());
    let config_service = ConfigService::new(root.clone(), default_config_builder())?;
    let mut config = config_service.store().load().await?;
    let provider_key = "openai".to_owned();
    let model_key = config
        .providers
        .get(&provider_key)
        .and_then(|provider| provider.models.keys().next())
        .cloned()
        .expect("test defaults include an OpenAI model");
    config.inference_defaults = Some(InferenceDefaults {
        provider_key: provider_key.clone(),
        model_key,
        reasoning_effort: None,
    });
    config
        .providers
        .get_mut(&provider_key)
        .expect("selected provider")
        .headers
        .clear();
    config.mcp_servers.clear();
    config_service.store().save(config).await?;
    let app = build_app_with_services(config_service, persistence, {
        let coordinator = coordinator.clone();
        move |routes, config, credential_store| {
            session::add_sessions_service(
                routes,
                coordinator,
                config,
                credential_store,
                None,
                waltr_server::test_support::default_agent_catalog(),
            )
        }
    })
    .await?;
    let router = TestRouter::start(app).await?;

    let selected = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "create",
            "input": { "agent_name": "coding", "prompt": "selected prompt" }
        }),
    )
    .await?;
    assert_eq!(
        selected["structuredContent"]
            .as_object()
            .expect("create structured content")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "agent_name",
            "disposition",
            "queue_position",
            "run_id",
            "session_id",
        ])
    );
    assert_eq!(selected["structuredContent"]["agent_name"], "coding");
    assert_eq!(selected["structuredContent"]["disposition"], "started");
    assert_eq!(selected["structuredContent"]["queue_position"], Value::Null);
    let selected_id = SessionId::new(
        selected["structuredContent"]["session_id"]
            .as_str()
            .expect("selected session id"),
    );
    assert!(selected["structuredContent"]["run_id"].as_u64().is_some());
    let selected_session = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let session = coordinator.load(&selected_id).await?;
            if session.record.summary().current_prompt.as_deref() == Some("selected prompt") {
                break Ok::<_, waltr_agent::LlmError>(session);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "prompt was not journaled"))??;
    assert_eq!(
        selected_session
            .record
            .agent_definition
            .as_ref()
            .map(|bundle| bundle.agent_name.as_str()),
        Some("coding")
    );

    let defaulted = call_tool(
        &router.endpoint,
        "sessions",
        json!({ "action": "create", "input": { "prompt": "default prompt" } }),
    )
    .await?;
    assert_eq!(defaulted["structuredContent"]["agent_name"], "coding");
    assert_eq!(defaulted["structuredContent"]["disposition"], "started");
    assert!(defaulted["structuredContent"]["run_id"].as_u64().is_some());
    assert_eq!(coordinator.list().await?.len(), 2);

    let blank_prompt = call_tool_response(
        &router.endpoint,
        "sessions",
        json!({
            "action": "create",
            "input": { "agent_name": "missing", "prompt": "  " }
        }),
    )
    .await?;
    assert_mcp_error(&blank_prompt, -32602, "prompt must not be empty");
    assert_eq!(coordinator.list().await?.len(), 2);

    for agent_name in ["", "missing"] {
        let invalid_agent = call_tool_response(
            &router.endpoint,
            "sessions",
            json!({
                "action": "create",
                "input": { "agent_name": agent_name, "prompt": "must not create" }
            }),
        )
        .await?;
        assert_mcp_error(&invalid_agent, -32602, "agent");
        assert_eq!(coordinator.list().await?.len(), 2);
    }

    router.shutdown().await?;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test]
async fn mcp_sessions_reject_invalid_wait_inputs() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    for arguments in [
        json!({
            "action": "wait",
            "input": { "id": "missing", "jq": ".[", "timeout_seconds": 1 }
        }),
        json!({
            "action": "wait",
            "input": { "id": "missing", "jq": ".", "timeout_seconds": 301 }
        }),
    ] {
        let response = call_tool_response(&server.url("/mcp"), "sessions", arguments).await?;
        assert!(
            response.get("error").is_some()
                || response["result"]["isError"].as_bool().unwrap_or(false),
            "{response}"
        );
    }
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the integration test covers bounded jq output and wait lifecycle reconciliation"
)]
async fn mcp_session_wait_reconciles_durable_state_and_reports_fallbacks()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temp_root();
    let persistence = Persistence::initialize(DatabaseConfig::from_url(&format!(
        "sqlite://{}",
        root.join("waltr.sqlite3").display()
    ))?)
    .await?;
    let store = MemorySessionStore::new();
    let coordinator = SessionCoordinator::new(store.clone());
    let id = SessionId::new("mcp-wait-session");
    coordinator
        .create_with_id_and_metadata_values(id.clone(), BTreeMap::default())
        .await?;
    append_status(&store, &id, TimelineStatus::StreamingProvider).await?;

    let app = build_app_with_services(
        ConfigService::new(root.clone(), default_config_builder())?,
        persistence,
        {
            let coordinator = coordinator.clone();
            move |routes, config, credential_store| {
                session::add_sessions_service(
                    routes,
                    coordinator,
                    config,
                    credential_store,
                    None,
                    waltr_server::test_support::default_agent_catalog(),
                )
            }
        },
    )
    .await?;
    let router = TestRouter::start(app).await?;

    let overflow = call_tool_response(
        &router.endpoint,
        "sessions",
        json!({
            "action": "wait",
            "input": { "id": id.as_str(), "jq": "range(0; 257)", "timeout_seconds": 1 }
        }),
    )
    .await?;
    assert_mcp_error(
        &overflow,
        -32602,
        "jq filter produced more than 256 results",
    );

    let wait_for_finish = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "wait",
            "input": { "id": id.as_str(), "jq": ".status == \"finished\"", "timeout_seconds": 2 }
        }),
    );
    let finish = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        append_status(&store, &id, TimelineStatus::Finished)
            .await
            .expect("durable finished status");
    };
    let (waited, ()) = tokio::join!(wait_for_finish, finish);
    let waited = waited?;
    assert_eq!(waited["structuredContent"]["reason"], "condition_met");
    assert_eq!(waited["structuredContent"]["jq_output"], true);

    let stopped = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "wait",
            "input": { "id": id.as_str(), "jq": ".status == \"never\"", "timeout_seconds": 1 }
        }),
    )
    .await?;
    assert_eq!(
        stopped["structuredContent"]["reason"],
        "session_not_running"
    );

    append_status(&store, &id, TimelineStatus::StreamingProvider).await?;
    let timed_out = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "wait",
            "input": { "id": id.as_str(), "jq": ".status == \"never\"", "timeout_seconds": 1 }
        }),
    )
    .await?;
    assert_eq!(timed_out["structuredContent"]["reason"], "timeout");

    router.shutdown().await?;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

async fn append_status(
    store: &MemorySessionStore,
    id: &SessionId,
    status: TimelineStatus,
) -> Result<(), Box<dyn std::error::Error>> {
    let revision = store.load(id).await?.revision;
    let event = status_event(id, status);
    store.append_timeline(id, revision, vec![event]).await?;
    Ok(())
}

fn status_event(id: &SessionId, status: TimelineStatus) -> PendingTimelineEvent {
    let mut event = PendingTimelineEvent::new(
        id.clone(),
        0,
        waltr_agent::PendingTimelineItemId::New,
        TimelineEventKind::SessionStatusChanged,
        None,
        TimelineContent::Empty,
    );
    event.status = Some(status);
    event
}

struct TestRouter {
    endpoint: String,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<io::Result<()>>,
}

impl TestRouter {
    async fn start(app: ServerApplication) -> io::Result<Self> {
        Self::start_at(app, SocketAddr::from(([127, 0, 0, 1], 0))).await
    }

    async fn start_at(app: ServerApplication, addr: SocketAddr) -> io::Result<Self> {
        let (app, listener, addr) = app.bind(addr).await?;
        let (shutdown, receiver) = oneshot::channel();
        let server = tokio::spawn(app.serve_with_shutdown(listener, async move {
            let _ = receiver.await;
        }));
        Ok(Self {
            endpoint: format!("http://{addr}/mcp"),
            shutdown,
            server,
        })
    }

    async fn shutdown(self) -> io::Result<()> {
        let _ = self.shutdown.send(());
        self.server
            .await
            .map_err(|error| io::Error::other(format!("test router task failed: {error}")))?
    }
}

#[tokio::test]
async fn start_router_bind_failure_cancels_and_joins_cleanup() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();
    let (app, probe) = blocked_background_application();
    probe.cleanup_started.notified().await;

    let startup = tokio::spawn(TestRouter::start_at(app, addr));
    probe.shutdown_observed.notified().await;
    assert!(probe.cleanup_dropped.load(Ordering::SeqCst));
    assert!(!startup.is_finished());
    probe.release_cleanup.notify_one();

    let Err(error) = startup.await.expect("router startup should join") else {
        panic!("occupied port should fail router startup");
    };
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
}

#[tokio::test]
async fn test_router_shutdown_cancels_and_joins_cleanup() {
    let (app, probe) = blocked_background_application();
    let router = TestRouter::start(app).await.unwrap();
    probe.cleanup_started.notified().await;

    let shutdown = tokio::spawn(router.shutdown());
    probe.shutdown_observed.notified().await;
    assert!(probe.cleanup_dropped.load(Ordering::SeqCst));
    assert!(!shutdown.is_finished());
    probe.release_cleanup.notify_one();

    shutdown
        .await
        .expect("router shutdown should join")
        .expect("router shutdown should succeed");
}

async fn call_exec(endpoint: &str, arguments: Value) -> Result<Value, Box<dyn std::error::Error>> {
    call_tool(endpoint, "exec", arguments).await
}

async fn call_tool(
    endpoint: &str,
    tool: &str,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = call_tool_response(endpoint, tool, arguments).await?;
    assert!(
        response.get("error").is_none(),
        "MCP JSON-RPC error: {}",
        response["error"]
    );
    assert!(
        !response["result"]["isError"].as_bool().unwrap_or(false),
        "MCP tool error: {}",
        response["result"]
    );
    Ok(response["result"].clone())
}

async fn call_tool_response(
    endpoint: &str,
    tool: &str,
    arguments: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = post_json(
        endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "exec",
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }),
    )
    .await?;
    assert_eq!(response.status, StatusCode::OK);
    Ok(response.body)
}

async fn call_method(
    endpoint: &str,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = post_json(
        endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "method",
            "method": method,
            "params": params
        }),
    )
    .await?;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.get("error").is_none(), "{}", response.body);
    Ok(response.body)
}

fn assert_mcp_error(response: &Value, code: i64, message: &str) {
    assert_eq!(response["error"]["code"], code, "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|actual| actual.contains(message)),
        "expected error containing {message:?}: {response}"
    );
}

struct JsonResponse {
    status: StatusCode,
    body: Value,
}

async fn post_json(
    endpoint: &str,
    mut body: Value,
) -> Result<JsonResponse, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let method = body["method"]
        .as_str()
        .ok_or("missing MCP method")?
        .to_owned();
    let params = body
        .as_object_mut()
        .ok_or("MCP request must be an object")?
        .entry("params")
        .or_insert_with(|| json!({}));
    let params = params
        .as_object_mut()
        .ok_or("MCP params must be an object")?;
    let meta = params
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("MCP params._meta must be an object")?;
    meta.insert(
        "io.modelcontextprotocol/protocolVersion".to_owned(),
        Value::String(::mcp::protocol::MCP_PROTOCOL_VERSION.to_owned()),
    );
    meta.insert(
        "io.modelcontextprotocol/clientCapabilities".to_owned(),
        json!({}),
    );
    meta.insert(
        "io.modelcontextprotocol/clientInfo".to_owned(),
        json!({ "name": "waltr-server-test", "version": "1.0.0" }),
    );
    let request_name = match method.as_str() {
        "tools/call" | "prompts/get" => body["params"]["name"].as_str(),
        "resources/read" => body["params"]["uri"].as_str(),
        _ => None,
    }
    .map(ToOwned::to_owned);

    let mut request = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header(
            "mcp-protocol-version",
            ::mcp::protocol::MCP_PROTOCOL_VERSION,
        )
        .header("mcp-method", &method);
    if let Some(request_name) = request_name {
        request = request.header("mcp-name", request_name);
    }
    let response = request.json(&body).send().await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let text = response.text().await?;
    let body = if content_type.starts_with("text/event-stream") {
        serde_json::from_str(
            text.lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .next_back()
                .ok_or("missing MCP SSE data")?,
        )?
    } else {
        serde_json::from_str(&text)?
    };
    Ok(JsonResponse { status, body })
}
