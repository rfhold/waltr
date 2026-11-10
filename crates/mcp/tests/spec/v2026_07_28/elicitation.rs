use std::{sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}}, time::Duration};

use mcp::{
    McpElicitationAction, McpElicitationCapabilities, McpElicitationFuture,
    McpElicitationRequest, McpElicitationResponse, McpToolCall, McpToolResult,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    server::{BoxFuture, ServerCapabilities, ServerContext, ServerHandler, ServerInfo, ServerResult},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::spec::support::SpecServer;

fn form() -> McpElicitationRequest {
    McpElicitationRequest::parse(&json!({
        "message": "Choose a name", "requestedSchema": {
            "type": "object", "properties": {"name": {"type": "string", "minLength": 1}}, "required": ["name"]
        }
    })).unwrap()
}

fn url() -> McpElicitationRequest {
    McpElicitationRequest::parse(&json!({
        "mode": "url", "message": "Connect account", "url": "https://example.com/connect"
    })).unwrap()
}

struct ElicitingServer {
    request: McpElicitationRequest,
    calls: AtomicUsize,
    effects: AtomicUsize,
}

impl ServerHandler for ElicitingServer {
    fn server_info(&self) -> ServerInfo { ServerInfo::new("elicitation-spec", "1") }
    fn capabilities(&self) -> ServerCapabilities { ServerCapabilities::new().tools() }

    fn list_tools(&self, _cursor: Option<String>, _context: ServerContext) -> BoxFuture<ServerResult<mcp::McpToolList>> {
        Box::pin(async {
            mcp::McpToolList::parse(&json!({"tools": [{
                "name": "interactive", "inputSchema": {
                    "type": "object", "properties": {"original": {"type": "boolean"}},
                    "required": ["original"]
                }
            }]})).map_err(|error| mcp::server::ServerError::internal(error.to_string()))
        })
    }

    fn call_tool(&self, call: McpToolCall, context: ServerContext) -> BoxFuture<ServerResult<McpToolResult>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(call.arguments, json!({"original": true}));
        let response = context.elicitation_response("question", &self.request);
        let result = match response {
            Ok(Some(response)) => {
                assert_eq!(context.request_state(), Some("opaque-state"));
                self.effects.fetch_add(1, Ordering::SeqCst);
                Ok(McpToolResult::text(serde_json::to_string(&response).unwrap()))
            }
            Ok(None) => context.elicitation_required("question", &self.request, Some("opaque-state"))
                .map(McpToolResult::new),
            Err(error) => Err(error),
        };
        Box::pin(async move { result })
    }
}

struct RespondingClient {
    response: McpElicitationResponse,
    tokens: Mutex<Vec<CancellationToken>>,
    wait: bool,
    entered: CancellationToken,
}

impl McpClientHandler for RespondingClient {
    fn elicit(&self, key: String, request: McpElicitationRequest, token: CancellationToken) -> McpElicitationFuture {
        assert_eq!(key, "question");
        request.validate().unwrap();
        self.tokens.lock().unwrap().push(token.clone());
        let entered = self.entered.clone();
        let response = self.response.clone();
        let wait = self.wait;
        Box::pin(async move {
            entered.cancel();
            if wait { token.cancelled().await; }
            Ok(response)
        })
    }
}

async fn connect(server: &SpecServer, handler: Arc<RespondingClient>, modes: McpElicitationCapabilities, timeout: Duration) -> mcp::Result<McpClient> {
    McpClient::connect(
        "elicitation-spec", McpTransportConfig::StreamableHttp { endpoint: server.endpoint(), bearer_token: None },
        handler, McpClientOptions::new("spec", "1", timeout).with_elicitation(modes), CancellationToken::new(),
    ).await
}

fn handler(response: McpElicitationResponse, wait: bool) -> Arc<RespondingClient> {
    Arc::new(RespondingClient { response, tokens: Mutex::new(Vec::new()), wait, entered: CancellationToken::new() })
}

struct OperationHandler {
    barrier: Arc<tokio::sync::Barrier>,
    response: McpElicitationResponse,
}
impl McpClientHandler for OperationHandler {
    fn elicit(&self, key: String, _: McpElicitationRequest, _: CancellationToken) -> McpElicitationFuture {
        assert_eq!(key, "question");
        let barrier = self.barrier.clone();
        let response = self.response.clone();
        Box::pin(async move { barrier.wait().await; Ok(response) })
    }
}

#[tokio::test]
async fn immutable_operation_views_isolate_identical_upstream_keys_and_capabilities() -> mcp::Result<()> {
    let implementation = Arc::new(ElicitingServer { request: form(), calls: AtomicUsize::new(0), effects: AtomicUsize::new(0) });
    let server = SpecServer::start(implementation.clone()).await?;
    let base_handler = handler(McpElicitationResponse { action: McpElicitationAction::Cancel, content: None }, false);
    let base = connect(&server, base_handler.clone(), McpElicitationCapabilities::default(), Duration::from_secs(5)).await?;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let response = |name| McpElicitationResponse { action: McpElicitationAction::Accept,
        content: Some(serde_json::from_value(json!({"name":name})).unwrap()) };
    let a_response = response("operation-a");
    let b_response = response("operation-b");
    let modes = McpElicitationCapabilities { form: true, url: false };
    let a = base.for_operation(Arc::new(OperationHandler { barrier: barrier.clone(), response: a_response.clone() }), modes);
    let b = base.for_operation(Arc::new(OperationHandler { barrier, response: b_response.clone() }), modes);
    let (a, b) = tokio::join!(
        a.call_tool(McpToolCall::new("interactive", json!({"original":true})), CancellationToken::new()),
        b.call_tool(McpToolCall::new("interactive", json!({"original":true})), CancellationToken::new()));
    assert_eq!(a?.format_text(), serde_json::to_string(&a_response).unwrap());
    assert_eq!(b?.format_text(), serde_json::to_string(&b_response).unwrap());
    assert_eq!(implementation.effects.load(Ordering::SeqCst), 2);
    assert_eq!(implementation.calls.load(Ordering::SeqCst), 4);
    assert!(base_handler.tokens.lock().unwrap().is_empty());
    assert!(base.call_tool(McpToolCall::new("interactive", json!({"original":true})), CancellationToken::new()).await.is_err());
    assert_eq!(implementation.effects.load(Ordering::SeqCst), 2);
    assert!(base_handler.tokens.lock().unwrap().is_empty());
    base.shutdown().await
}

#[tokio::test]
async fn form_and_url_accept_decline_cancel_resume_exactly_once() -> mcp::Result<()> {
    for request in [form(), url()] {
        for action in [McpElicitationAction::Accept, McpElicitationAction::Decline, McpElicitationAction::Cancel] {
            let content = if action == McpElicitationAction::Accept && matches!(&request, McpElicitationRequest::Form { .. }) {
                Some(serde_json::from_value(json!({"name": "octocat"})).unwrap())
            } else { None };
            let response = McpElicitationResponse { action, content };
            let implementation = Arc::new(ElicitingServer { request: request.clone(), calls: AtomicUsize::new(0), effects: AtomicUsize::new(0) });
            let server = SpecServer::start(implementation.clone()).await?;
            let handler = handler(response.clone(), false);
            let client = connect(&server, handler.clone(), McpElicitationCapabilities { form: true, url: true }, Duration::from_secs(5)).await?;
            let result = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await?;
            assert_eq!(result.format_text(), serde_json::to_string(&response).unwrap());
            assert_eq!(implementation.calls.load(Ordering::SeqCst), 2);
            assert_eq!(implementation.effects.load(Ordering::SeqCst), 1);
            assert!(handler.tokens.lock().unwrap().iter().all(CancellationToken::is_cancelled));
            client.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn operation_cancellation_abandons_input_without_continuation() -> mcp::Result<()> {
    let implementation = Arc::new(ElicitingServer { request: form(), calls: AtomicUsize::new(0), effects: AtomicUsize::new(0) });
    let server = SpecServer::start(implementation.clone()).await?;
    let handler = handler(McpElicitationResponse { action: McpElicitationAction::Cancel, content: None }, true);
    let client = connect(&server, handler.clone(), McpElicitationCapabilities { form: true, url: false }, Duration::from_secs(5)).await?;
    let cancellation = CancellationToken::new();
    let call = async {
        let result = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), cancellation.clone()).await;
        assert!(handler.entered.is_cancelled(), "operation ended before callback entry: {result:?}");
        result
    };
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(10), handler.entered.cancelled())
            .await.expect("elicitation callback did not start before cancellation deadline");
        cancellation.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(call, cancel) })
        .await.expect("elicitation operation did not finish after cancellation");
    assert!(result.unwrap_err().to_string().contains("canceled"));
    assert_eq!(implementation.calls.load(Ordering::SeqCst), 1);
    assert_eq!(implementation.effects.load(Ordering::SeqCst), 0);
    assert!(handler.tokens.lock().unwrap().iter().all(CancellationToken::is_cancelled));
    client.shutdown().await
}

#[tokio::test]
async fn elicitation_timeout_does_not_resend_the_original_operation() -> mcp::Result<()> {
    let implementation = Arc::new(ElicitingServer { request: url(), calls: AtomicUsize::new(0), effects: AtomicUsize::new(0) });
    let server = SpecServer::start(implementation.clone()).await?;
    let handler = handler(McpElicitationResponse { action: McpElicitationAction::Accept, content: None }, true);
    let client = connect(&server, handler.clone(), McpElicitationCapabilities { form: false, url: true }, Duration::from_secs(1)).await?;
    let error = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(implementation.calls.load(Ordering::SeqCst), 1);
    assert_eq!(implementation.effects.load(Ordering::SeqCst), 0);
    assert!(handler.tokens.lock().unwrap().iter().all(CancellationToken::is_cancelled));
    client.shutdown().await
}

#[tokio::test]
async fn server_rejects_unadvertised_modes_without_invoking_client() -> mcp::Result<()> {
    let implementation = Arc::new(ElicitingServer { request: url(), calls: AtomicUsize::new(0), effects: AtomicUsize::new(0) });
    let server = SpecServer::start(implementation.clone()).await?;
    let handler = handler(McpElicitationResponse { action: McpElicitationAction::Accept, content: None }, false);
    let client = connect(&server, handler.clone(), McpElicitationCapabilities { form: true, url: false }, Duration::from_secs(5)).await?;
    let error = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("mode was not advertised"));
    assert!(!handler.entered.is_cancelled());
    assert_eq!(implementation.effects.load(Ordering::SeqCst), 0);
    client.shutdown().await
}

#[tokio::test]
async fn shutdown_cancels_the_live_callback_without_a_resume() -> mcp::Result<()> {
    let implementation = Arc::new(ElicitingServer { request: form(), calls: AtomicUsize::new(0), effects: AtomicUsize::new(0) });
    let server = SpecServer::start(implementation.clone()).await?;
    let handler = handler(McpElicitationResponse { action: McpElicitationAction::Cancel, content: None }, true);
    let client = connect(&server, handler.clone(), McpElicitationCapabilities { form: true, url: false }, Duration::from_secs(5)).await?;
    let call = async {
        let result = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await;
        assert!(handler.entered.is_cancelled(), "operation ended before callback entry: {result:?}");
        result
    };
    let shutdown = async {
        tokio::time::timeout(Duration::from_secs(10), handler.entered.cancelled())
            .await.expect("elicitation callback did not start before shutdown deadline");
        client.shutdown().await
    };
    let (result, shutdown) = tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(call, shutdown) })
        .await.expect("elicitation operation did not finish after shutdown");
    shutdown?;
    assert!(result.unwrap_err().to_string().contains("closed during elicitation"));
    assert!(handler.tokens.lock().unwrap().iter().all(CancellationToken::is_cancelled));
    assert_eq!(implementation.calls.load(Ordering::SeqCst), 1);
    assert_eq!(implementation.effects.load(Ordering::SeqCst), 0);
    Ok(())
}

#[derive(Clone)]
struct RawServer {
    result: serde_json::Value,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn raw_response(
    axum::extract::State(state): axum::extract::State<RawServer>,
    axum::Json(request): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let result = if request["method"] == "server/discover" {
        json!({"resultType": "complete", "supportedVersions": [mcp::protocol::MCP_PROTOCOL_VERSION],
            "capabilities": {"tools": {}}, "ttlMs": 0, "cacheScope": "private"})
    } else {
        state.requests.lock().unwrap().push(request.clone());
        state.result.clone()
    };
    axum::Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
}

async fn raw_client(result: serde_json::Value, handler: Arc<RespondingClient>, modes: McpElicitationCapabilities) -> mcp::Result<(McpClient, Arc<Mutex<Vec<serde_json::Value>>>, tokio::task::JoinHandle<()>)> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new().route("/mcp", axum::routing::post(raw_response))
        .with_state(RawServer { result, requests: requests.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let task = tokio::spawn(async move { drop(axum::serve(listener, app).await); });
    let client = McpClient::connect(
        "raw-elicitation-spec", McpTransportConfig::StreamableHttp { endpoint, bearer_token: None }, handler,
        McpClientOptions::new("spec", "1", Duration::from_secs(5)).with_elicitation(modes), CancellationToken::new(),
    ).await?;
    Ok((client, requests, task))
}

#[tokio::test]
async fn hostile_input_batches_fail_before_any_ui_or_continuation() -> mcp::Result<()> {
    let valid = form().to_input_request()?;
    for (result, modes) in [
        (json!({"resultType": "input_required", "inputRequests": {"question": valid.clone()}, "requestState": {"invalid": true}}), McpElicitationCapabilities { form: true, url: true }),
        (json!({"resultType": "input_required", "inputRequests": {"question": valid.clone(), "unsupported": {"method": "sampling/createMessage"}}}), McpElicitationCapabilities { form: true, url: true }),
        (json!({"resultType": "input_required", "inputRequests": {"question": valid.clone()}}), McpElicitationCapabilities::default()),
        (json!({"resultType": "input_required", "inputRequests": {"question": url().to_input_request()?}}), McpElicitationCapabilities { form: true, url: false }),
        (json!({"resultType": "input_required", "inputRequests": {"question": {"method": "elicitation/create", "params": {"message": "invalid"}}}}), McpElicitationCapabilities { form: true, url: true }),
    ] {
        let handler = handler(McpElicitationResponse { action: McpElicitationAction::Cancel, content: None }, false);
        let (client, requests, task) = raw_client(result, handler.clone(), modes).await?;
        assert!(client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await.is_err());
        assert!(!handler.entered.is_cancelled());
        assert_eq!(requests.lock().unwrap().len(), 1);
        client.shutdown().await?;
        task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn invalid_callback_content_never_resumes_the_operation() -> mcp::Result<()> {
    let handler = handler(McpElicitationResponse { action: McpElicitationAction::Accept, content: None }, false);
    let (client, requests, task) = raw_client(
        json!({"resultType": "input_required", "inputRequests": {"question": form().to_input_request()?}}),
        handler.clone(), McpElicitationCapabilities { form: true, url: false },
    ).await?;
    let error = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("accepted form omitted content"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(handler.entered.is_cancelled());
    assert!(handler.tokens.lock().unwrap().iter().all(CancellationToken::is_cancelled));
    client.shutdown().await?;
    task.abort();
    Ok(())
}

#[tokio::test]
async fn state_only_continuations_are_bounded_and_keep_state_opaque() -> mcp::Result<()> {
    let handler = handler(McpElicitationResponse { action: McpElicitationAction::Cancel, content: None }, false);
    let (client, requests, task) = raw_client(
        json!({"resultType": "input_required", "requestState": "not-json:opaque"}),
        handler.clone(), McpElicitationCapabilities::default(),
    ).await?;
    let error = client.call_tool(McpToolCall::new("interactive", json!({"original": true})), CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("round limit"));
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 32);
    let ids: std::collections::HashSet<_> = requests.iter().map(|request| request["id"].to_string()).collect();
    assert_eq!(ids.len(), requests.len());
    assert!(requests[0]["params"].get("requestState").is_none());
    for request in &requests[1..] {
        assert_eq!(request["params"]["requestState"], "not-json:opaque");
        assert_eq!(request["params"]["arguments"], json!({"original": true}));
        assert!(request["params"].get("inputResponses").is_none());
    }
    assert!(!handler.entered.is_cancelled());
    client.shutdown().await?;
    task.abort();
    Ok(())
}

#[test]
fn wire_shapes_capabilities_and_schema_validation() -> mcp::Result<()> {
    let implicit = McpElicitationCapabilities::parse(&json!({"elicitation": {}}))?;
    assert!(implicit.form && !implicit.url);
    assert_eq!(McpElicitationCapabilities::default().to_value(), None);
    assert!(McpElicitationCapabilities::parse(&json!({"elicitation": {"form": true}})).is_err());
    assert!(matches!(form(), McpElicitationRequest::Form { .. }));
    assert_eq!(url().to_input_request()?, json!({"method": "elicitation/create", "params": {
        "mode": "url", "message": "Connect account", "url": "https://example.com/connect"
    }}));
    for content in [json!({}), json!({"name": ""}), json!({"name": 1}), json!({"name": "ok", "extra": true})] {
        let response: McpElicitationResponse = serde_json::from_value(json!({"action": "accept", "content": content}))?;
        assert!(form().validate_response(&response).is_err());
    }
    let with_content: McpElicitationResponse = serde_json::from_value(json!({"action": "accept", "content": {"name": "ok"}}))?;
    assert!(url().validate_response(&with_content).is_err());
    assert!(McpElicitationRequest::parse(&json!({"mode": "url", "message": "x", "url": "/relative"})).is_err());
    assert!(McpElicitationRequest::parse(&json!({"message": "x", "requestedSchema": {
        "type": "object", "properties": {"nested": {"type": "object"}}
    }})).is_err());
    let selections = McpElicitationRequest::parse(&json!({"message": "x", "requestedSchema": {
        "type": "object", "properties": {"choices": {"type": "array", "items": {"anyOf": [{"const": "red", "title": "Red"}]}, "minItems": 1}}, "required": ["choices"]
    }}))?;
    for (choices, valid) in [(json!(["red"]), true), (json!(["blue"]), false), (json!([]), false)] {
        let response = serde_json::from_value(json!({"action": "accept", "content": {"choices": choices}}))?;
        assert_eq!(selections.validate_response(&response).is_ok(), valid);
    }
    Ok(())
}
