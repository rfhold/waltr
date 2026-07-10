use crate::support::grpc::{config_client, get_config};
use waltr::grpc::waltr::config::v1::McpServerType;
use waltr_server::test_support::setup_server;
use reqwest::StatusCode;
use serde_json::{Value, json};

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
