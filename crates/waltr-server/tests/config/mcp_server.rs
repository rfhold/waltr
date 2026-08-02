use crate::support::grpc::{config_client, get_config};
use waltr::grpc::waltr::config::v1::McpServerType;
use waltr_agent::{
    MemorySessionStore, ModelApi, PendingTimelineEvent, SessionCoordinator, SessionId,
    SessionProviderRuntimeSettings, SessionRuntimeSettings, SessionStore, TimelineContent,
    TimelineEventKind, TimelineStatus,
};
use waltr_server::{
    ServerApplication, build_app_with_services,
    config_service::ConfigService,
    persistence::{DatabaseConfig, Persistence},
    session,
    test_support::{blocked_cleanup_application, default_config_builder, setup_server, temp_root},
};
use mcp::server::{
    StreamableHttpInspectSession, StreamableHttpSessionKey, StreamableHttpSessionOwner,
    StreamableHttpSessionRequest,
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{io, net::SocketAddr, sync::atomic::Ordering, time::SystemTime};
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
        .create_with_id_and_metadata_values(id.clone(), Default::default())
        .await?;
    append_status(&store, &id, TimelineStatus::Finished).await?;
    let runnable_id = SessionId::new("mcp-runnable-session");
    coordinator
        .create_with_id_and_runtime_settings(
            runnable_id.clone(),
            Some(SessionRuntimeSettings {
                provider: SessionProviderRuntimeSettings {
                    key: "test".to_owned(),
                    api: ModelApi::OpenAiResponses,
                    url: "http://127.0.0.1:9".to_owned(),
                    model_id: "test-model".to_owned(),
                    model_cost: None,
                    headers: Default::default(),
                    oauth: None,
                    reasoning_effort: None,
                },
                mcp_servers: Vec::new(),
                mcp_trace_propagation: false,
            }),
            Default::default(),
        )
        .await?;
    let app = build_app_with_services(
        ConfigService::new(root.clone(), default_config_builder())?,
        persistence,
        {
            let coordinator = coordinator.clone();
            move |routes, config, credential_store| {
                session::add_sessions_service(routes, coordinator, config, credential_store, None)
            }
        },
    )
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
    for field in ["wipe", "confirm", "id"] {
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

    let waited = call_tool(
        &router.endpoint,
        "sessions",
        json!({
            "action": "wait",
            "input": { "id": id.as_str(), "jq": ".status == \"finished\"", "timeout_seconds": 1 }
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
        call_tool_response(
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
        .create_with_id_and_metadata_values(id.clone(), Default::default())
        .await?;
    append_status(&store, &id, TimelineStatus::StreamingProvider).await?;

    let app = build_app_with_services(
        ConfigService::new(root.clone(), default_config_builder())?,
        persistence,
        {
            let coordinator = coordinator.clone();
            move |routes, config, credential_store| {
                session::add_sessions_service(routes, coordinator, config, credential_store, None)
            }
        },
    )
    .await?;
    let router = TestRouter::start(app).await?;

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

#[tokio::test]
async fn reconstructed_router_uses_persisted_mcp_sessions() -> Result<(), Box<dyn std::error::Error>>
{
    let root = temp_root();
    let database =
        DatabaseConfig::from_url(&format!("sqlite://{}", root.join("waltr.sqlite3").display()))?;
    let first_persistence = Persistence::initialize(database.clone()).await?;
    let first = start_router(root.clone(), first_persistence.clone()).await?;
    let client = reqwest::Client::new();
    let session_id = initialized_session(&client, &first.endpoint).await?;
    assert!(matches!(
        first_persistence
            .mcp_session_store()
            .inspect_session(session_request(&session_id))
            .await?,
        StreamableHttpInspectSession::Session(_)
    ));
    first.shutdown().await?;
    drop(first_persistence);

    let second_persistence = Persistence::initialize(database).await?;
    let second = start_router(root.clone(), second_persistence.clone()).await?;
    assert!(matches!(
        second_persistence
            .mcp_session_store()
            .inspect_session(session_request(&session_id))
            .await?,
        StreamableHttpInspectSession::Session(_)
    ));
    second.shutdown().await?;
    std::fs::remove_dir_all(root)?;
    Ok(())
}

fn session_request(session_id: &str) -> StreamableHttpSessionRequest {
    StreamableHttpSessionRequest {
        key: StreamableHttpSessionKey {
            session_id: session_id.to_owned(),
            owner: StreamableHttpSessionOwner::Anonymous,
        },
        presented_protocol_version: Some(mcp::protocol::MCP_PROTOCOL_VERSION.to_owned()),
        now: SystemTime::now(),
    }
}

async fn start_router(
    root: std::path::PathBuf,
    persistence: Persistence,
) -> Result<TestRouter, Box<dyn std::error::Error>> {
    let app = build_app_with_services(
        ConfigService::new(root, default_config_builder())?,
        persistence,
        |routes, config, credential_store| {
            session::add_memory_service(routes, config, credential_store)
        },
    )
    .await?;
    TestRouter::start(app).await.map_err(Into::into)
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
    let (app, probe) = blocked_cleanup_application();
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
    let (app, probe) = blocked_cleanup_application();
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

fn assert_mcp_error(response: Value, code: i64, message: &str) {
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
    body: Value,
) -> Result<JsonResponse, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let session_id = initialized_session(&client, endpoint).await?;

    let response = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header(
            "mcp-protocol-version",
            ::mcp::protocol::MCP_PROTOCOL_VERSION,
        )
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let body = response.json::<Value>().await?;
    Ok(JsonResponse { status, body })
}

async fn initialized_session(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let initialize = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": ::mcp::protocol::MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "waltr-server-test", "version": "1.0.0" }
            }
        }))
        .send()
        .await?;
    assert_eq!(initialize.status(), StatusCode::OK);
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .ok_or("missing MCP session id")?
        .to_str()?
        .to_owned();
    drop(initialize.json::<Value>().await?);

    drop(
        client
            .post(endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .header(
                "mcp-protocol-version",
                ::mcp::protocol::MCP_PROTOCOL_VERSION,
            )
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }))
            .send()
            .await?
            .bytes()
            .await?,
    );

    Ok(session_id)
}
