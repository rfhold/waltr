use super::super::support::{SpecServer, modern_params, post_json, sse_messages};
use mcp::{
    McpEventDefinition, McpEventError, McpEventList, McpEventMessage, McpEventStreamRequest,
    client::{McpClient, McpClientHandler, McpClientOptions, McpTransportConfig},
    server::{
        BoxFuture, ServerCapabilities, ServerContext, ServerError, ServerHandler, ServerInfo,
        ServerResult,
    },
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Events {
    seen: Mutex<Vec<McpEventStreamRequest>>,
    live: Arc<AtomicUsize>,
    disabled: bool,
    cancellations: Mutex<Vec<Value>>,
}
struct Live(Arc<AtomicUsize>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ServerHandler for Events {
    fn handle_notification(
        &self,
        notification: mcp::protocol::JsonRpcNotification,
        _context: ServerContext,
    ) -> BoxFuture<()> {
        if notification.method == "notifications/cancelled" {
            self.cancellations
                .lock()
                .unwrap()
                .push(notification.params.unwrap());
        }
        Box::pin(async {})
    }
    fn server_info(&self) -> ServerInfo {
        ServerInfo::new("events", "1")
    }
    fn capabilities(&self) -> ServerCapabilities {
        if self.disabled {
            ServerCapabilities::new()
        } else {
            ServerCapabilities::new().events()
        }
    }
    fn list_events(
        &self,
        cursor: Option<String>,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<McpEventList>> {
        assert_eq!(
            context.client_capabilities().unwrap()["extensions"]["io.modelcontextprotocol/events"],
            json!({})
        );
        Box::pin(async move {
            if cursor.as_deref().is_some_and(|v| v != "next") {
                return Err(ServerError::invalid_params("unknown cursor"));
            }
            Ok(McpEventList {
                events: vec![McpEventDefinition {
                    name: if cursor.is_some() {
                        "second"
                    } else {
                        "mail.received"
                    }
                    .into(),
                    description: None,
                    input_schema: Some(serde_json::Map::new()),
                    payload_schema: None,
                    delivery: Some(vec!["push".into()]),
                }],
                next_cursor: if cursor.is_none() {
                    Some("next".into())
                } else {
                    None
                },
            })
        })
    }
    fn stream_events(
        &self,
        request: McpEventStreamRequest,
        context: ServerContext,
    ) -> BoxFuture<ServerResult<()>> {
        self.seen.lock().unwrap().push(request.clone());
        self.live.fetch_add(1, Ordering::SeqCst);
        let live = Live(Arc::clone(&self.live));
        Box::pin(async move {
            let _live = live;
            if request
                .arguments
                .as_ref()
                .is_some_and(|v| v.contains_key("invalid"))
            {
                return Err(ServerError::invalid_params("invalid filter"));
            }
            context
                .event(McpEventMessage::Active {
                    cursor: request.cursor.clone(),
                    truncated: true,
                })
                .await
                .map_err(|e| ServerError::internal(e.to_string()))?;
            if request.name == "wait" {
                std::future::pending::<()>().await;
            }
            if request.name == "malformed" {
                context
                    .notify("notifications/events/event", Some(json!({"eventId":4})))
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            }
            if request.name == "terminated" {
                context
                    .event(McpEventMessage::Terminated {
                        error: McpEventError {
                            code: -32024,
                            message: "Forbidden".into(),
                            data: Some(json!({"reason":"revoked"})),
                        },
                    })
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            }
            let count = if request.name == "flood" { 512 } else { 1 };
            for index in 0..count {
                if request.name != "flood" {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                context
                    .event(McpEventMessage::Event {
                        event_id: format!("{}-{index}", request.name),
                        name: request.name.clone(),
                        timestamp: "2026-11-10T00:00:00Z".into(),
                        data: json!({"uri":"inbox://message/1"}),
                        cursor: Some("c1".into()),
                    })
                    .await
                    .map_err(|e| ServerError::internal(e.to_string()))?;
            }
            context
                .event(McpEventMessage::Heartbeat {
                    cursor: Some("c2".into()),
                })
                .await
                .unwrap();
            context
                .event(McpEventMessage::Error {
                    error: McpEventError {
                        code: -32603,
                        message: "recoverable".into(),
                        data: None,
                    },
                })
                .await
                .unwrap();
            Ok(())
        })
    }
}
struct Handler;
impl McpClientHandler for Handler {}
fn input(name: &str) -> McpEventStreamRequest {
    McpEventStreamRequest {
        name: name.into(),
        arguments: None,
        cursor: Some("c0".into()),
        max_age_ms: Some(123),
    }
}
async fn connect(server: &SpecServer, events: bool) -> mcp::Result<McpClient> {
    let mut options = McpClientOptions::new("events-test", "1", Duration::from_millis(80));
    if events {
        options = options.with_events();
    }
    McpClient::connect(
        "events",
        McpTransportConfig::StreamableHttp {
            endpoint: server.endpoint(),
            bearer_token: None,
        },
        Arc::new(Handler),
        options,
        CancellationToken::new(),
    )
    .await
}
async fn no_live(handler: &Events) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while handler.live.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
fn params(value: Value) -> Value {
    let mut value = modern_params(value);
    value["_meta"]["io.modelcontextprotocol/clientCapabilities"] =
        json!({"extensions":{"io.modelcontextprotocol/events":{}}});
    value
}

#[tokio::test]
async fn opt_in_pagination_and_default_off() -> mcp::Result<()> {
    let server = SpecServer::start(Arc::new(Events::default())).await?;
    let off = connect(&server, false).await?;
    assert!(off.capabilities().supports_events());
    assert!(
        off.list_events(None, CancellationToken::new())
            .await
            .is_err()
    );
    let on = connect(&server, true).await?;
    let first = on.list_events(None, CancellationToken::new()).await?;
    assert_eq!(first.events[0].name, "mail.received");
    let second = on
        .list_events(first.next_cursor.as_deref(), CancellationToken::new())
        .await?;
    assert_eq!(second.events[0].name, "second");
    assert!(second.next_cursor.is_none());
    for invalid in [json!({"cursor":3}), json!({"cursor":"unknown"})] {
        let response = post_json(
            &server.endpoint(),
            json!({"jsonrpc":"2.0","id":1,"method":"events/list","params":params(invalid)}),
        )
        .await?;
        assert_eq!(response.body["error"]["code"], -32602);
    }
    let denied = post_json(
        &server.endpoint(),
        json!({"jsonrpc":"2.0","id":1,"method":"events/list","params":modern_params(json!({}))}),
    )
    .await?;
    assert_eq!(denied.body["error"]["code"], -32602);
    off.shutdown().await?;
    on.shutdown().await?;
    let server = SpecServer::start(Arc::new(Events {
        disabled: true,
        ..Events::default()
    }))
    .await?;
    let client = connect(&server, true).await?;
    assert!(!client.capabilities().supports_events());
    assert!(
        client
            .stream_events(input("mail.received"), CancellationToken::new())
            .await
            .is_err()
    );
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn incremental_http_outlives_unary_timeout_and_routes_concurrent_streams() -> mcp::Result<()>
{
    let handler = Arc::new(Events::default());
    let server = SpecServer::start(handler.clone()).await?;
    let client = connect(&server, true).await?;
    let mut a = client
        .stream_events(input("a"), CancellationToken::new())
        .await?;
    let mut b = client
        .stream_events(input("b"), CancellationToken::new())
        .await?;
    assert_eq!(
        a.recv().await.unwrap()?,
        McpEventMessage::Active {
            cursor: Some("c0".into()),
            truncated: true
        }
    );
    assert!(matches!(
        b.recv().await.unwrap()?,
        McpEventMessage::Active { .. }
    ));
    assert_eq!(
        client
            .list_events(None, CancellationToken::new())
            .await?
            .events[0]
            .name,
        "mail.received"
    );
    for (stream, name) in [(&mut a, "a"), (&mut b, "b")] {
        assert!(
            matches!(stream.recv().await.unwrap()?, McpEventMessage::Event { event_id, .. } if event_id == format!("{name}-0"))
        );
        assert!(
            matches!(stream.recv().await.unwrap()?, McpEventMessage::Heartbeat { cursor: Some(cursor) } if cursor == "c2")
        );
        assert!(matches!(
            stream.recv().await.unwrap()?,
            McpEventMessage::Error { .. }
        ));
        assert!(stream.recv().await.is_none());
        stream.shutdown().await;
    }
    assert!(
        handler
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|v| v.max_age_ms == Some(123) && v.cursor.as_deref() == Some("c0"))
    );
    no_live(&handler).await;
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_termination_malformed_and_slow_consumer_cleanup() -> mcp::Result<()> {
    let handler = Arc::new(Events::default());
    let server = SpecServer::start(handler.clone()).await?;
    let client = connect(&server, true).await?;
    for name in ["wait", "terminated", "malformed", "flood"] {
        let mut stream = client
            .stream_events(input(name), CancellationToken::new())
            .await?;
        assert!(matches!(
            stream.recv().await.unwrap()?,
            McpEventMessage::Active { .. }
        ));
        if name == "terminated" {
            assert!(
                matches!(stream.recv().await.unwrap()?, McpEventMessage::Terminated { error } if error.code == -32024 && error.data == Some(json!({"reason":"revoked"})))
            );
            assert!(stream.recv().await.is_none());
        } else if name == "malformed" {
            assert!(stream.recv().await.unwrap().is_err());
        } else if name == "flood" {
            tokio::time::sleep(Duration::from_millis(100)).await;
            no_live(&handler).await;
            let mut count = 0;
            loop {
                match stream.recv().await {
                    Some(Ok(_)) => count += 1,
                    Some(Err(_)) => break,
                    None => panic!("overflow must fail the stream"),
                }
            }
            assert!(count <= 64, "consumer queue exceeded its bound");
        }
        stream.shutdown().await;
        no_live(&handler).await;
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while handler.cancellations.lock().unwrap().len() < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for cancellation in handler.cancellations.lock().unwrap().iter() {
        assert!(cancellation["requestId"].is_number());
        assert_eq!(
            cancellation["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
    }
    let mut waiting = client
        .stream_events(input("wait"), CancellationToken::new())
        .await?;
    assert!(waiting.recv().await.is_some());
    client.shutdown().await?;
    waiting.shutdown().await;
    no_live(&handler).await;
    Ok(())
}

async fn raw_events(axum::Json(request): axum::Json<Value>) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    use futures_util::StreamExt as _;
    if request["method"] == "server/discover" {
        return axum::Json(json!({"jsonrpc":"2.0", "id":request["id"], "result":{
            "supportedVersions":["2026-07-28"], "capabilities":{"extensions":{"io.modelcontextprotocol/events":{}}}, "ttlMs":0, "cacheScope":"private"
        }})).into_response();
    }
    let id = if request["params"]["name"] == "wrong-id" {
        json!("wrong")
    } else {
        request["id"].clone()
    };
    let active = json!({"jsonrpc":"2.0", "method":"notifications/events/active", "params":{"cursor":null, "_meta":{"io.modelcontextprotocol/subscriptionId":id}}});
    let mut wire = format!("data: {active}\n\n");
    let abrupt = request["params"]["name"] == "abrupt";
    if !abrupt {
        let result = match request["params"]["name"].as_str() {
            Some("complete") => json!({"resultType":"complete"}),
            Some("input_required") => {
                json!({"resultType":"input_required", "inputRequests":{"question":{"method":"elicitation/create"}}})
            }
            Some("unknown") => json!({"resultType":"unknown"}),
            Some("number") => json!({"resultType":3}),
            Some("null") => json!({"resultType":null}),
            _ => json!({}),
        };
        let final_response = json!({"jsonrpc":"2.0", "id":request["id"], "result":result});
        wire.push_str("data: ");
        wire.push_str(&final_response.to_string());
        wire.push_str("\n\n");
    }
    let first = futures_util::stream::iter(vec![Ok::<_, std::convert::Infallible>(
        axum::body::Bytes::from(wire),
    )]);
    let body = if abrupt {
        axum::body::Body::from_stream(first)
    } else {
        axum::body::Body::from_stream(first.chain(futures_util::stream::pending()))
    };
    ([("content-type", "text/event-stream")], body).into_response()
}

#[tokio::test]
async fn empty_final_result_ends_without_http_eof_and_broken_streams_fail_locally()
-> mcp::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/mcp", listener.local_addr()?);
    let app = axum::Router::new().route("/mcp", axum::routing::post(raw_events));
    let task = tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });
    let client = McpClient::connect(
        "raw-events",
        McpTransportConfig::StreamableHttp {
            endpoint,
            bearer_token: None,
        },
        Arc::new(Handler),
        McpClientOptions::new("test", "1", Duration::from_secs(2)).with_events(),
        CancellationToken::new(),
    )
    .await?;
    for name in [
        "normal",
        "complete",
        "input_required",
        "unknown",
        "number",
        "null",
        "abrupt",
        "wrong-id",
    ] {
        let mut stream = client
            .stream_events(input(name), CancellationToken::new())
            .await?;
        if name != "wrong-id" {
            assert!(matches!(
                stream.recv().await.unwrap()?,
                McpEventMessage::Active { .. }
            ));
        }
        let final_message = tokio::time::timeout(Duration::from_secs(2), stream.recv())
            .await
            .unwrap();
        if matches!(name, "normal" | "complete") {
            assert!(final_message.is_none());
        } else {
            assert!(final_message.unwrap().is_err());
        }
        stream.shutdown().await;
    }
    client.shutdown().await?;
    task.abort();
    Ok(())
}

async fn authorization_failure_events(
    axum::extract::State(live): axum::extract::State<Arc<AtomicUsize>>,
    axum::Json(request): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if request["method"] == "server/discover" {
        return raw_events(axum::Json(request)).await;
    }
    if request["method"] == "events/list" {
        let status = if request["params"]["cursor"] == "401" {
            axum::http::StatusCode::UNAUTHORIZED
        } else {
            axum::http::StatusCode::FORBIDDEN
        };
        return status.into_response();
    }
    assert_eq!(request["method"], "events/stream");
    let meta = json!({"io.modelcontextprotocol/subscriptionId":request["id"]});
    let active =
        json!({"jsonrpc":"2.0", "method":"notifications/events/active", "params":{"_meta":meta}});
    let event = json!({"jsonrpc":"2.0", "method":"notifications/events/event", "params":{
        "_meta":meta, "eventId":request["params"]["name"], "name":request["params"]["name"], "timestamp":"2026-11-15T00:00:00Z", "data":{"uri":"inbox://message/1"}
    }});
    live.fetch_add(1, Ordering::SeqCst);
    let producer = Live(live);
    let bytes = axum::body::Bytes::from(format!("data: {active}\n\ndata: {event}\n\n"));
    let stream =
        futures_util::stream::unfold((Some(bytes), producer), |(bytes, producer)| async move {
            match bytes {
                Some(bytes) => Some((Ok::<_, std::convert::Infallible>(bytes), (None, producer))),
                None => std::future::pending().await,
            }
        });
    (
        [("content-type", "text/event-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

#[tokio::test]
async fn concurrent_authorization_failure_closes_unpolled_event_bodies_and_preserves_buffers()
-> mcp::Result<()> {
    for status in ["401", "403"] {
        let live = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/mcp", listener.local_addr()?);
        let app = axum::Router::new()
            .route("/mcp", axum::routing::post(authorization_failure_events))
            .with_state(Arc::clone(&live));
        let task = tokio::spawn(async move {
            drop(axum::serve(listener, app).await);
        });
        let client = McpClient::connect(
            "auth-events",
            McpTransportConfig::StreamableHttp {
                endpoint,
                bearer_token: None,
            },
            Arc::new(Handler),
            McpClientOptions::new("test", "1", Duration::from_secs(2)).with_events(),
            CancellationToken::new(),
        )
        .await?;
        let mut a = client
            .stream_events(input("a"), CancellationToken::new())
            .await?;
        let mut b = client
            .stream_events(input("b"), CancellationToken::new())
            .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while live.load(Ordering::SeqCst) != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            client
                .list_events(Some(status), CancellationToken::new())
                .await,
            Err(mcp::Error::Authorization(_))
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while live.load(Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect(
            "unpolled event handles retained their HTTP body producers after authorization failure",
        );
        for (stream, name) in [(&mut a, "a"), (&mut b, "b")] {
            assert!(matches!(
                stream.recv().await.unwrap()?,
                McpEventMessage::Active { .. }
            ));
            assert!(
                matches!(stream.recv().await.unwrap()?, McpEventMessage::Event { event_id, .. } if event_id == name)
            );
            assert!(matches!(
                stream.recv().await,
                Some(Err(mcp::Error::Authorization(_)))
            ));
            assert!(stream.recv().await.is_none());
            stream.shutdown().await;
        }
        client.shutdown().await?;
        task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn stream_optional_fields_validate_before_handler_on_http_and_stdio() -> mcp::Result<()> {
    let handler = Arc::new(Events::default());
    let server = SpecServer::start(handler.clone()).await?;
    let (client, transport) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(transport);
    let task = tokio::spawn(mcp::server::serve_stream(handler.clone(), read, write));
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    for invalid in [
        json!({"name":"x","arguments":null}),
        json!({"name":"x","maxAgeMs":null}),
        json!({"name":"x","arguments":[]}),
        json!({"name":"x","maxAgeMs":-1}),
        json!({"name":"x","cursor":42}),
    ] {
        let request =
            json!({"jsonrpc":"2.0","id":1,"method":"events/stream","params":params(invalid)});
        let result = post_json(&server.endpoint(), request.clone()).await?;
        assert_eq!(result.body["error"]["code"], -32602);
        assert!(handler.seen.lock().unwrap().is_empty());
        write.write_all(format!("{request}\n").as_bytes()).await?;
        let result: Value = serde_json::from_str(
            &tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                .await
                .unwrap()?
                .unwrap(),
        )?;
        assert_eq!(result["id"], 1);
        assert_eq!(result["error"]["code"], -32602);
        assert!(handler.seen.lock().unwrap().is_empty());
    }
    for (valid, arguments, max_age_ms) in [
        (json!({"name":"x"}), None, None),
        (json!({"name":"x","maxAgeMs":0}), None, Some(0)),
        (
            json!({"name":"x","arguments":{}}),
            Some(serde_json::Map::new()),
            None,
        ),
        (json!({"name":"x","cursor":null}), None, None),
    ] {
        let expected = McpEventStreamRequest {
            name: "x".into(),
            arguments,
            cursor: None,
            max_age_ms,
        };
        let request =
            json!({"jsonrpc":"2.0","id":2,"method":"events/stream","params":params(valid)});
        let result = post_json(&server.endpoint(), request.clone()).await?;
        assert_eq!(result.body["result"]["resultType"], "complete");
        assert_eq!(*handler.seen.lock().unwrap(), vec![expected.clone()]);
        handler.seen.lock().unwrap().clear();
        write.write_all(format!("{request}\n").as_bytes()).await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let message: Value = serde_json::from_str(&lines.next_line().await?.unwrap())?;
                if message.get("id").is_some() {
                    assert_eq!(message["id"], 2);
                    assert_eq!(message["result"]["resultType"], "complete");
                    break;
                }
            }
            Ok::<_, mcp::Error>(())
        })
        .await
        .unwrap()?;
        assert_eq!(*handler.seen.lock().unwrap(), vec![expected]);
        handler.seen.lock().unwrap().clear();
    }
    no_live(&handler).await;
    drop(write);
    drop(lines);
    task.await.unwrap()?;
    Ok(())
}

#[tokio::test]
async fn wire_subscription_ids_final_response_and_invalid_arguments_on_stdio_and_http()
-> mcp::Result<()> {
    let handler = Arc::new(Events::default());
    let server = SpecServer::start(handler.clone()).await?;
    let response = reqwest::Client::new().post(server.endpoint()).header("accept","application/json, text/event-stream")
        .header("mcp-protocol-version","2026-07-28").header("mcp-method","events/stream").header("mcp-name","mail.received")
        .json(&json!({"jsonrpc":"2.0","id":"stream-1","method":"events/stream","params":params(json!({"name":"mail.received"}))})).send().await?;
    let messages = sse_messages(&response.text().await?)?;
    assert_eq!(messages.len(), 5);
    for message in &messages[..4] {
        assert_eq!(
            message["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
            "stream-1"
        );
    }
    assert_eq!(messages[4]["id"], "stream-1");
    assert_eq!(messages[4]["result"]["resultType"], "complete");
    for invalid in [
        json!({"name":"x","arguments":[]}),
        json!({"name":"x","maxAgeMs":-1}),
        json!({"name":"x","cursor":42}),
        json!({"name":"x","arguments":{"invalid":true}}),
    ] {
        let result = post_json(
            &server.endpoint(),
            json!({"jsonrpc":"2.0","id":1,"method":"events/stream","params":params(invalid)}),
        )
        .await?;
        assert_eq!(result.body["error"]["code"], -32602);
    }
    let (client, transport) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(transport);
    let task = tokio::spawn(mcp::server::serve_stream(handler.clone(), read, write));
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    let request = json!({"jsonrpc":"2.0","id":7,"method":"events/stream","params":params(json!({"name":"wait"}))});
    write.write_all(format!("{request}\n").as_bytes()).await?;
    let active: Value = serde_json::from_str(&lines.next_line().await?.unwrap())?;
    assert_eq!(
        active["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        7
    );
    let cancel = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":params(json!({"requestId":7}))});
    write.write_all(format!("{cancel}\n").as_bytes()).await?;
    no_live(&handler).await;
    drop(write);
    drop(lines);
    task.await.unwrap()?;
    Ok(())
}
