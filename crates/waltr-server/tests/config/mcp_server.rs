use crate::support::grpc::{config_client, get_config};
use waltr::grpc::waltr::config::v1::McpServerType;
use waltr_server::{
    ServerApplication, build_app_with_services,
    config_service::ConfigService,
    persistence::{DatabaseConfig, Persistence},
    session,
    test_support::{setup_server, temp_root},
};
use mcp::server::{
    StreamableHttpInspectSession, StreamableHttpSessionKey, StreamableHttpSessionOwner,
    StreamableHttpSessionRequest,
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{io, net::SocketAddr, time::SystemTime};
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
        ConfigService::new(root)?,
        persistence,
        |routes, config, _credential_store| session::add_memory_service(routes, config),
    );
    TestRouter::start(app).await.map_err(Into::into)
}

struct TestRouter {
    endpoint: String,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<io::Result<()>>,
}

impl TestRouter {
    async fn start(app: ServerApplication) -> io::Result<Self> {
        let (app, listener, addr) = app.bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
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

async fn call_exec(endpoint: &str, arguments: Value) -> Result<Value, Box<dyn std::error::Error>> {
    let response = post_json(
        endpoint,
        json!({
            "jsonrpc": "2.0",
            "id": "exec",
            "method": "tools/call",
            "params": { "name": "exec", "arguments": arguments }
        }),
    )
    .await?;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        response.body.get("error").is_none(),
        "MCP JSON-RPC error: {}",
        response.body["error"]
    );
    Ok(response.body["result"].clone())
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
