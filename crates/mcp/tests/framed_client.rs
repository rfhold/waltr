use std::{
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, mpsc as std_mpsc},
    time::Duration,
};

use mcp::{
    Error, McpToolCall, Result,
    client::{McpClient, McpClientHandler, McpClientOptions, McpFramedChannel},
    protocol::{MCP_CLIENT_CAPABILITIES_META_KEY, MCP_PROTOCOL_VERSION},
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const LIMIT: usize = 512;
const WAIT: Duration = Duration::from_secs(3);

type ChannelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
type ShutdownReply = oneshot::Sender<Result<()>>;

struct Channel {
    inbound: Mutex<mpsc::Receiver<Vec<u8>>>,
    outbound: mpsc::Sender<Vec<u8>>,
    shutdown: Mutex<Option<oneshot::Sender<ShutdownReply>>>,
    stall_send: bool,
}

impl McpFramedChannel for Channel {
    fn send_frame(&self, frame: Vec<u8>) -> ChannelFuture<'_, ()> {
        Box::pin(async move {
            let held = self.stall_send
                && serde_json::from_slice::<Value>(&frame)?["method"] == "tools/list";
            self.outbound
                .send(frame)
                .await
                .map_err(|_| Error::protocol("fixture writer closed"))?;
            if held {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn recv_frame(&self) -> ChannelFuture<'_, Option<Vec<u8>>> {
        Box::pin(async { Ok(self.inbound.lock().await.recv().await) })
    }

    fn shutdown(&self) -> ChannelFuture<'_, ()> {
        Box::pin(async {
            assert!(
                self.inbound.try_lock().is_ok(),
                "receive must stop before cleanup"
            );
            if let Some(shutdown) = self.shutdown.lock().await.take() {
                let (tx, rx) = oneshot::channel();
                shutdown.send(tx).expect("shutdown observer");
                return rx.await.expect("explicit fixture shutdown result");
            }
            Ok(())
        })
    }
}

struct Handler {
    logs: mpsc::UnboundedSender<Value>,
    disconnects: mpsc::UnboundedSender<String>,
    log_gate: StdMutex<Option<LogGate>>,
}

struct LogGate {
    entered: oneshot::Sender<()>,
    release: std_mpsc::Receiver<()>,
}

impl McpClientHandler for Handler {
    fn log_message(&self, params: Value) {
        let gate = self.log_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            let _ = gate.release.recv_timeout(WAIT * 2);
        }
        self.logs.send(params).unwrap();
    }

    fn disconnected(&self, error: String) {
        self.disconnects.send(error).unwrap();
    }
}

struct Peer {
    send: mpsc::Sender<Vec<u8>>,
    receive: mpsc::Receiver<Vec<u8>>,
    shutdown: oneshot::Receiver<ShutdownReply>,
    logs: mpsc::UnboundedReceiver<Value>,
    disconnects: mpsc::UnboundedReceiver<String>,
}

impl Peer {
    async fn request(&mut self, method: &str) -> Value {
        let frame = tokio::time::timeout(WAIT, self.receive.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!frame.contains(&b'\n') && !frame.contains(&b'\r'));
        let request: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], method);
        request
    }

    async fn message(&self, message: Value) {
        self.send
            .send(serde_json::to_vec(&message).unwrap())
            .await
            .unwrap();
    }

    async fn reply(&self, request: &Value, result: Value) {
        self.message(json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": result,
        }))
        .await;
    }

    async fn finish(&mut self, client: &McpClient) {
        let shutdown = client.shutdown();
        let acknowledge = async {
            let reply = (&mut self.shutdown).await.unwrap();
            reply.send(Ok(())).unwrap();
        };
        let (result, ()) =
            tokio::time::timeout(WAIT, async { tokio::join!(shutdown, acknowledge) })
                .await
                .unwrap();
        result.unwrap();
    }
}

async fn connected(stall_send: bool) -> (McpClient, Peer) {
    connected_with_log_gate(stall_send, None).await
}

async fn connected_with_log_gate(stall_send: bool, log_gate: Option<LogGate>) -> (McpClient, Peer) {
    let (in_tx, in_rx) = mpsc::channel(4);
    let (out_tx, out_rx) = mpsc::channel(4);
    let (shutdown_tx, shutdown) = oneshot::channel();
    let (logs_tx, logs) = mpsc::unbounded_channel();
    let (disconnects_tx, disconnects) = mpsc::unbounded_channel();
    let mut peer = Peer {
        send: in_tx,
        receive: out_rx,
        shutdown,
        logs,
        disconnects,
    };
    let connect = McpClient::connect_framed(
        Arc::new(Channel {
            inbound: Mutex::new(in_rx),
            outbound: out_tx,
            shutdown: Mutex::new(Some(shutdown_tx)),
            stall_send,
        }),
        NonZeroUsize::new(LIMIT).unwrap(),
        Arc::new(Handler {
            logs: logs_tx,
            disconnects: disconnects_tx,
            log_gate: StdMutex::new(log_gate),
        }),
        McpClientOptions::new("fixture", "1", Duration::from_secs(30))
            .with_apps()
            .with_skills(),
        CancellationToken::new(),
    );
    let discover = async {
        let request = peer.request("server/discover").await;
        let meta = &request["params"]["_meta"];
        let extensions = &meta[MCP_CLIENT_CAPABILITIES_META_KEY]["extensions"];
        assert!(extensions[mcp::apps::UI_EXTENSION].is_object());
        assert_eq!(extensions[mcp::skills::SKILLS_EXTENSION], json!({}));
        assert!(meta.get("traceparent").is_none());
        peer.reply(
            &request,
            json!({
                "resultType": "complete", "supportedVersions": [MCP_PROTOCOL_VERSION],
                "capabilities": { "tools": {} }, "ttlMs": 0, "cacheScope": "private",
                "instructions": "frame fixture"
            }),
        )
        .await;
    };
    let (result, ()) = tokio::join!(connect, discover);
    (result.unwrap(), peer)
}

#[tokio::test]
async fn discovery_request_reply_and_interleaved_notification() {
    let (client, mut peer) = connected(false).await;
    assert_eq!(client.instructions(), Some("frame fixture"));
    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    let request = peer.request("tools/list").await;
    assert!(
        request["params"]["_meta"][MCP_CLIENT_CAPABILITIES_META_KEY]["extensions"]
            [mcp::apps::UI_EXTENSION]
            .is_object()
    );
    peer.message(json!({
        "jsonrpc": "2.0", "method": "notifications/message", "params": {"data": "fixture"},
    }))
    .await;
    assert_eq!(
        tokio::time::timeout(WAIT, peer.logs.recv())
            .await
            .unwrap()
            .unwrap()["data"],
        "fixture"
    );
    assert!(!list.is_finished());
    let mut exact_limit = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": request["id"],
        "result": {"tools": [{"name": "fixture", "inputSchema": {"type": "object"}}]},
    }))
    .unwrap();
    exact_limit.resize(LIMIT, b' ');
    peer.send.send(exact_limit).await.unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, list)
            .await
            .unwrap()
            .unwrap()
            .unwrap()[0]
            .name,
        "fixture"
    );
    peer.finish(&client).await;
}

#[tokio::test]
async fn server_input_request_uses_the_existing_mrtr_continuation() {
    let (client, mut peer) = connected(false).await;
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_tool(
                    McpToolCall {
                        name: "fixture".to_owned(),
                        arguments: json!({}),
                        progress_token: None,
                    },
                    CancellationToken::new(),
                )
                .await
        }
    });
    let initial = peer.request("tools/call").await;
    peer.reply(
        &initial,
        json!({
            "resultType": "input_required",
            "inputRequests": { "roots": { "method": "roots/list" } },
            "requestState": "opaque"
        }),
    )
    .await;
    let continuation = peer.request("tools/call").await;
    assert_ne!(continuation["id"], initial["id"]);
    assert_eq!(continuation["params"]["requestState"], "opaque");
    assert_eq!(
        continuation["params"]["inputResponses"]["roots"],
        json!({"resultType": "complete", "roots": []})
    );
    let result = json!({"resultType": "complete", "content": []});
    peer.reply(&continuation, result.clone()).await;
    assert_eq!(
        tokio::time::timeout(WAIT, call)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .raw,
        result
    );
    peer.finish(&client).await;
}

#[tokio::test]
async fn stale_operation_cannot_send_a_request_state_or_roots_continuation() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    for result in [
        json!({"resultType": "input_required", "requestState": "opaque"}),
        json!({"resultType": "input_required", "inputRequests": {"roots": {"method": "roots/list"}}}),
    ] {
        let (client, mut peer) = connected(false).await;
        let current = Arc::new(AtomicBool::new(true));
        let checks = Arc::new(AtomicUsize::new(0));
        let call = tokio::spawn({
            let (client, current, checks) = (client.clone(), current.clone(), checks.clone());
            async move {
                client
                    .call_tool_checked(
                        McpToolCall::new("fixture", json!({})),
                        CancellationToken::new(),
                        || {
                            checks.fetch_add(1, Ordering::SeqCst);
                            std::future::ready(if current.load(Ordering::SeqCst) {
                                Ok(())
                            } else {
                                Err(Error::protocol("operation authority retired"))
                            })
                        },
                    )
                    .await
            }
        });
        let initial = peer.request("tools/call").await;
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        current.store(false, Ordering::SeqCst);
        peer.reply(&initial, result).await;
        let error = tokio::time::timeout(WAIT, call)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("operation authority retired"));
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert!(matches!(
            peer.receive.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        peer.finish(&client).await;
    }
}

#[tokio::test]
async fn canceled_request_notifies_once_and_ignores_late_reply() {
    let (client, mut peer) = connected(false).await;
    let cancellation = CancellationToken::new();
    let list = tokio::spawn({
        let client = client.clone();
        let cancellation = cancellation.clone();
        async move { client.list_tools(cancellation).await }
    });
    let request = peer.request("tools/list").await;
    cancellation.cancel();
    let notification = peer.request("notifications/cancelled").await;
    assert!(notification.get("id").is_none());
    assert_eq!(notification["params"]["requestId"], request["id"]);
    let error = tokio::time::timeout(WAIT, list)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("canceled"));
    peer.reply(&request, json!({"tools": []})).await;
    assert!(peer.receive.try_recv().is_err());
    peer.finish(&client).await;
}

#[tokio::test]
async fn eof_interrupts_an_ambiguous_send_and_does_not_retransmit() {
    let (client, mut peer) = connected(true).await;
    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    peer.request("tools/list").await;
    drop(peer.send);
    let error = tokio::time::timeout(WAIT, list)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("EOF"));
    assert!(
        tokio::time::timeout(WAIT, peer.disconnects.recv())
            .await
            .unwrap()
            .unwrap()
            .contains("EOF")
    );
    let error = client
        .list_tools(CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("EOF"));
    assert!(peer.receive.try_recv().is_err());
    let shutdown = tokio::spawn(async move { client.shutdown().await });
    tokio::time::timeout(WAIT, &mut peer.shutdown)
        .await
        .unwrap()
        .unwrap()
        .send(Ok(()))
        .unwrap();
    tokio::time::timeout(WAIT, shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cancellation_or_future_drop_does_not_replay_an_ambiguous_send() {
    let (client, mut peer) = connected(true).await;
    let cancellation = CancellationToken::new();
    let cancelled = tokio::spawn({
        let client = client.clone();
        let cancellation = cancellation.clone();
        async move { client.list_tools(cancellation).await }
    });
    let first = peer.request("tools/list").await;
    cancellation.cancel();
    let error = tokio::time::timeout(WAIT, cancelled)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("canceled"));
    let dropped = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    let second = peer.request("tools/list").await;
    assert_ne!(first["id"], second["id"]);
    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    assert!(peer.receive.try_recv().is_err());
    peer.finish(&client).await;
}

#[tokio::test]
async fn rejects_oversized_frames_without_sending_or_decoding_them() {
    let (client, mut peer) = connected(false).await;
    let error = client
        .read_resource(&"x".repeat(LIMIT), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outbound MCP frame exceeds"));
    assert!(peer.receive.try_recv().is_err());

    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    peer.request("tools/list").await;
    peer.send.send(vec![b' '; LIMIT + 1]).await.unwrap();
    let error = tokio::time::timeout(WAIT, list)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("inbound MCP frame exceeds"));
    peer.finish(&client).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_inbound_queue_fails_pending_without_waiting_for_dispatch() {
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = std_mpsc::channel();
    let (client, mut peer) = connected_with_log_gate(
        false,
        Some(LogGate {
            entered: entered_tx,
            release: release_rx,
        }),
    )
    .await;
    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    peer.request("tools/list").await;
    let log = json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}});
    peer.message(log.clone()).await;
    tokio::time::timeout(WAIT, entered).await.unwrap().unwrap();
    let flood = tokio::spawn({
        let send = peer.send.clone();
        let frame = serde_json::to_vec(&log).unwrap();
        async move {
            for _ in 0..64 {
                send.send(frame.clone()).await.unwrap();
            }
        }
    });
    let result = tokio::time::timeout(WAIT, list).await;
    release.send(()).unwrap();
    flood.abort();
    assert!(
        result
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("backlog is full")
    );
    assert!(
        tokio::time::timeout(WAIT, peer.disconnects.recv())
            .await
            .unwrap()
            .unwrap()
            .contains("backlog is full")
    );
    assert!(peer.disconnects.try_recv().is_err());
    peer.finish(&client).await;
}

#[tokio::test]
async fn eof_preserves_a_reply_already_received_from_the_peer() {
    let (client, mut peer) = connected(false).await;
    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    let request = peer.request("tools/list").await;
    peer.reply(&request, json!({"tools": []})).await;
    peer.send = mpsc::channel(1).0;
    assert!(
        tokio::time::timeout(WAIT, list)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_empty()
    );
    assert!(
        tokio::time::timeout(WAIT, peer.disconnects.recv())
            .await
            .unwrap()
            .unwrap()
            .contains("EOF")
    );
    peer.finish(&client).await;
}

#[tokio::test]
async fn permits_one_trailing_cr_and_rejects_other_literal_line_endings() {
    let (client, mut peer) = connected(false).await;
    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_tools(CancellationToken::new()).await }
    });
    let request = peer.request("tools/list").await;
    let mut frame = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": request["id"], "result": {"tools": []},
    }))
    .unwrap();
    frame.push(b'\r');
    peer.send.send(frame).await.unwrap();
    assert!(
        tokio::time::timeout(WAIT, list)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_empty()
    );
    peer.finish(&client).await;

    for line_ending in [
        b"{\r\"jsonrpc\":\"2.0\"}".as_slice(),
        b"{\n\"jsonrpc\":\"2.0\"}",
        b"{}\r\r",
    ] {
        let (client, mut peer) = connected(false).await;
        peer.send.send(line_ending.to_vec()).await.unwrap();
        assert!(
            tokio::time::timeout(WAIT, peer.disconnects.recv())
                .await
                .unwrap()
                .unwrap()
                .contains("contains a line ending")
        );
        peer.finish(&client).await;
    }
}

#[tokio::test]
async fn explicit_shutdown_waits_for_cleanup_and_returns_its_error() {
    let (client, mut peer) = connected(false).await;
    let shutdown = tokio::spawn({
        let client = client.clone();
        async move { client.shutdown().await }
    });
    let complete = tokio::time::timeout(WAIT, &mut peer.shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(!shutdown.is_finished());
    complete
        .send(Err(Error::protocol("cleanup failed")))
        .unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, shutdown)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string(),
        "cleanup failed"
    );
    assert_eq!(
        client.shutdown().await.unwrap_err().to_string(),
        "cleanup failed"
    );
}
