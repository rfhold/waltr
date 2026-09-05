use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, header},
    routing::post,
};
use mcp::{
    McpRoot, McpToolCall, McpToolDefinition, McpToolList, McpToolResult,
    client::{
        McpClient, McpClientHandler, McpClientOptions, McpTransportConfig, McpTransportLimits,
        McpTransportOptions,
    },
    protocol::{MCP_PROTOCOL_VERSION, MCP_PROTOCOL_VERSION_META_KEY},
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct RootsHandler;

impl McpClientHandler for RootsHandler {
    fn roots(&self) -> Vec<McpRoot> {
        vec![McpRoot::new(
            "file:///workspace",
            Some("Workspace".to_owned()),
        )]
    }
}

#[tokio::test]
async fn streamable_http_uses_supplied_client_and_preserves_default_client() -> mcp::Result<()> {
    let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let app = Router::new()
        .route("/mcp", post(header_mock_server))
        .with_state(Arc::clone(&captured));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });

    let mut default_headers = HeaderMap::new();
    default_headers.insert("x-mcp-test-client", "supplied".parse().unwrap());
    let http_client = reqwest::Client::builder()
        .default_headers(default_headers)
        .build()?;
    let client = McpClient::connect_with_transport_options(
        "custom-http-client-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: endpoint.clone(),
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        McpTransportOptions::default().with_streamable_http_client(http_client),
        CancellationToken::new(),
    )
    .await?;
    client.shutdown().await?;

    let client = McpClient::connect(
        "default-http-client-spec",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    client.shutdown().await?;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["x-mcp-test-client"], "supplied");
    assert!(!captured[1].contains_key("x-mcp-test-client"));
    Ok(())
}

#[tokio::test]
async fn http_client_encodes_names_and_mirrors_nested_tool_parameters() -> mcp::Result<()> {
    let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let app = Router::new()
        .route("/mcp", post(header_mock_server))
        .with_state(Arc::clone(&captured));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });

    let cancellation = CancellationToken::new();
    let client = McpClient::connect(
        "header-spec",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    let tools = client.list_tools(cancellation.clone()).await?;
    assert_eq!(tools.len(), 1);
    client
        .call_tool(
            McpToolCall::new(
                "caf\u{e9}",
                json!({
                    "tenant": "M\u{fc}nchen",
                    "routing": { "shard": 42, "urgent": true }
                }),
            ),
            cancellation,
        )
        .await?;

    let call = captured
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("tools/call headers");
    assert_eq!(call["mcp-name"], "=?base64?Y2Fmw6k=?=");
    assert_eq!(call["mcp-param-tenant"], "=?base64?TcO8bmNoZW4=?=");
    assert_eq!(call["mcp-param-shard"], "42");
    assert_eq!(call["mcp-param-urgent"], "true");
    Ok(())
}

#[tokio::test]
async fn client_merges_progress_metadata_and_completes_roots_mrtr_retry() -> mcp::Result<()> {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let state = Arc::clone(&requests);
    let app = Router::new()
        .route("/mcp", post(mock_server))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });

    let cancellation = CancellationToken::new();
    let client = McpClient::connect(
        "mrtr-spec",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    let result = client
        .call_tool(
            McpToolCall::new("needs-roots", json!({ "value": "x" }))
                .with_progress_token(json!("progress-1")),
            cancellation,
        )
        .await?;
    assert_eq!(result.format_text(), "done");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert_eq!(
            request["params"]["_meta"][MCP_PROTOCOL_VERSION_META_KEY],
            MCP_PROTOCOL_VERSION
        );
    }
    assert_eq!(
        requests[1]["params"]["_meta"]["progressToken"],
        "progress-1"
    );
    assert_ne!(requests[1]["id"], requests[2]["id"]);
    assert_eq!(requests[2]["params"]["requestState"], "round-1");
    assert_eq!(
        requests[2]["params"]["inputResponses"]["roots"]["resultType"],
        "complete"
    );
    assert_eq!(
        requests[2]["params"]["inputResponses"]["roots"]["roots"][0]["uri"],
        "file:///workspace"
    );
    drop(requests);
    Ok(())
}

struct InteropMrtrServer {
    calls: AtomicUsize,
}

struct LargeImageServer;

impl ServerHandler for LargeImageServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("large-image-spec", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: vec![McpToolDefinition {
                    name: "image".to_owned(),
                    title: None,
                    description: None,
                    icons: Vec::new(),
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    annotations: None,
                    meta: None,
                }],
                next_cursor: None,
            })
        })
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        Box::pin(async {
            Ok(McpToolResult::new(json!({
                "content": [{
                    "type": "image",
                    "data": "a".repeat(96 * 1024),
                    "mimeType": "image/png"
                }]
            })))
        })
    }
}

impl ServerHandler for InteropMrtrServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("mrtr-interop-spec", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new().tools()
    }

    fn list_tools(
        &self,
        _cursor: Option<String>,
        _context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolList>> {
        Box::pin(async {
            Ok(McpToolList {
                tools: vec![McpToolDefinition {
                    name: "needs-roots".to_owned(),
                    title: None,
                    description: None,
                    icons: Vec::new(),
                    input_schema: json!({ "type": "object" }),
                    output_schema: None,
                    annotations: None,
                    meta: None,
                }],
                next_cursor: None,
            })
        })
    }

    fn call_tool(
        &self,
        _call: McpToolCall,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpToolResult>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call == 0 {
                return Ok(McpToolResult::new(json!({
                    "resultType": "input_required",
                    "inputRequests": { "roots": { "method": "roots/list" } },
                    "requestState": "round-1"
                })));
            }
            if context.request_state() != Some("round-1")
                || context
                    .input_responses()
                    .and_then(|value| value.pointer("/roots/roots/0/uri"))
                    .and_then(Value::as_str)
                    != Some("file:///workspace")
            {
                return Err(ServerError::internal(
                    "MRTR retry fields were not preserved",
                ));
            }
            Ok(McpToolResult::text("interoperable"))
        })
    }
}

#[tokio::test]
async fn client_and_server_interoperate_across_roots_mrtr_retry() -> mcp::Result<()> {
    let server = crate::spec::support::SpecServer::start(Arc::new(InteropMrtrServer {
        calls: AtomicUsize::new(0),
    }))
    .await?;
    let cancellation = CancellationToken::new();
    let client = McpClient::connect(
        "mrtr-interop-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    let result = client
        .call_tool(McpToolCall::new("needs-roots", json!({})), cancellation)
        .await?;
    assert_eq!(result.format_text(), "interoperable");
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_transport_receives_large_sse_tool_result() -> mcp::Result<()> {
    let server = crate::spec::support::SpecServer::start(Arc::new(LargeImageServer)).await?;
    let cancellation = CancellationToken::new();
    let client = McpClient::connect_with_transport_limits(
        "large-image-spec",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        McpTransportLimits::new(256 * 1024, 256 * 1024),
        cancellation.clone(),
    )
    .await?;
    let result = client
        .call_tool(McpToolCall::new("image", json!({})), cancellation)
        .await?;
    assert_eq!(
        result.raw["content"][0]["data"].as_str().unwrap().len(),
        96 * 1024
    );
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn abrupt_subscription_eof_fails_and_shutdown_releases_client_graph() -> mcp::Result<()> {
    let calls = Arc::new(Mutex::new(0_u8));
    let app = Router::new()
        .route("/mcp", post(abrupt_subscription_server))
        .with_state(Arc::clone(&calls));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });

    let handler = Arc::new(RootsHandler);
    let weak_handler = Arc::downgrade(&handler);
    let cancellation = CancellationToken::new();
    let client = McpClient::connect(
        "subscription-eof-spec",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        handler.clone(),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.listen_for_list_changes(cancellation),
    )
    .await
    .expect("abrupt subscription EOF must not hang")
    .expect_err("abrupt subscription EOF must fail");
    assert!(
        error
            .to_string()
            .contains("ended before a terminal response"),
        "unexpected subscription error: {error}"
    );

    client.shutdown().await?;
    drop(client);
    drop(handler);
    assert!(weak_handler.upgrade().is_none());
    Ok(())
}

#[tokio::test]
async fn client_rejects_malformed_discovery_results() -> mcp::Result<()> {
    let cases = [
        (
            json!({
                "resultType": "input_required",
                "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": {}, "ttlMs": 0, "cacheScope": "private"
            }),
            "unsupported MCP discovery resultType",
        ),
        (
            json!({ "capabilities": {}, "ttlMs": 0, "cacheScope": "private" }),
            "does not advertise MCP protocol version",
        ),
        (
            json!({
                "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": [], "ttlMs": 0, "cacheScope": "private"
            }),
            "capabilities must be an object",
        ),
        (
            json!({
                "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": {}, "ttlMs": -1, "cacheScope": "private"
            }),
            "ttlMs must be a non-negative integer",
        ),
        (
            json!({
                "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": {}, "ttlMs": 0, "cacheScope": "shared"
            }),
            "cacheScope must be `public` or `private`",
        ),
    ];

    for (result, expected) in cases {
        let app = Router::new()
            .route("/mcp", post(discovery_response_server))
            .with_state(result);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/mcp", listener.local_addr()?);
        tokio::spawn(async move {
            drop(axum::serve(listener, app).await);
        });

        let cancellation = CancellationToken::new();
        let error = McpClient::connect(
            "invalid-discovery-spec",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            Arc::new(RootsHandler),
            McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
            cancellation,
        )
        .await
        .expect_err("malformed discovery must fail");
        assert!(
            error.to_string().contains(expected),
            "expected `{expected}` in `{error}`"
        );
    }
    Ok(())
}

#[tokio::test]
async fn malformed_request_sse_is_reported_to_the_originating_request() -> mcp::Result<()> {
    let app = Router::new().route("/mcp", post(malformed_sse_server));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });
    let cancellation = CancellationToken::new();
    let client = McpClient::connect(
        "malformed-sse-spec",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RootsHandler),
        McpClientOptions::new("spec-client", "1.0.0", Duration::from_secs(2)),
        cancellation.clone(),
    )
    .await?;
    let error = client
        .list_tools(cancellation)
        .await
        .expect_err("malformed SSE must fail the request");
    assert!(
        error.to_string().contains("expected ident"),
        "unexpected error: {error}"
    );
    client.shutdown().await?;
    Ok(())
}

async fn discovery_response_server(
    State(result): State<Value>,
    Json(request): Json<Value>,
) -> Json<Value> {
    Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
}

async fn malformed_sse_server(Json(request): Json<Value>) -> Response<Body> {
    if request["method"] == "server/discover" {
        let body = json!({
            "jsonrpc": "2.0", "id": request["id"], "result": {
                "resultType": "complete", "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": { "tools": {} }, "ttlMs": 0, "cacheScope": "private"
            }
        });
        return Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("valid response");
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from("data: not-json\n\n"))
        .expect("valid response")
}

async fn mock_server(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push(request.clone());
    let id = request["id"].clone();
    let result = match request["method"].as_str().unwrap() {
        "server/discover" => json!({
            "resultType": "complete", "supportedVersions": [MCP_PROTOCOL_VERSION],
            "capabilities": { "tools": {} }, "ttlMs": 0, "cacheScope": "private"
        }),
        "tools/call" if request["params"].get("inputResponses").is_none() => json!({
            "resultType": "input_required",
            "inputRequests": { "roots": { "method": "roots/list" } },
            "requestState": "round-1"
        }),
        "tools/call" => json!({
            "resultType": "complete", "content": [{ "type": "text", "text": "done" }]
        }),
        method => panic!("unexpected method {method}"),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn header_mock_server(
    State(captured): State<Arc<Mutex<Vec<HeaderMap>>>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    captured.lock().unwrap().push(headers);
    let id = request["id"].clone();
    let result = match request["method"].as_str().unwrap() {
        "server/discover" => json!({
            "resultType": "complete",
            "supportedVersions": [MCP_PROTOCOL_VERSION],
            "capabilities": { "tools": {} },
            "ttlMs": 0,
            "cacheScope": "private"
        }),
        "tools/list" => json!({
            "resultType": "complete",
            "ttlMs": 0,
            "cacheScope": "private",
            "tools": [{
                "name": "caf\u{e9}",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "tenant": { "type": "string", "x-mcp-header": "Tenant" },
                        "routing": {
                            "type": "object",
                            "properties": {
                                "shard": { "type": "integer", "x-mcp-header": "Shard" },
                                "urgent": { "type": "boolean", "x-mcp-header": "Urgent" }
                            }
                        }
                    }
                }
            }]
        }),
        "tools/call" => json!({
            "resultType": "complete",
            "content": [{ "type": "text", "text": "ok" }]
        }),
        method => panic!("unexpected method {method}"),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn abrupt_subscription_server(
    State(calls): State<Arc<Mutex<u8>>>,
    Json(request): Json<Value>,
) -> Response<Body> {
    let call = {
        let mut calls = calls.lock().unwrap();
        *calls += 1;
        *calls
    };
    if call == 1 {
        let body = json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "resultType": "complete",
                "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": { "tools": { "listChanged": true } },
                "ttlMs": 0,
                "cacheScope": "private"
            }
        });
        return Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
    }
    let acknowledgment = json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {
            "notifications": { "toolsListChanged": true },
            "_meta": {
                "io.modelcontextprotocol/subscriptionId": request["id"]
            }
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(format!("data: {acknowledgment}\n\n")))
        .unwrap()
}
