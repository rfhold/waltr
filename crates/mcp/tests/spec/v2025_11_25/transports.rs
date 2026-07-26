// Linear transport probes and one-field config variants are clearer in-place.
#![allow(clippy::field_reassign_with_default, clippy::too_many_lines)]

use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use futures_util::StreamExt;
use mcp::{
    McpToolCall, McpToolDefinition, McpToolResult,
    client::{
        McpBearerProvider, McpBearerProviderError, McpClient, McpClientHandler, McpClientOptions,
        McpTransportConfig, McpTransportError, McpTransportErrorKind,
    },
    protocol::MCP_PROTOCOL_VERSION,
    server::{
        BoxFuture, InMemoryStreamableHttpSessionStore, ServerCapabilities, ServerContext,
        ServerError, ServerHandler, ServerInfo, ServerResult, StreamableHttpAppendEvents,
        StreamableHttpAppendEventsRequest, StreamableHttpEventId, StreamableHttpInspectSession,
        StreamableHttpOptions, StreamableHttpSessionKey, StreamableHttpSessionLifecycle,
        StreamableHttpSessionOwner, StreamableHttpSessionRequest, StreamableHttpSessionStore,
        StreamableHttpSessionStoreConfig,
    },
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify, Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

use crate::spec::support::{SpecServer, initialized_session};

struct TransportServer;

#[mcp::mcp_server(
    name = "mcp-2025-11-25-transport",
    version = "1.0.0",
    capabilities(logging, tools)
)]
impl TransportServer {
    #[tool(name = "direct-sse", definition = direct_sse_tool_definition())]
    async fn direct_sse(_call: McpToolCall, context: ServerContext) -> ServerResult<McpToolResult> {
        context
            .notify("notifications/direct", Some(json!({ "direct": true })))
            .await
            .map_err(|error| ServerError::internal(error.to_string()))?;
        Ok(McpToolResult::text("direct response"))
    }

    #[notification(method = "notifications/initialized")]
    async fn initialized(
        _notification: mcp::protocol::JsonRpcNotification,
        context: ServerContext,
    ) {
        context
            .notify(
                "notifications/message",
                Some(json!({
                    "level": "info",
                    "data": "queued-for-get-polling"
                })),
            )
            .await
            .expect("queue notification");
    }
}

fn direct_sse_tool_definition() -> McpToolDefinition {
    McpToolDefinition {
        name: "direct-sse".to_owned(),
        title: None,
        description: None,
        icons: Vec::new(),
        input_schema: json!({ "type": "object", "properties": {} }),
        output_schema: None,
        task_support: None,
        annotations: None,
    }
}

#[tokio::test]
async fn streamable_http_get_polls_queued_sse_messages_end_to_end() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = McpClient::connect(
        "polling-server",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(RecordingClient { tx }),
        McpClientOptions::new("polling-client", "0", Duration::from_secs(5)),
        CancellationToken::new(),
    )
    .await?;

    let received = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("client receives GET-polled notification")
        .expect("notification value");
    assert_eq!(received["data"], "queued-for-get-polling");

    client.shutdown().await?;
    Ok(())
}

struct RecordingClient {
    tx: mpsc::UnboundedSender<Value>,
}

impl McpClientHandler for RecordingClient {
    fn log_message(&self, params: Value) {
        drop(self.tx.send(params));
    }
}

struct ClientTransportProbe {
    deletes: AtomicUsize,
    delete_headers: Mutex<Vec<(String, String)>>,
    request_bearers: Mutex<Vec<(axum::http::Method, String)>>,
    get_attempts: Mutex<Vec<Instant>>,
    get_started: Notify,
    delete_status: StatusCode,
    omit_initialize_version: bool,
    retry_ms: Option<u64>,
    open_event: bool,
    fail_first_get_body: AtomicBool,
    fail_first_get_decode: AtomicBool,
    initialized_failure: InitializedFailure,
    initialized_started: Notify,
}

#[derive(Clone, Copy)]
enum InitializedFailure {
    None,
    Unauthorized,
    ServerError,
    HeaderTimeout,
    PendingBody,
    OversizedBody,
    BodyError,
}

impl ClientTransportProbe {
    fn new(delete_status: StatusCode) -> Self {
        Self {
            deletes: AtomicUsize::new(0),
            delete_headers: Mutex::new(Vec::new()),
            request_bearers: Mutex::new(Vec::new()),
            get_attempts: Mutex::new(Vec::new()),
            get_started: Notify::new(),
            delete_status,
            omit_initialize_version: false,
            retry_ms: None,
            open_event: false,
            fail_first_get_body: AtomicBool::new(false),
            fail_first_get_decode: AtomicBool::new(false),
            initialized_failure: InitializedFailure::None,
            initialized_started: Notify::new(),
        }
    }
}

async fn start_client_transport_probe(
    probe: Arc<ClientTransportProbe>,
) -> mcp::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = axum::Router::new()
        .route("/mcp", axum::routing::any(client_transport_probe))
        .with_state(probe);
    let task = tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    Ok((format!("http://{address}/mcp"), task))
}

async fn client_transport_probe(
    axum::extract::State(probe): axum::extract::State<Arc<ClientTransportProbe>>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let method = request.method().clone();
    if let Some(bearer) = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        probe
            .request_bearers
            .lock()
            .await
            .push((method.clone(), bearer.to_owned()));
    }
    match *request.method() {
        axum::http::Method::DELETE => {
            probe.deletes.fetch_add(1, Ordering::SeqCst);
            let session = request
                .headers()
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let version = request
                .headers()
                .get("mcp-protocol-version")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            probe.delete_headers.lock().await.push((session, version));
            if probe.delete_status == StatusCode::UNAUTHORIZED {
                return (
                    probe.delete_status,
                    [("www-authenticate", "Bearer error=\"invalid_token\"")],
                )
                    .into_response();
            }
            probe.delete_status.into_response()
        }
        axum::http::Method::GET => {
            probe.get_attempts.lock().await.push(Instant::now());
            probe.get_started.notify_waiters();
            let retry = probe.retry_ms.unwrap_or(1_000);
            let close = probe.retry_ms.is_some();
            let open_event = probe.open_event;
            let fail_body = probe.fail_first_get_body.swap(false, Ordering::SeqCst);
            let fail_decode = probe.fail_first_get_decode.swap(false, Ordering::SeqCst);
            let stream = futures_util::stream::unfold(0_u8, move |state| async move {
                if state == 0 {
                    Some((Ok::<_, std::io::Error>(format!("retry: {retry}\n\n")), 1))
                } else if state == 1 && fail_body {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((Err(std::io::Error::other("injected SSE body failure")), 2))
                } else if state == 1 && fail_decode {
                    Some((Ok::<_, std::io::Error>("data: not-json\n\n".to_owned()), 2))
                } else if state == 1 && open_event {
                    Some((
                        Ok::<_, std::io::Error>(
                            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"data\":\"open-body-event\"}}\n\n"
                                .to_owned(),
                        ),
                        2,
                    ))
                } else if close {
                    None
                } else {
                    std::future::pending().await
                }
            });
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        axum::http::Method::POST => {
            let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let message: Value = serde_json::from_slice(&body).unwrap();
            if message["method"] == "initialize" {
                let mut result = json!({
                    "capabilities": {},
                    "serverInfo": { "name": "probe", "version": "1" },
                });
                if !probe.omit_initialize_version {
                    result["protocolVersion"] = Value::String(MCP_PROTOCOL_VERSION.to_owned());
                }
                return (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("mcp-session-id", "probe-session"),
                        ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
                    ],
                    axum::Json(json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": result,
                    })),
                )
                    .into_response();
            }
            if message["method"] == "notifications/initialized" {
                probe.initialized_started.notify_one();
                return match probe.initialized_failure {
                    InitializedFailure::None => StatusCode::ACCEPTED.into_response(),
                    InitializedFailure::Unauthorized => (
                        StatusCode::UNAUTHORIZED,
                        [("www-authenticate", "Bearer error=\"invalid_token\"")],
                    )
                        .into_response(),
                    InitializedFailure::ServerError => {
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                    InitializedFailure::HeaderTimeout => {
                        tokio::time::sleep(Duration::from_millis(5_250)).await;
                        StatusCode::ACCEPTED.into_response()
                    }
                    InitializedFailure::PendingBody => {
                        let stream =
                            futures_util::stream::pending::<Result<String, std::io::Error>>();
                        (StatusCode::ACCEPTED, axum::body::Body::from_stream(stream))
                            .into_response()
                    }
                    InitializedFailure::OversizedBody => {
                        (StatusCode::ACCEPTED, "x".repeat(1024 * 1024 + 1)).into_response()
                    }
                    InitializedFailure::BodyError => {
                        let stream = futures_util::stream::unfold(0_u8, |state| async move {
                            if state == 0 {
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                Some((
                                    Err::<String, _>(std::io::Error::other(
                                        "injected initialized body failure",
                                    )),
                                    1,
                                ))
                            } else {
                                None
                            }
                        });
                        (StatusCode::ACCEPTED, axum::body::Body::from_stream(stream))
                            .into_response()
                    }
                };
            }
            StatusCode::ACCEPTED.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

#[tokio::test]
async fn dynamic_bearer_provider_supplies_each_post_get_reconnect_and_delete() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        retry_ms: Some(20),
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = calls.clone();
    let provider: Arc<dyn McpBearerProvider> = Arc::new(move || {
        let sequence = provider_calls.fetch_add(1, Ordering::SeqCst);
        async move { Ok(format!("rotating-{sequence}")) }
    });
    let client = McpClient::connect_with_bearer_provider(
        "dynamic-bearer-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: Some("static-token-must-not-be-used".to_owned()),
        },
        provider,
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while probe.get_attempts.lock().await.len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("GET reconnect uses the provider");
    client.shutdown().await?;

    let requests = probe.request_bearers.lock().await;
    assert!(
        requests
            .iter()
            .filter(|(method, _)| *method == axum::http::Method::POST)
            .count()
            >= 2
    );
    assert!(
        requests
            .iter()
            .filter(|(method, _)| *method == axum::http::Method::GET)
            .count()
            >= 2
    );
    assert!(
        requests
            .iter()
            .any(|(method, _)| *method == axum::http::Method::DELETE)
    );
    assert_eq!(requests.len(), calls.load(Ordering::SeqCst));
    for (sequence, (_, bearer)) in requests.iter().enumerate() {
        assert_eq!(bearer, &format!("Bearer rotating-{sequence}"));
    }
    drop(requests);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn dynamic_bearer_resolution_honors_request_cancellation() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe::new(StatusCode::ACCEPTED));
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_started = Arc::new(Notify::new());
    let provider: Arc<dyn McpBearerProvider> = Arc::new({
        let calls = calls.clone();
        let provider_started = provider_started.clone();
        move || {
            let sequence = calls.fetch_add(1, Ordering::SeqCst);
            let provider_started = provider_started.clone();
            async move {
                if sequence == 3 {
                    provider_started.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(format!("dynamic-{sequence}"))
            }
        }
    });
    let client = Arc::new(
        McpClient::connect_with_bearer_provider(
            "cancelled-bearer-probe",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            provider,
            Arc::new(RecordingClient {
                tx: mpsc::unbounded_channel().0,
            }),
            McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
            CancellationToken::new(),
        )
        .await?,
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while probe.get_attempts.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("GET starts before the request probe");

    let cancellation = CancellationToken::new();
    let request = tokio::spawn({
        let client = client.clone();
        let cancellation = cancellation.clone();
        async move { client.list_tools(cancellation).await }
    });
    provider_started.notified().await;
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("request cancellation interrupts bearer resolution")
        .expect("request task")
        .expect_err("cancelled request fails");
    assert!(matches!(error, mcp::Error::Protocol(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        probe
            .request_bearers
            .lock()
            .await
            .iter()
            .filter(|(method, _)| *method == axum::http::Method::POST)
            .count(),
        2
    );

    client.shutdown().await?;
    task.abort();
    Ok(())
}

struct BearerFailureClient {
    failures: mpsc::UnboundedSender<McpBearerProviderError>,
}

impl McpClientHandler for BearerFailureClient {
    fn bearer_provider_failed(&self, error: McpBearerProviderError) {
        self.failures
            .send(error)
            .expect("bearer failure receiver remains open");
    }
}

#[tokio::test]
async fn bearer_provider_failures_are_structured_for_post_get_and_delete() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe::new(StatusCode::ACCEPTED));
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let post_error = McpClient::connect_with_bearer_provider(
        "provider-post-failure",
        McpTransportConfig::StreamableHttp {
            endpoint: endpoint.clone(),
            bearer_token: Some("secret-static-token".to_owned()),
        },
        Arc::new(|| async { Err(McpBearerProviderError::Authorization) }),
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await
    .expect_err("POST token resolution fails");
    assert!(matches!(
        post_error,
        mcp::Error::BearerProvider(McpBearerProviderError::Authorization)
    ));
    let rendered = format!("{post_error:?} {post_error}");
    assert!(!rendered.contains("secret-static-token"));

    let calls = Arc::new(AtomicUsize::new(0));
    let get_provider_started = Arc::new(Notify::new());
    let release_get_provider = Arc::new(Notify::new());
    let provider: Arc<dyn McpBearerProvider> = Arc::new({
        let calls = calls.clone();
        let get_provider_started = get_provider_started.clone();
        let release_get_provider = release_get_provider.clone();
        move || {
            let sequence = calls.fetch_add(1, Ordering::SeqCst);
            let get_provider_started = get_provider_started.clone();
            let release_get_provider = release_get_provider.clone();
            async move {
                if sequence == 2 {
                    get_provider_started.notify_one();
                    release_get_provider.notified().await;
                    return Err(McpBearerProviderError::Unavailable);
                }
                if sequence == 4 {
                    return Err(McpBearerProviderError::Unavailable);
                }
                Ok(format!("initial-{sequence}"))
            }
        }
    });
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let client = Arc::new(
        McpClient::connect_with_bearer_provider(
            "provider-background-failure",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            provider,
            Arc::new(BearerFailureClient {
                failures: failure_tx,
            }),
            McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
            CancellationToken::new(),
        )
        .await?,
    );
    get_provider_started.notified().await;
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while calls.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("pending POST obtains its bearer");
    release_get_provider.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
            .await
            .expect("GET provider failure callback")
            .expect("GET provider failure value"),
        McpBearerProviderError::Unavailable
    );
    assert!(matches!(
        pending.await.expect("pending request task"),
        Err(mcp::Error::BearerProvider(
            McpBearerProviderError::Unavailable
        ))
    ));
    assert!(matches!(
        client.shutdown().await,
        Err(mcp::Error::BearerProvider(
            McpBearerProviderError::Unavailable
        ))
    ));
    assert!(matches!(
        client.shutdown().await,
        Err(mcp::Error::BearerProvider(
            McpBearerProviderError::Unavailable
        ))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(probe.deletes.load(Ordering::SeqCst), 0);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn client_shutdown_joins_blocked_get_and_sends_exactly_one_delete() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe::new(StatusCode::ACCEPTED));
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let client = McpClient::connect(
        "shutdown-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(1), probe.get_started.notified())
        .await
        .expect("GET reader starts");
    let (first, second) = tokio::join!(client.shutdown(), client.shutdown());
    first?;
    second?;
    assert_eq!(probe.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe.delete_headers.lock().await.as_slice(),
        &[("probe-session".to_owned(), MCP_PROTOCOL_VERSION.to_owned())]
    );
    task.abort();
    Ok(())
}

#[tokio::test]
async fn client_shutdown_cleans_locally_and_reports_delete_authorization_failure() -> mcp::Result<()>
{
    let probe = Arc::new(ClientTransportProbe::new(StatusCode::UNAUTHORIZED));
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let client = McpClient::connect(
        "shutdown-auth-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: Some("expired".to_owned()),
        },
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    assert!(matches!(
        client.shutdown().await,
        Err(mcp::Error::Authorization(_))
    ));
    assert!(matches!(
        client.shutdown().await,
        Err(mcp::Error::Authorization(_))
    ));
    assert_eq!(probe.deletes.load(Ordering::SeqCst), 1);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn client_shutdown_cleans_locally_and_reports_delete_network_failure() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe::new(StatusCode::ACCEPTED));
    let (endpoint, task) = start_client_transport_probe(probe).await?;
    let client = McpClient::connect(
        "shutdown-network-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    task.abort();
    drop(task.await);
    let first = client
        .shutdown()
        .await
        .expect_err("unreachable DELETE endpoint fails shutdown");
    let second = client
        .shutdown()
        .await
        .expect_err("idempotent shutdown preserves DELETE failure");
    assert!(matches!(first, mcp::Error::Protocol(_)));
    assert!(matches!(second, mcp::Error::Protocol(_)));
    Ok(())
}

#[tokio::test]
async fn initialize_result_failure_attempts_session_delete() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        omit_initialize_version: true,
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let result = McpClient::connect(
        "invalid-initialize-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(result, Err(mcp::Error::Protocol(message)) if message.contains("protocolVersion"))
    );
    assert_eq!(probe.deletes.load(Ordering::SeqCst), 1);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn issued_session_failures_each_send_exactly_one_cleanup_delete() -> mcp::Result<()> {
    for failure in [
        InitializedFailure::Unauthorized,
        InitializedFailure::ServerError,
        InitializedFailure::HeaderTimeout,
        InitializedFailure::PendingBody,
        InitializedFailure::OversizedBody,
        InitializedFailure::BodyError,
    ] {
        let probe = Arc::new(ClientTransportProbe {
            initialized_failure: failure,
            ..ClientTransportProbe::new(StatusCode::ACCEPTED)
        });
        let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
        let result = McpClient::connect(
            "issued-session-failure-probe",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            Arc::new(RecordingClient {
                tx: mpsc::unbounded_channel().0,
            }),
            McpClientOptions::new("probe-client", "1", Duration::from_secs(10)),
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(probe.deletes.load(Ordering::SeqCst), 1);
        assert_eq!(probe.delete_headers.lock().await.len(), 1);
        task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn issued_session_cancellation_sends_exactly_one_cleanup_delete() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        initialized_failure: InitializedFailure::PendingBody,
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let cancellation = CancellationToken::new();
    let connecting = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            McpClient::connect(
                "issued-session-cancel-probe",
                McpTransportConfig::StreamableHttp {
                    endpoint,
                    bearer_token: None,
                },
                Arc::new(RecordingClient {
                    tx: mpsc::unbounded_channel().0,
                }),
                McpClientOptions::new("probe-client", "1", Duration::from_secs(10)),
                cancellation,
            )
            .await
        }
    });
    probe.initialized_started.notified().await;
    cancellation.cancel();
    assert!(connecting.await.unwrap().is_err());
    assert_eq!(probe.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(probe.delete_headers.lock().await.len(), 1);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn client_honors_sse_retry_instead_of_polling_at_ten_hertz() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        retry_ms: Some(500),
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let client = McpClient::connect(
        "retry-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RecordingClient {
            tx: mpsc::unbounded_channel().0,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(probe.get_attempts.lock().await.len(), 1);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if probe.get_attempts.lock().await.len() >= 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retry delay eventually permits reconnect");
    client.shutdown().await?;
    task.abort();
    Ok(())
}

#[tokio::test]
async fn client_delivers_an_event_while_the_sse_body_remains_open() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        open_event: true,
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = McpClient::connect(
        "incremental-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(RecordingClient { tx }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("event arrives before EOF")
        .expect("event value");
    assert_eq!(event["data"], "open-body-event");
    client.shutdown().await?;
    task.abort();
    Ok(())
}

struct FailureRecordingClient {
    events: mpsc::UnboundedSender<Value>,
    failures: mpsc::UnboundedSender<McpTransportError>,
}

impl McpClientHandler for FailureRecordingClient {
    fn log_message(&self, params: Value) {
        drop(self.events.send(params));
    }

    fn transport_failed(&self, error: McpTransportError) {
        drop(self.failures.send(error));
    }
}

#[tokio::test]
async fn failed_get_body_is_reported_and_polling_recovers() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        retry_ms: Some(20),
        open_event: true,
        fail_first_get_body: AtomicBool::new(true),
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe.clone()).await?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let client = McpClient::connect(
        "body-failure-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(FailureRecordingClient {
            events: event_tx,
            failures: failure_tx,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    let failure = tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
        .await
        .expect("body failure callback")
        .expect("body failure value");
    assert_eq!(failure.kind, McpTransportErrorKind::SseBody);
    assert!(failure.retrying);
    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("polling reconnects after body failure")
        .expect("recovered event");
    assert_eq!(event["data"], "open-body-event");
    assert!(probe.get_attempts.lock().await.len() >= 2);
    client.shutdown().await?;
    task.abort();
    Ok(())
}

#[tokio::test]
async fn failed_get_decoder_is_reported_and_polling_recovers() -> mcp::Result<()> {
    let probe = Arc::new(ClientTransportProbe {
        retry_ms: Some(20),
        open_event: true,
        fail_first_get_decode: AtomicBool::new(true),
        ..ClientTransportProbe::new(StatusCode::ACCEPTED)
    });
    let (endpoint, task) = start_client_transport_probe(probe).await?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let client = McpClient::connect(
        "decoder-failure-probe",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(FailureRecordingClient {
            events: event_tx,
            failures: failure_tx,
        }),
        McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
        CancellationToken::new(),
    )
    .await?;
    let failure = tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
        .await
        .expect("decoder failure callback")
        .expect("decoder failure value");
    assert_eq!(failure.kind, McpTransportErrorKind::SseDecode);
    assert!(failure.retrying);
    let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("polling reconnects after decoder failure")
        .expect("recovered event");
    assert_eq!(event["data"], "open-body-event");
    client.shutdown().await?;
    task.abort();
    Ok(())
}

#[derive(Clone, Copy)]
enum ReinitializeForm {
    JsonSession,
    SseSession,
    SseSessionPendingBody,
    JsonStateless,
}

struct ReinitializeProbe {
    form: ReinitializeForm,
    initializes: AtomicUsize,
    mutations: AtomicUsize,
    old_gets: AtomicUsize,
    old_get_blocked: AtomicBool,
    release_old_get: Notify,
    deletes: Mutex<Vec<String>>,
    new_get_cursors: Mutex<Vec<Option<String>>>,
    block_reinitialized_notification: bool,
    reinitialized_started: Notify,
}

async fn start_reinitialize_probe(
    probe: Arc<ReinitializeProbe>,
) -> mcp::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = axum::Router::new()
        .route("/mcp", axum::routing::any(reinitialize_probe))
        .with_state(probe);
    let task = tokio::spawn(async move {
        drop(axum::serve(listener, router).await);
    });
    Ok((format!("http://{address}/mcp"), task))
}

async fn reinitialize_probe(
    axum::extract::State(probe): axum::extract::State<Arc<ReinitializeProbe>>,
    request: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let method = request.method().clone();
    let session_id = request
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let last_event_id = request
        .headers()
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if method == axum::http::Method::GET {
        if session_id.as_deref() == Some("session-old") {
            if probe.old_gets.fetch_add(1, Ordering::SeqCst) == 0 {
                return (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    "retry: 10\nid: old-stream:1\n\n",
                )
                    .into_response();
            }
            probe.old_get_blocked.store(true, Ordering::SeqCst);
            probe.release_old_get.notified().await;
            return StatusCode::NOT_FOUND.into_response();
        }
        probe.new_get_cursors.lock().await.push(last_event_id);
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if method == axum::http::Method::DELETE {
        if let Some(session_id) = session_id {
            probe.deletes.lock().await.push(session_id);
        }
        return StatusCode::ACCEPTED.into_response();
    }
    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let message: Value = serde_json::from_slice(&body).unwrap();
    if message["method"] == "initialize" {
        let attempt = probe.initializes.fetch_add(1, Ordering::SeqCst);
        let response = json!({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "reinitialize-probe", "version": "1" },
            },
        });
        if attempt == 0 {
            return (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "session-old"),
                    ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
                ],
                axum::Json(response),
            )
                .into_response();
        }
        return match probe.form {
            ReinitializeForm::JsonSession => (
                StatusCode::OK,
                [
                    ("content-type", "application/json"),
                    ("mcp-session-id", "session-new"),
                    ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
                ],
                axum::Json(response),
            )
                .into_response(),
            ReinitializeForm::SseSession => (
                StatusCode::OK,
                [
                    ("content-type", "text/event-stream"),
                    ("mcp-session-id", "session-new"),
                    ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
                ],
                format!("data: {}\n\n", serde_json::to_string(&response).unwrap()),
            )
                .into_response(),
            ReinitializeForm::SseSessionPendingBody => {
                let first = format!("data: {}\n\n", serde_json::to_string(&response).unwrap());
                let stream = futures_util::stream::unfold(0_u8, move |state| {
                    let first = first.clone();
                    async move {
                        if state == 0 {
                            Some((Ok::<_, Infallible>(first), 1))
                        } else {
                            std::future::pending().await
                        }
                    }
                });
                (
                    StatusCode::OK,
                    [
                        ("content-type", "text/event-stream"),
                        ("mcp-session-id", "session-new"),
                        ("mcp-protocol-version", MCP_PROTOCOL_VERSION),
                    ],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
            ReinitializeForm::JsonStateless => (
                StatusCode::OK,
                [("content-type", "application/json")],
                axum::Json(response),
            )
                .into_response(),
        };
    }
    if message["method"] == "notifications/initialized" {
        if probe.block_reinitialized_notification && session_id.as_deref() == Some("session-new") {
            probe.reinitialized_started.notify_one();
            std::future::pending::<()>().await;
        }
        return StatusCode::ACCEPTED.into_response();
    }
    if message["method"] == "tools/list" {
        if probe.mutations.fetch_add(1, Ordering::SeqCst) == 0 {
            return StatusCode::NOT_FOUND.into_response();
        }
        return axum::Json(json!({
            "jsonrpc": "2.0",
            "id": message["id"],
            "result": { "tools": [] },
        }))
        .into_response();
    }
    StatusCode::ACCEPTED.into_response()
}

#[tokio::test]
async fn reinitialization_accepts_json_sse_and_stateless_without_replaying_mutation()
-> mcp::Result<()> {
    for form in [
        ReinitializeForm::JsonSession,
        ReinitializeForm::SseSession,
        ReinitializeForm::SseSessionPendingBody,
        ReinitializeForm::JsonStateless,
    ] {
        let probe = Arc::new(ReinitializeProbe {
            form,
            initializes: AtomicUsize::new(0),
            mutations: AtomicUsize::new(0),
            old_gets: AtomicUsize::new(0),
            old_get_blocked: AtomicBool::new(false),
            release_old_get: Notify::new(),
            deletes: Mutex::new(Vec::new()),
            new_get_cursors: Mutex::new(Vec::new()),
            block_reinitialized_notification: false,
            reinitialized_started: Notify::new(),
        });
        let (endpoint, task) = start_reinitialize_probe(probe.clone()).await?;
        let client = McpClient::connect(
            "reinitialize-probe",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            Arc::new(RecordingClient {
                tx: mpsc::unbounded_channel().0,
            }),
            McpClientOptions::new("probe-client", "1", Duration::from_secs(2)),
            CancellationToken::new(),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !probe.old_get_blocked.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("old GET blocks after establishing a cursor");
        assert!(client.list_tools(CancellationToken::new()).await.is_err());
        assert!(
            client
                .list_tools(CancellationToken::new())
                .await?
                .is_empty()
        );
        probe.release_old_get.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.shutdown().await?;
        assert_eq!(probe.initializes.load(Ordering::SeqCst), 2);
        assert_eq!(probe.mutations.load(Ordering::SeqCst), 2);
        assert!(
            probe
                .new_get_cursors
                .lock()
                .await
                .iter()
                .all(Option::is_none)
        );
        match form {
            ReinitializeForm::JsonStateless => assert!(probe.deletes.lock().await.is_empty()),
            ReinitializeForm::JsonSession
            | ReinitializeForm::SseSession
            | ReinitializeForm::SseSessionPendingBody => {
                assert_eq!(probe.deletes.lock().await.as_slice(), ["session-new"]);
            }
        }
        task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn canceled_reinitialization_deletes_issued_session_without_sending_mutation()
-> mcp::Result<()> {
    let probe = Arc::new(ReinitializeProbe {
        form: ReinitializeForm::JsonSession,
        initializes: AtomicUsize::new(0),
        mutations: AtomicUsize::new(0),
        old_gets: AtomicUsize::new(0),
        old_get_blocked: AtomicBool::new(false),
        release_old_get: Notify::new(),
        deletes: Mutex::new(Vec::new()),
        new_get_cursors: Mutex::new(Vec::new()),
        block_reinitialized_notification: true,
        reinitialized_started: Notify::new(),
    });
    let (endpoint, task) = start_reinitialize_probe(probe.clone()).await?;
    let client = Arc::new(
        McpClient::connect(
            "reinitialize-cancel-probe",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            Arc::new(RecordingClient {
                tx: mpsc::unbounded_channel().0,
            }),
            McpClientOptions::new("probe-client", "1", Duration::from_secs(10)),
            CancellationToken::new(),
        )
        .await?,
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while !probe.old_get_blocked.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(client.list_tools(CancellationToken::new()).await.is_err());
    let cancellation = CancellationToken::new();
    let operation = tokio::spawn({
        let client = client.clone();
        let cancellation = cancellation.clone();
        async move { client.list_tools(cancellation).await }
    });
    probe.reinitialized_started.notified().await;
    cancellation.cancel();
    assert!(operation.await.unwrap().is_err());
    assert_eq!(probe.mutations.load(Ordering::SeqCst), 1);
    assert_eq!(probe.deletes.lock().await.as_slice(), ["session-new"]);
    probe.release_old_get.notify_waiters();
    client.shutdown().await?;
    assert_eq!(probe.deletes.lock().await.len(), 1);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn reconstructed_routers_share_initialized_lifecycle_and_queued_events() -> mcp::Result<()> {
    let store = Arc::new(InMemoryStreamableHttpSessionStore::default());
    let first = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let session_id = initialized_session(&first.endpoint()).await?;
    let reconstructed = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store),
    )
    .await?;
    let client = reqwest::Client::new();

    let tools = client
        .post(reconstructed.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "shared-lifecycle",
            "method": "tools/list",
        }))
        .send()
        .await?;
    assert_eq!(tools.status(), StatusCode::OK);
    assert!(tools.json::<Value>().await?["result"]["tools"].is_array());

    let queued = client
        .get(reconstructed.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(queued.status(), StatusCode::OK);
    assert!(queued.text().await?.contains("queued-for-get-polling"));

    Ok(())
}

#[tokio::test]
async fn reconstructed_router_rejects_repeated_initialize() -> mcp::Result<()> {
    let store = Arc::new(InMemoryStreamableHttpSessionStore::default());
    let first = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let session_id = initialized_session(&first.endpoint()).await?;
    let reconstructed = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store),
    )
    .await?;

    let repeated = reqwest::Client::new()
        .post(reconstructed.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "repeated-initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "reconstructed-client", "version": "1.0.0" },
            },
        }))
        .send()
        .await?;
    assert_eq!(repeated.status(), StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn delete_is_visible_across_routers_and_cannot_resurrect_session() -> mcp::Result<()> {
    let store = Arc::new(InMemoryStreamableHttpSessionStore::default());
    let first = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let session_id = initialized_session(&first.endpoint()).await?;
    let reconstructed = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let client = reqwest::Client::new();

    let deleted = client
        .delete(reconstructed.endpoint())
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(deleted.status(), StatusCode::ACCEPTED);

    let resurrection = client
        .post(first.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
        .send()
        .await?;
    assert_eq!(resurrection.status(), StatusCode::NOT_FOUND);

    let after_attempt = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store),
    )
    .await?;
    let unknown = client
        .get(after_attempt.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn direct_post_sse_omits_ids_while_queued_get_ids_are_stream_scoped() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();

    let direct = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "direct-sse",
            "method": "tools/call",
            "params": { "name": "direct-sse", "arguments": {} },
        }))
        .send()
        .await?;
    assert_eq!(direct.status(), StatusCode::OK);
    let direct_body = direct.text().await?;
    assert!(
        !direct_body.lines().any(|line| line.starts_with("id:")),
        "direct POST SSE must not be resumable: {direct_body}"
    );

    let queued = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(queued.status(), StatusCode::OK);
    let queued_body = queued.text().await?;
    let event_id = queued_body
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .filter_map(|line| line.parse::<StreamableHttpEventId>().ok())
        .find(|event_id| event_id.sequence > 0)
        .expect("stream-scoped queued GET event id");
    assert!(!event_id.stream_id.is_empty());

    Ok(())
}

#[tokio::test]
async fn streamable_http_delete_terminates_session() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();

    let deleted = client
        .delete(server.endpoint())
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(deleted.status(), StatusCode::ACCEPTED);

    let get_after_delete = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .send()
        .await?;
    assert_eq!(get_after_delete.status(), StatusCode::NOT_FOUND);

    let post_after_delete = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "post-after-delete",
            "method": "ping",
        }))
        .send()
        .await?;
    assert_eq!(post_after_delete.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn streamable_http_origin_policy_rejects_invalid_origin() -> mcp::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address: SocketAddr = listener.local_addr()?;
    let router = mcp::server::streamable_http_router_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().try_with_allowed_origins(["https://allowed.test"])?,
    );
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::debug!(%error, "mcp spec server stopped");
        }
    });
    let endpoint = format!("http://{address}/mcp");

    let response = reqwest::Client::new()
        .post(endpoint)
        .header("origin", "https://blocked.test")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "origin-rejected",
            "method": "ping",
        }))
        .send()
        .await?;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    task.abort();
    Ok(())
}

#[tokio::test]
async fn streamable_http_origin_is_default_deny_canonical_and_validated_first() -> mcp::Result<()> {
    assert!(
        StreamableHttpOptions::default()
            .try_with_allowed_origins(["https://allowed.test/path"])
            .is_err()
    );
    let default_server = SpecServer::start(Arc::new(TransportServer)).await?;
    let client = reqwest::Client::new();
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": "origin-matrix",
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "origin-client", "version": "1" },
        },
    });
    assert_eq!(
        client
            .post(default_server.endpoint())
            .header("accept", "application/json, text/event-stream")
            .json(&initialize)
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    for origin in [
        "https://allowed.test",
        "null",
        "not-an-origin",
        "https://allowed.test/path",
        "https://user@allowed.test",
    ] {
        let response = client
            .post(default_server.endpoint())
            .header("origin", origin)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "origin {origin}");
    }

    let allowed = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().try_with_allowed_origins(["https://ALLOWED.test:443"])?,
    )
    .await?;
    assert_eq!(
        client
            .post(allowed.endpoint())
            .header("origin", "https://allowed.test")
            .header("accept", "application/json, text/event-stream")
            .json(&initialize)
            .send()
            .await?
            .status(),
        StatusCode::OK
    );
    for origin in [
        "http://allowed.test",
        "https://allowed.test:444",
        "https://other.test",
    ] {
        assert_eq!(
            client
                .post(allowed.endpoint())
                .header("origin", origin)
                .header("accept", "application/json, text/event-stream")
                .json(&initialize)
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut duplicate_headers = reqwest::header::HeaderMap::new();
    duplicate_headers.append("origin", "https://allowed.test".parse().unwrap());
    duplicate_headers.append("origin", "https://allowed.test".parse().unwrap());
    assert_eq!(
        client
            .post(allowed.endpoint())
            .headers(duplicate_headers)
            .header("accept", "application/json, text/event-stream")
            .json(&initialize)
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn initialize_validation_creates_no_session_and_persisted_version_is_enforced()
-> mcp::Result<()> {
    let store = Arc::new(InMemoryStreamableHttpSessionStore::default());
    let server = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let client = reqwest::Client::new();
    for params in [
        json!({ "capabilities": {}, "clientInfo": { "name": "x", "version": "1" } }),
        json!({ "protocolVersion": MCP_PROTOCOL_VERSION, "clientInfo": { "name": "x", "version": "1" } }),
        json!({ "protocolVersion": MCP_PROTOCOL_VERSION, "capabilities": {} }),
    ] {
        let response = client
            .post(server.endpoint())
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": "invalid-initialize",
                "method": "initialize",
                "params": params,
            }))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("mcp-session-id").is_none());
        assert_eq!(response.json::<Value>().await?["error"]["code"], -32602);
    }
    let session_id = initialized_session(&server.endpoint()).await?;
    let absent = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&json!({ "jsonrpc": "2.0", "id": "absent-version", "method": "ping" }))
        .send()
        .await?;
    assert_eq!(absent.status(), StatusCode::OK);
    let mismatch = client
        .post(server.endpoint())
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", "2024-11-05")
        .json(&json!({ "jsonrpc": "2.0", "id": "bad-version", "method": "ping" }))
        .send()
        .await?;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    assert!(matches!(
        store
            .inspect_session(StreamableHttpSessionRequest {
                key: StreamableHttpSessionKey {
                    session_id,
                    owner: StreamableHttpSessionOwner::Anonymous,
                },
                presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
                now: SystemTime::now(),
            })
            .await
            .unwrap(),
        StreamableHttpInspectSession::Session(session)
            if session.protocol_version == MCP_PROTOCOL_VERSION
    ));
    Ok(())
}

#[tokio::test]
async fn unknown_session_is_rejected_before_malformed_or_oversized_body() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let client = reqwest::Client::new();
    for body in ["{".to_owned(), "x".repeat(1024 * 1024 + 1)] {
        let response = client
            .post(server.endpoint())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-session-id", "unknown-session")
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .body(body)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    Ok(())
}

#[tokio::test]
async fn idle_get_stays_open_after_retry_and_priming_frames() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();
    let queued = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .send()
        .await?;
    let body = queued.text().await?;
    let last_id = body
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .next_back()
        .expect("queued event id");
    let response = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("last-event-id", last_id)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(1), body.next())
        .await
        .expect("retry frame arrives")
        .expect("retry chunk")?;
    assert!(String::from_utf8_lossy(&first).contains("retry: 1000"));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), body.next())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn disconnected_delivery_replays_same_ids_after_lease_expiry() -> mcp::Result<()> {
    let mut config = StreamableHttpSessionStoreConfig::default();
    config.delivery_lease = Duration::from_millis(100);
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(config).unwrap());
    let server = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store),
    )
    .await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();
    let first = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .send()
        .await?;
    let first_body = first.text().await?;
    let first_id = first_body
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .filter_map(|id| id.parse::<StreamableHttpEventId>().ok())
        .find(|id| id.sequence > 0)
        .expect("first delivery id");
    tokio::time::sleep(Duration::from_millis(125)).await;
    let replay = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .send()
        .await?;
    let replay_body = replay.text().await?;
    assert!(replay_body.contains(&format!("id: {first_id}")));
    Ok(())
}

#[tokio::test]
async fn concurrent_gets_do_not_deliver_the_same_message() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(TransportServer)).await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();
    let request = || {
        client
            .get(server.endpoint())
            .header("accept", "text/event-stream")
            .header("mcp-session-id", &session_id)
            .send()
    };
    let (first, second) = tokio::join!(request(), request());
    let first = first?;
    let second = second?;
    let statuses = [first.status(), second.status()];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::CONFLICT));
    let delivered = if first.status() == StatusCode::OK {
        first.text().await?
    } else {
        second.text().await?
    };
    assert_eq!(delivered.matches("queued-for-get-polling").count(), 1);
    Ok(())
}

#[tokio::test]
async fn partial_delivery_ack_reconnects_with_only_the_suffix() -> mcp::Result<()> {
    let mut config = StreamableHttpSessionStoreConfig::default();
    config.delivery_batch_size = 2;
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(config).unwrap());
    let server = SpecServer::start_with_options(
        Arc::new(TransportServer),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let client = reqwest::Client::new();
    let initial = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .send()
        .await?
        .text()
        .await?;
    let initial_id = positive_event_ids(&initial)
        .pop()
        .expect("initial event id");
    let session = StreamableHttpSessionRequest {
        key: StreamableHttpSessionKey {
            session_id: session_id.clone(),
            owner: StreamableHttpSessionOwner::Anonymous,
        },
        presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
        now: SystemTime::now(),
    };
    let StreamableHttpAppendEvents::Events(appended) = store
        .append_events(StreamableHttpAppendEventsRequest {
            session,
            messages: vec![json!("suffix-1"), json!("suffix-2"), json!("suffix-3")],
        })
        .await
        .unwrap()
    else {
        panic!("append succeeds");
    };
    let first = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("last-event-id", initial_id.to_string())
        .send()
        .await?
        .text()
        .await?;
    assert!(first.contains("suffix-1"));
    assert!(first.contains("suffix-2"));
    assert!(!first.contains("suffix-3"));
    let acknowledged = positive_event_ids(&first).pop().expect("partial event id");
    let suffix = client
        .get(server.endpoint())
        .header("accept", "text/event-stream")
        .header("mcp-session-id", session_id)
        .header("last-event-id", acknowledged.to_string())
        .send()
        .await?
        .text()
        .await?;
    assert!(suffix.contains("suffix-3"));
    assert!(!suffix.contains("suffix-1"));
    assert_eq!(positive_event_ids(&suffix), vec![appended[2].id.clone()]);
    Ok(())
}

fn positive_event_ids(body: &str) -> Vec<StreamableHttpEventId> {
    body.lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .filter_map(|id| id.parse().ok())
        .filter(|id: &StreamableHttpEventId| id.sequence > 0)
        .collect()
}

struct InitializationNotificationServer {
    delay: Duration,
    notification_count: usize,
    started: Option<Arc<AtomicBool>>,
    retain_context: bool,
}

impl ServerHandler for InitializationNotificationServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("initialization-regression-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
    }

    fn handle_notification(
        &self,
        notification: mcp::protocol::JsonRpcNotification,
        context: ServerContext,
    ) -> BoxFuture<()> {
        let delay = self.delay;
        let notification_count = self.notification_count;
        let started = self.started.clone();
        let retain_context = self.retain_context;
        Box::pin(async move {
            if notification.method != "notifications/initialized" {
                return;
            }
            if let Some(started) = started {
                started.store(true, Ordering::SeqCst);
            }
            if retain_context {
                let retained = context.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    drop(retained);
                });
            }
            tokio::time::sleep(delay).await;
            for sequence in 0..notification_count {
                context
                    .notify(
                        "notifications/message",
                        Some(json!({ "sequence": sequence })),
                    )
                    .await
                    .expect("queue initialization notification");
            }
        })
    }
}

#[tokio::test]
async fn initialization_drains_more_than_channel_capacity_into_one_completion() -> mcp::Result<()> {
    let store = Arc::new(InMemoryStreamableHttpSessionStore::default());
    let server = SpecServer::start_with_options(
        Arc::new(InitializationNotificationServer {
            delay: Duration::ZERO,
            notification_count: 17,
            started: None,
            retain_context: true,
        }),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let (session_id, initialized) = initialize_with_response(server.endpoint()).await?;
    let initialized = tokio::time::timeout(Duration::from_secs(2), initialized)
        .await
        .expect("initialization must not deadlock")?;
    assert_eq!(initialized.status(), StatusCode::ACCEPTED);
    assert!(matches!(
        store
            .inspect_session(StreamableHttpSessionRequest {
                key: StreamableHttpSessionKey {
                    session_id,
                    owner: StreamableHttpSessionOwner::Anonymous,
                },
                presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
                now: SystemTime::now(),
            })
            .await
            .unwrap(),
        StreamableHttpInspectSession::Session(session)
            if session.lifecycle == StreamableHttpSessionLifecycle::Initialized
                && session.pending_event_count == 17
    ));
    Ok(())
}

#[tokio::test]
async fn delayed_initialization_cannot_bypass_session_expiry() -> mcp::Result<()> {
    let mut config = StreamableHttpSessionStoreConfig::default();
    config.session_ttl = Duration::from_millis(200);
    config.initialization_claim_lease = Duration::from_secs(1);
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(config).unwrap());
    let started = Arc::new(AtomicBool::new(false));
    let server = SpecServer::start_with_options(
        Arc::new(InitializationNotificationServer {
            delay: Duration::from_millis(300),
            notification_count: 0,
            started: Some(started.clone()),
            retain_context: false,
        }),
        StreamableHttpOptions::default().with_session_store(store),
    )
    .await?;
    let (_, initialized) = initialize_with_response(server.endpoint()).await?;
    assert_eq!(initialized.await?.status(), StatusCode::NOT_FOUND);
    assert!(started.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn delayed_initialization_cannot_bypass_claim_expiry() -> mcp::Result<()> {
    let mut config = StreamableHttpSessionStoreConfig::default();
    config.session_ttl = Duration::from_secs(2);
    config.initialization_claim_lease = Duration::from_millis(50);
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(config).unwrap());
    let started = Arc::new(AtomicBool::new(false));
    let server = SpecServer::start_with_options(
        Arc::new(InitializationNotificationServer {
            delay: Duration::from_millis(100),
            notification_count: 0,
            started: Some(started.clone()),
            retain_context: false,
        }),
        StreamableHttpOptions::default().with_session_store(store),
    )
    .await?;
    let (_, initialized) = initialize_with_response(server.endpoint()).await?;
    assert_eq!(initialized.await?.status(), StatusCode::BAD_REQUEST);
    assert!(started.load(Ordering::SeqCst));
    Ok(())
}

#[tokio::test]
async fn initialization_notification_overflow_abandons_without_partial_completion()
-> mcp::Result<()> {
    let mut config = StreamableHttpSessionStoreConfig::default();
    config.max_events_per_session = 16;
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(config).unwrap());
    let server = SpecServer::start_with_options(
        Arc::new(InitializationNotificationServer {
            delay: Duration::ZERO,
            notification_count: 17,
            started: None,
            retain_context: false,
        }),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let (session_id, initialized) = initialize_with_response(server.endpoint()).await?;
    assert_eq!(initialized.await?.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(matches!(
        store
            .inspect_session(StreamableHttpSessionRequest {
                key: StreamableHttpSessionKey {
                    session_id,
                    owner: StreamableHttpSessionOwner::Anonymous,
                },
                presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
                now: SystemTime::now(),
            })
            .await
            .unwrap(),
        StreamableHttpInspectSession::Session(session)
            if session.lifecycle == StreamableHttpSessionLifecycle::InitializeResponded
                && session.pending_event_count == 0
    ));
    Ok(())
}

struct DelayedQueuedNotificationServer {
    started: Arc<Semaphore>,
    resume: Arc<Semaphore>,
}

impl ServerHandler for DelayedQueuedNotificationServer {
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("delayed-notification-server", "1.0.0")
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities::new()
    }

    fn handle_notification(
        &self,
        notification: mcp::protocol::JsonRpcNotification,
        context: ServerContext,
    ) -> BoxFuture<()> {
        let started = self.started.clone();
        let resume = self.resume.clone();
        Box::pin(async move {
            if notification.method == "notifications/delayed" {
                started.add_permits(1);
                resume
                    .acquire()
                    .await
                    .expect("delayed handler barrier remains open")
                    .forget();
                context
                    .notify("notifications/message", Some(json!({ "delayed": true })))
                    .await
                    .expect("queue delayed notification");
            }
        })
    }
}

#[tokio::test]
async fn delayed_notification_append_cannot_bypass_session_expiry() -> mcp::Result<()> {
    let mut config = StreamableHttpSessionStoreConfig::default();
    config.session_ttl = Duration::from_secs(2);
    let store = Arc::new(InMemoryStreamableHttpSessionStore::new(config).unwrap());
    let started = Arc::new(Semaphore::new(0));
    let resume = Arc::new(Semaphore::new(0));
    let server = SpecServer::start_with_options(
        Arc::new(DelayedQueuedNotificationServer {
            started: started.clone(),
            resume: resume.clone(),
        }),
        StreamableHttpOptions::default().with_session_store(store.clone()),
    )
    .await?;
    let session_id = initialized_session(&server.endpoint()).await?;
    let session_key = StreamableHttpSessionKey {
        session_id: session_id.clone(),
        owner: StreamableHttpSessionOwner::Anonymous,
    };
    let StreamableHttpInspectSession::Session(session) = store
        .inspect_session(StreamableHttpSessionRequest {
            key: session_key.clone(),
            presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
            now: SystemTime::now(),
        })
        .await
        .unwrap()
    else {
        panic!("initialized session must be active");
    };
    let endpoint = server.endpoint();
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", session_id)
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/delayed",
            }))
            .send()
            .await
    });
    started
        .acquire()
        .await
        .expect("delayed handler start barrier remains open")
        .forget();
    let active_now = SystemTime::now();
    assert!(active_now < session.expires_at);
    assert!(matches!(
        store
            .inspect_session(StreamableHttpSessionRequest {
                key: session_key.clone(),
                presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
                now: active_now,
            })
            .await
            .unwrap(),
        StreamableHttpInspectSession::Session(_)
    ));
    tokio::time::sleep(
        session
            .expires_at
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            + Duration::from_millis(1),
    )
    .await;
    let expired_now = SystemTime::now();
    assert!(expired_now >= session.expires_at);
    assert_eq!(
        store
            .inspect_session(StreamableHttpSessionRequest {
                key: session_key,
                presented_protocol_version: Some(MCP_PROTOCOL_VERSION.to_owned()),
                now: expired_now,
            })
            .await
            .unwrap(),
        StreamableHttpInspectSession::NotFound
    );
    resume.add_permits(1);
    let response = request.await.expect("delayed request task")?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

async fn initialize_with_response(
    endpoint: String,
) -> mcp::Result<(
    String,
    impl Future<Output = Result<reqwest::Response, reqwest::Error>>,
)> {
    let client = reqwest::Client::new();
    let initialize = client
        .post(&endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "regression-initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "regression-client", "version": "1.0.0" },
            },
        }))
        .send()
        .await?;
    let session_id = initialize
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("initialize session id")
        .to_owned();
    drop(initialize.bytes().await?);
    let initialized = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
        .send();
    Ok((session_id, initialized))
}
