use std::{future::Future, time::Duration};

use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Request, Response, Status as RpcStatus, Streaming,
    transport::{Endpoint, Server},
};
use waltr::grpc::waltr::sandbox::v1::sandbox_service_server::{
    SandboxService, SandboxServiceServer,
};

use super::*;

async fn wait<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .unwrap()
}

fn launch(name: &str, args: &[&str]) -> proto::McpStdioLaunch {
    proto::McpStdioLaunch {
        name: name.into(),
        executable: "/bin/cat".into(),
        arguments: args.iter().map(|arg| (*arg).into()).collect(),
    }
}

fn update(launches: Vec<proto::McpStdioLaunch>) -> HostBody {
    HostBody::McpStdioCatalogUpdate(proto::McpStdioCatalogUpdate {
        request_id: "test-catalog".into(),
        launches,
    })
}

fn open(channel_id: &str, server_name: &str, working_directory: &str) -> HostBody {
    HostBody::McpStdioOpen(proto::McpStdioOpen {
        channel_id: channel_id.into(),
        server_name: server_name.into(),
        working_directory: working_directory.into(),
    })
}

fn frame(channel_id: &str, data: &[u8]) -> HostBody {
    HostBody::McpStdioFrame(proto::McpStdioFrame {
        channel_id: channel_id.into(),
        data: data.into(),
    })
}

fn id() -> String {
    Uuid::new_v4().to_string()
}

struct Fixture {
    sandbox: Arc<Sandbox>,
    stdio: StreamStdio,
    out: mpsc::Receiver<Outbound>,
    fence: CancellationToken,
}

impl Fixture {
    async fn new(dir: &Path, private: bool, definitions: Vec<proto::McpStdioLaunch>) -> Self {
        let sandbox = Arc::new(Sandbox::new(dir).unwrap());
        let catalog = LaunchCatalog::load(sandbox.root().to_owned(), definitions)
            .await
            .unwrap();
        let (tx, out) = mpsc::channel(OUTPUT_DEPTH);
        let fence = CancellationToken::new();
        let stdio = StreamStdio::new(
            private,
            catalog,
            tx,
            fence.clone(),
            None,
            StreamStdio::path_checks(),
        );
        Self {
            sandbox,
            stdio,
            out,
            fence,
        }
    }

    fn send(&mut self, body: HostBody) {
        self.stdio.handle(body, &self.sandbox);
    }

    async fn receive(&mut self) -> WorkerBody {
        wait(async {
            loop {
                tokio::select! {
                    () = self.stdio.next(), if self.stdio.has_tasks() => {}
                    reply = self.out.recv() => {
                        if let Some(message) = reply.unwrap().into_live_message() {
                            return message.body.unwrap();
                        }
                    }
                }
            }
        })
        .await
    }

    async fn opened(&mut self, channel_id: &str, opened: bool) {
        assert_eq!(
            self.receive().await,
            WorkerBody::McpStdioOpened(proto::McpStdioOpened {
                channel_id: channel_id.into(),
                opened,
            })
        );
    }

    async fn terminal(&mut self, channel_id: &str, status: Status) {
        assert_eq!(
            self.receive().await,
            WorkerBody::McpStdioTerminal(proto::McpStdioTerminal {
                channel_id: channel_id.into(),
                status: status as i32,
                reaped: true,
            })
        );
    }

    async fn catalog_ack(&mut self, applied: bool) {
        assert_eq!(
            self.receive().await,
            WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
                request_id: "test-catalog".into(),
                applied,
            })
        );
    }
}

#[tokio::test]
async fn private_catalog_exact_name_and_directory_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("subdir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("marker"), b"worker-only\n").unwrap();
    std::os::unix::fs::symlink(&dir, root.path().join("alias")).unwrap();
    let mut fixture = Fixture::new(root.path(), true, vec![]).await;
    let denied = id();
    fixture.send(open(&denied, "exact", "."));
    fixture.opened(&denied, false).await;

    fixture.send(update(vec![launch("exact", &["marker", "-"])]));
    fixture.catalog_ack(true).await;
    fixture.send(update(vec![launch("exact", &[]), launch("exact", &[])]));
    fixture.catalog_ack(false).await;

    for (name, cwd) in [
        ("Exact", "subdir"),
        ("exact", "../subdir"),
        ("exact", "/"),
        ("exact", "alias"),
        ("exact", "missing"),
        ("exact", ""),
    ] {
        let denied = id();
        fixture.send(open(&denied, name, cwd));
        fixture.opened(&denied, false).await;
    }

    let channel = id();
    fixture.send(open(&channel, "exact", "subdir"));
    fixture.opened(&channel, true).await;
    assert_eq!(
        fixture.receive().await,
        WorkerBody::McpStdioFrame(proto::McpStdioFrame {
            channel_id: channel.clone(),
            data: b"worker-only".into(),
        })
    );
    let opaque = b"opaque \xff payload\r";
    fixture.send(frame(&channel, opaque));
    assert_eq!(
        fixture.receive().await,
        WorkerBody::McpStdioFrame(proto::McpStdioFrame {
            channel_id: channel.clone(),
            data: opaque.into(),
        })
    );
    fixture.send(HostBody::McpStdioClose(proto::McpStdioClose {
        channel_id: channel.clone(),
    }));
    fixture.terminal(&channel, Status::Closed).await;
    wait(fixture.stdio.shutdown()).await;
}

#[tokio::test]
async fn unavailable_executable_only_rejects_its_open_and_pipe_failure_reports_reap() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("partial"), b"no newline").unwrap();
    let mut fixture = Fixture::new(root.path(), true, vec![]).await;
    let missing = proto::McpStdioLaunch {
        name: "missing".into(),
        executable: "definitely-not-installed-waltr-mcp".into(),
        arguments: Vec::new(),
    };
    let malformed = proto::McpStdioLaunch {
        executable: "./relative-command".into(),
        ..missing.clone()
    };
    fixture.send(update(vec![launch("valid", &[]), malformed]));
    fixture.catalog_ack(false).await;
    fixture.send(update(vec![launch("valid", &["partial"]), missing]));
    fixture.catalog_ack(true).await;

    let denied = id();
    fixture.send(open(&denied, "missing", "."));
    fixture.opened(&denied, false).await;
    assert!(!fixture.fence.is_cancelled());
    let valid = id();
    fixture.send(open(&valid, "valid", "."));
    fixture.opened(&valid, true).await;
    fixture.terminal(&valid, Status::Failed).await;
    assert!(!fixture.fence.is_cancelled());
    wait(fixture.stdio.shutdown()).await;
}

#[tokio::test]
async fn hosted_cannot_extend_or_replace_its_export_catalog() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::new(root.path(), false, vec![launch("allowed", &[])]).await;
    fixture.send(update(vec![launch("unauthorized", &[])]));
    fixture.catalog_ack(false).await;
    let denied = id();
    fixture.send(open(&denied, "unauthorized", "."));
    fixture.opened(&denied, false).await;
    let channel = id();
    fixture.send(open(&channel, "allowed", "."));
    fixture.opened(&channel, true).await;
    fixture.send(frame(&channel, &vec![b'a'; FRAME_BYTES + 1]));
    fixture.terminal(&channel, Status::Failed).await;
    wait(fixture.stdio.shutdown()).await;
}

#[tokio::test]
async fn replacement_joins_changed_channel_and_discards_its_queued_frames() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::new(
        root.path(),
        true,
        vec![launch("changed", &[]), launch("same", &[])],
    )
    .await;
    let changed = id();
    fixture.send(open(&changed, "changed", "."));
    fixture.opened(&changed, true).await;
    let same = id();
    fixture.send(open(&same, "same", "."));
    fixture.opened(&same, true).await;
    fixture.send(frame(&changed, b"queued"));
    let queued = wait(fixture.out.recv()).await.unwrap();

    fixture.send(update(vec![launch("same", &[])]));
    fixture.terminal(&changed, Status::Revoked).await;
    fixture.catalog_ack(true).await;
    assert!(queued.into_live_message().is_none());
    fixture.send(frame(&same, b"unchanged"));
    assert!(
        matches!(fixture.receive().await, WorkerBody::McpStdioFrame(reply)
        if reply.channel_id == same && reply.data == b"unchanged")
    );
    fixture.send(open(&changed, "same", "."));
    assert!(fixture.fence.is_cancelled(), "channel IDs cannot be reused");
    wait(fixture.stdio.shutdown()).await;
}

#[tokio::test]
async fn stream_shutdown_fences_queued_frames_without_confirmations() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::new(root.path(), false, vec![launch("cat", &[])]).await;
    let channel = id();
    fixture.send(open(&channel, "cat", "."));
    fixture.opened(&channel, true).await;
    fixture.send(frame(&channel, b"old-generation"));
    let queued = wait(fixture.out.recv()).await.unwrap();
    wait(fixture.stdio.shutdown()).await;
    assert!(queued.into_live_message().is_none());
    assert!(fixture.fence.is_cancelled());
    assert!(wait(fixture.out.recv()).await.is_none());

    let mut next = Fixture::new(root.path(), false, vec![launch("cat", &[])]).await;
    next.send(frame(&channel, b"old-generation"));
    let fresh = id();
    next.send(open(&fresh, "cat", "."));
    next.opened(&fresh, true).await;
    next.send(frame(&fresh, b"new-generation"));
    assert!(
        matches!(next.receive().await, WorkerBody::McpStdioFrame(reply)
        if reply.channel_id == fresh && reply.data == b"new-generation")
    );
    wait(next.stdio.shutdown()).await;
}

#[tokio::test]
async fn saturated_outbound_queue_fences_without_waiting_for_the_peer() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::new(root.path(), false, vec![launch("cat", &[])]).await;
    let channel = id();
    fixture.send(open(&channel, "cat", "."));
    fixture.opened(&channel, true).await;
    for queued in 1..=OUTPUT_DEPTH {
        fixture.send(frame(&channel, b"queued"));
        wait(async {
            while fixture.out.len() < queued {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    fixture.send(frame(&channel, b"overflow"));
    wait(fixture.fence.cancelled()).await;
    wait(fixture.stdio.shutdown()).await;
    for _ in 0..OUTPUT_DEPTH {
        assert!(
            wait(fixture.out.recv())
                .await
                .unwrap()
                .into_live_message()
                .is_none()
        );
    }
    assert!(wait(fixture.out.recv()).await.is_none());
}

#[test]
fn bare_path_resolution_excludes_workspace_entries_and_symlinks_into_export() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let copy = root.join("cat");
    std::fs::copy("/bin/cat", &copy).unwrap();
    let inside_alias = root.join("external-cat");
    std::os::unix::fs::symlink("/bin/cat", inside_alias).unwrap();
    std::os::unix::fs::symlink(&copy, outside.join("cat")).unwrap();
    let path = std::env::join_paths([Path::new("."), &root, &outside]).unwrap();
    assert_eq!(
        resolve_executable(&root, Path::new("cat"), Some(&path)),
        None
    );
    assert_eq!(
        resolve_executable(&root, Path::new("external-cat"), Some(&path)),
        None
    );
    let path = std::env::join_paths([&root, &outside, Path::new("/bin")]).unwrap();
    assert_eq!(
        resolve_executable(&root, Path::new("cat"), Some(&path)),
        Some(PathBuf::from("/bin/cat"))
    );
}

type HostReplies = ReceiverStream<Result<proto::ConnectResponse, RpcStatus>>;
type Dial = (
    Streaming<proto::ConnectRequest>,
    oneshot::Sender<HostReplies>,
);

struct Gateway(mpsc::Sender<Dial>);

#[tonic::async_trait]
impl SandboxService for Gateway {
    type ConnectStream = HostReplies;

    async fn connect(
        &self,
        request: Request<Streaming<proto::ConnectRequest>>,
    ) -> Result<Response<Self::ConnectStream>, RpcStatus> {
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            "Bearer stdio-fixture"
        );
        let (sender, receiver) = oneshot::channel();
        self.0
            .send((request.into_inner(), sender))
            .await
            .map_err(|_| RpcStatus::cancelled("fixture ended"))?;
        Ok(Response::new(
            receiver
                .await
                .map_err(|_| RpcStatus::cancelled("fixture ended"))?,
        ))
    }
}

type HostSender = mpsc::Sender<Result<proto::ConnectResponse, RpcStatus>>;

async fn admit(dials: &mut mpsc::Receiver<Dial>) -> (HostSender, Streaming<proto::ConnectRequest>) {
    let (mut input, replies) = wait(dials.recv()).await.unwrap();
    assert!(matches!(body(&mut input).await, WorkerBody::Hello(hello)
        if hello.mcp_stdio_version == VERSION && hello.workspace_id == "fixture"));
    let (send, receive) = mpsc::channel(4);
    assert!(replies.send(ReceiverStream::new(receive)).is_ok());
    (send, input)
}

async fn send(target: &HostSender, body: HostBody) {
    wait(target.send(Ok(proto::ConnectResponse { body: Some(body) })))
        .await
        .unwrap();
}

async fn body(stream: &mut Streaming<proto::ConnectRequest>) -> WorkerBody {
    wait(stream.message()).await.unwrap().unwrap().body.unwrap()
}

#[tokio::test]
async fn outbound_reconnect_requires_an_explicit_new_open() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::new(root.path()).unwrap();
    let catalog = LaunchCatalog::load(sandbox.root().to_owned(), vec![launch("cat", &[])])
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint::from_shared(format!("http://{}", listener.local_addr().unwrap()))
        .unwrap()
        .connect_lazy();
    let (dial_tx, mut dials) = mpsc::channel(1);
    let server = tokio::spawn(
        Server::builder()
            .add_service(SandboxServiceServer::new(Gateway(dial_tx)))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let stop = CancellationToken::new();
    let worker = tokio::spawn(crate::wire::run_worker_with_dial_authorization(
        endpoint,
        sandbox.root().to_owned(),
        "fixture".into(),
        stop.clone(),
        Arc::new(AtomicBool::new(false)),
        catalog,
        || {
            Some(crate::wire::DialAuthorization {
                header: "Bearer stdio-fixture".parse().unwrap(),
                expires: None,
            })
        },
    ));

    let (host, mut input) = admit(&mut dials).await;
    send(&host, update(vec![launch("new", &[])])).await;
    assert!(matches!(body(&mut input).await, WorkerBody::McpStdioCatalogAck(ack) if !ack.applied));
    let old = id();
    send(&host, open(&old, "cat", ".")).await;
    assert!(
        matches!(body(&mut input).await, WorkerBody::McpStdioOpened(opened)
        if opened.opened && opened.channel_id == old)
    );
    send(&host, frame(&old, b"old-generation")).await;
    drop(host);
    drop(input);

    let (host, mut input) = admit(&mut dials).await;
    send(&host, frame(&old, b"late")).await;
    let fresh = id();
    send(&host, open(&fresh, "cat", ".")).await;
    assert!(
        matches!(body(&mut input).await, WorkerBody::McpStdioOpened(opened)
        if opened.opened && opened.channel_id == fresh)
    );
    send(&host, frame(&fresh, b"fresh")).await;
    assert!(
        matches!(body(&mut input).await, WorkerBody::McpStdioFrame(reply)
        if reply.channel_id == fresh && reply.data == b"fresh")
    );
    send(
        &host,
        HostBody::McpStdioClose(proto::McpStdioClose {
            channel_id: fresh.clone(),
        }),
    )
    .await;
    assert!(
        matches!(body(&mut input).await, WorkerBody::McpStdioTerminal(reply)
        if reply.channel_id == fresh && reply.reaped && reply.status == Status::Closed as i32)
    );

    stop.cancel();
    wait(worker).await.unwrap().unwrap();
    server.abort();
}
