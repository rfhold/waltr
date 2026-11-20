//! Loopback exports and private sidecar control.
#![cfg(feature = "rpc")]

use std::{
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, UnixListener},
    sync::{mpsc, oneshot, watch},
    time::timeout,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream, UnixListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::{
    Request, Response, Status, Streaming,
    metadata::AsciiMetadataValue,
    transport::{Endpoint, Server},
};
use waltr::grpc::waltr::sandbox::v1::{
    self as p,
    connect_request::Body as Worker,
    connect_response::Body as Host,
    sandbox_service_server::{SandboxService, SandboxServiceServer},
    workspace_export_control::Change,
};
use waltr_agent_sandbox::peer_config::{CheckedExport, CheckedExports, WorkspaceExport};

const TOKEN: &str = "Bearer fixture-user-access-token";
const NEXT_TOKEN: &str = "Bearer fixture-refreshed-user-access-token";
const WAIT: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const FIXTURE: &str = "WALTR_TEST_OUTBOUND_FIXTURE";

fn export(endpoint: &str, root: &Path) -> CheckedExport {
    CheckedExports::load(&[WorkspaceExport {
        endpoint: endpoint.into(),
        machine_id: "laptop".into(),
        export_root: root.into(),
    }])
    .unwrap()
    .matching(&url::Url::parse(endpoint).unwrap())
    .unwrap()
}

type Replies = mpsc::Sender<Result<p::ConnectResponse, Status>>;
type Output = ReceiverStream<Result<p::ConnectResponse, Status>>;

struct Attempt {
    authorization: Option<AsciiMetadataValue>,
    hello: p::Hello,
    input: Streaming<p::ConnectRequest>,
    admit: oneshot::Sender<Output>,
}

impl Attempt {
    fn admit(self) -> (Replies, Streaming<p::ConnectRequest>) {
        let (tx, rx) = mpsc::channel(4);
        assert!(self.admit.send(ReceiverStream::new(rx)).is_ok());
        (tx, self.input)
    }
}

struct TestGateway(mpsc::Sender<Attempt>);

#[tonic::async_trait]
impl SandboxService for TestGateway {
    type ConnectStream = Output;

    async fn connect(
        &self,
        request: Request<Streaming<p::ConnectRequest>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let authorization = request.metadata().get("authorization").cloned();
        let mut input = request.into_inner();
        let hello = worker_hello(&mut input).await?;
        let (admit, output) = oneshot::channel();
        self.0
            .send(Attempt {
                authorization,
                hello,
                input,
                admit,
            })
            .await
            .map_err(|_| Status::cancelled("fixture ended"))?;
        let output = output
            .await
            .map_err(|_| Status::cancelled("fixture ended"))?;
        Ok(Response::new(output))
    }
}

async fn worker_hello(input: &mut Streaming<p::ConnectRequest>) -> Result<p::Hello, Status> {
    let Some(Worker::Hello(hello)) = input.message().await?.and_then(|m| m.body) else {
        return Err(Status::invalid_argument("missing hello"));
    };
    Ok(hello)
}

struct Gateway {
    attempts: mpsc::Receiver<Attempt>,
    task: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Gateway {
    fn start(listener: TcpListener) -> Self {
        let (attempts, rx) = mpsc::channel(4);
        let task = tokio::spawn(
            Server::builder()
                .add_service(SandboxServiceServer::new(TestGateway(attempts)))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        Self { attempts: rx, task }
    }

    async fn next(&mut self, authorization: &str) -> Attempt {
        let attempt = timeout(WAIT, self.attempts.recv()).await.unwrap().unwrap();
        assert_eq!(
            attempt.authorization.as_ref().unwrap().to_str().unwrap(),
            authorization
        );
        assert_eq!(attempt.hello.workspace_id, "laptop");
        assert_eq!(
            attempt.hello.tools.iter().any(|tool| tool == "shell"),
            cfg!(feature = "shell")
        );
        attempt
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn send_workspace_request(
    tx: &Replies,
    id: &str,
    operation: &str,
    path: &str,
    working_directory: Option<String>,
) {
    tx.send(Ok(p::ConnectResponse {
        body: Some(Host::WorkspaceRequest(p::WorkspaceRequest {
            request_id: id.into(),
            operation: operation.into(),
            path: path.into(),
            working_directory,
            ..Default::default()
        })),
    }))
    .await
    .unwrap();
}

async fn workspace_reply(
    input: &mut Streaming<p::ConnectRequest>,
    id: &str,
) -> p::WorkspaceResponse {
    let message = timeout(WAIT, input.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Some(Worker::WorkspaceResponse(reply)) = message.body else {
        panic!("expected a workspace reply")
    };
    assert_eq!(reply.request_id, id);
    assert!(reply.error.is_empty(), "{}", reply.error);
    reply
}

fn access(token: &str) -> p::WorkspaceExportAccess {
    p::WorkspaceExportAccess {
        access_token: token.strip_prefix("Bearer ").unwrap().into(),
        expires_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60,
    }
}

fn apply(endpoint: &str, root: &str, token: &str) -> Change {
    Change::Apply(p::WorkspaceExportApply {
        endpoint: endpoint.into(),
        machine_id: "laptop".into(),
        export_root: root.into(),
        access: Some(access(token)),
        mcp_stdio_launches: Vec::new(),
    })
}

async fn send_control(tx: &Replies, id: &str, change: Change) {
    tx.send(Ok(p::ConnectResponse {
        body: Some(Host::WorkspaceExportControl(p::WorkspaceExportControl {
            request_id: id.into(),
            binding: "work".into(),
            change: Some(change),
        })),
    }))
    .await
    .unwrap();
}

async fn acknowledged(input: &mut Streaming<p::ConnectRequest>, id: &str, applied: bool) {
    let message = timeout(WAIT, input.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Some(Worker::WorkspaceExportAck(ack)) = message.body else {
        panic!("expected a control acknowledgement")
    };
    assert_eq!(ack.request_id, id);
    assert_eq!(ack.applied, applied);
}

#[tokio::test]
async fn expired_private_lease_stops_dialing_until_rotated() {
    let root = tempfile::tempdir().unwrap();
    let private = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let channel = Endpoint::from_shared(format!("http://{}", private.local_addr().unwrap()))
        .unwrap()
        .connect_lazy();
    let mut private = Gateway::start(private);
    let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", remote.local_addr().unwrap());
    let mut destination = Gateway::start(remote);
    let stop = CancellationToken::new();
    let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_shutdown(
        channel,
        root.path().to_owned(),
        "local",
        stop.clone(),
        Arc::new(AtomicBool::new(false)),
    ));
    let (tx, mut input) = timeout(WAIT, private.attempts.recv())
        .await
        .unwrap()
        .unwrap()
        .admit();
    let Change::Apply(mut apply) = apply(&endpoint, root.path().to_str().unwrap(), TOKEN) else {
        unreachable!()
    };
    apply.access.as_mut().unwrap().expires_at_unix -= 60;
    send_control(&tx, "expired", Change::Apply(apply.clone())).await;
    acknowledged(&mut input, "expired", false).await;
    apply.access.as_mut().unwrap().expires_at_unix += 2;
    send_control(&tx, "brief", Change::Apply(apply)).await;
    acknowledged(&mut input, "brief", true).await;
    let first = destination.next(TOKEN).await;
    let worker_id = first.hello.worker_id.clone();
    let (_remote, mut output) = first.admit();
    timeout(WAIT, async {
        while let Ok(Some(_)) = output.message().await {}
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(200), destination.attempts.recv())
            .await
            .is_err()
    );
    send_control(&tx, "renew", Change::Rotate(access(NEXT_TOKEN))).await;
    acknowledged(&mut input, "renew", true).await;
    assert_eq!(
        destination.next(NEXT_TOKEN).await.hello.worker_id,
        worker_id
    );
    stop.cancel();
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn private_uds_control_uses_sidecar_home_rotates_and_retires_exports() {
    let root = tempfile::tempdir().unwrap();
    let local = root.path().join("local");
    let home = root.path().join("worker-home");
    std::fs::create_dir(&local).unwrap();
    std::fs::create_dir_all(home.join("export/repo")).unwrap();
    std::fs::write(local.join("sentinel"), "private local bytes").unwrap();
    std::fs::write(home.join("export/repo/sentinel"), "export worker bytes").unwrap();

    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", tcp.local_addr().unwrap());
    let mut destination = Gateway::start(tcp);
    let socket = root.path().join("sandbox.sock");
    let (attempts, mut private_attempts) = mpsc::channel(4);
    let private = tokio::spawn(
        Server::builder()
            .add_service(SandboxServiceServer::new(TestGateway(attempts)))
            .serve_with_incoming(UnixListenerStream::new(
                UnixListener::bind(&socket).unwrap(),
            )),
    );
    let mut worker = tokio::process::Command::new(env!("CARGO_BIN_EXE_waltr-agent-sandbox"))
        .env("WALTR_SANDBOX_SOCKET", &socket)
        .env("WALTR_SANDBOX_WORKSPACE_ROOT", &local)
        .env("HOME", &home)
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let attempt = timeout(WAIT, private_attempts.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(attempt.authorization.is_none());
    let local_id = attempt.hello.worker_id.clone();
    let (tx, mut input) = attempt.admit();
    send_control(&tx, "bad-root", apply(&endpoint, "~/missing", TOKEN)).await;
    acknowledged(&mut input, "bad-root", false).await;
    send_control(&tx, "start", apply(&endpoint, "~/export", TOKEN)).await;
    acknowledged(&mut input, "start", true).await;

    let first = destination.next(TOKEN).await;
    assert_eq!(
        Path::new(&first.hello.canonical_root),
        home.join("export").canonicalize().unwrap()
    );
    let worker_id = first.hello.worker_id.clone();
    let (remote, mut exported) = first.admit();
    send_workspace_request(&remote, "bind", "resolve_working_directory", "repo", None).await;
    let bound = workspace_reply(&mut exported, "bind").await;
    send_control(&tx, "rotate", Change::Rotate(access(NEXT_TOKEN))).await;
    acknowledged(&mut input, "rotate", true).await;
    send_control(&tx, "apply-same", apply(&endpoint, "~/export", NEXT_TOKEN)).await;
    acknowledged(&mut input, "apply-same", true).await;
    send_workspace_request(&remote, "read", "read", "sentinel", bound.working_directory).await;
    assert_eq!(
        workspace_reply(&mut exported, "read").await.content,
        "export worker bytes"
    );
    send_workspace_request(&tx, "private-read", "read", "sentinel", None).await;
    assert_eq!(
        workspace_reply(&mut input, "private-read").await.content,
        "private local bytes"
    );

    drop(remote);
    drop(exported);
    let next = destination.next(NEXT_TOKEN).await;
    assert_eq!(next.hello.worker_id, worker_id);
    let (remote, mut exported) = next.admit();
    send_control(&tx, "remove", Change::Remove(p::WorkspaceExportRemove {})).await;
    acknowledged(&mut input, "remove", true).await;
    drop(remote);
    timeout(WAIT, async {
        while let Ok(Some(_)) = exported.message().await {}
    })
    .await
    .unwrap();
    send_control(&tx, "not-running", Change::Rotate(access(NEXT_TOKEN))).await;
    acknowledged(&mut input, "not-running", false).await;

    send_control(&tx, "start-again", apply(&endpoint, "~/export", TOKEN)).await;
    acknowledged(&mut input, "start-again", true).await;
    let (remote, mut exported) = destination.next(TOKEN).await.admit();
    drop(tx);
    drop(input);
    timeout(WAIT, async {
        while let Ok(Some(_)) = exported.message().await {}
    })
    .await
    .unwrap();
    drop(remote);
    let private_reconnected = timeout(WAIT, private_attempts.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(private_reconnected.hello.worker_id, local_id);
    let (tx, mut input) = private_reconnected.admit();
    // Ordinary workspace replies retain the existing reconnect semantics.
    workspace_reply(&mut input, "private-read").await;
    send_control(&tx, "not-replayed", Change::Rotate(access(NEXT_TOKEN))).await;
    acknowledged(&mut input, "not-replayed", false).await;
    assert!(
        timeout(Duration::from_millis(200), destination.attempts.recv())
            .await
            .is_err()
    );
    worker.kill().await.unwrap();
    private.abort();
    drop(private.await);
}

#[test]
#[allow(clippy::too_many_lines)]
fn private_loss_retires_outbound_export_while_replacement_validation_is_pending() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let root = tempfile::tempdir().unwrap();
            let private = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let channel =
                Endpoint::from_shared(format!("http://{}", private.local_addr().unwrap()))
                    .unwrap()
                    .connect_lazy();
            let mut private = Gateway::start(private);
            let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", remote.local_addr().unwrap());
            let mut destination = Gateway::start(remote);
            let replacement = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let replacement_endpoint = format!("http://{}", replacement.local_addr().unwrap());
            let mut replacement = Gateway::start(replacement);
            let stop = CancellationToken::new();
            let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_shutdown(
                channel,
                root.path().to_owned(),
                "local",
                stop.clone(),
                Arc::new(AtomicBool::new(false)),
            ));
            let (tx, mut input) = timeout(WAIT, private.attempts.recv())
                .await
                .unwrap()
                .unwrap()
                .admit();
            send_control(
                &tx,
                "start",
                apply(&endpoint, root.path().to_str().unwrap(), TOKEN),
            )
            .await;
            acknowledged(&mut input, "start", true).await;
            let (remote, mut exported) = destination.next(TOKEN).await.admit();
            send_workspace_request(&remote, "active", "info", ".", None).await;
            workspace_reply(&mut exported, "active").await;

            // Keep filesystem validation queued without using a slow or special filesystem.
            let (started, occupied) = oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                blocked.recv_timeout(3 * WAIT)
            });
            timeout(WAIT, occupied).await.unwrap().unwrap();
            send_control(
                &tx,
                "replace",
                apply(
                    &replacement_endpoint,
                    root.path().to_str().unwrap(),
                    NEXT_TOKEN,
                ),
            )
            .await;
            let channel_id = uuid::Uuid::new_v4().to_string();
            tx.send(Ok(p::ConnectResponse {
                body: Some(Host::McpStdioOpen(p::McpStdioOpen {
                    channel_id: channel_id.clone(),
                    server_name: "not-granted".into(),
                    working_directory: ".".into(),
                })),
            }))
            .await
            .unwrap();
            let reply = timeout(WAIT, input.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(matches!(reply.body, Some(Worker::McpStdioOpened(opened))
                if opened.channel_id == channel_id && !opened.opened));

            drop(tx);
            drop(input);
            assert!(!matches!(
                timeout(WAIT, exported.message()).await.unwrap(),
                Ok(Some(_))
            ));
            drop(remote);
            release.send(()).unwrap();
            timeout(WAIT, blocker).await.unwrap().unwrap().unwrap();

            let (tx, mut input) = timeout(WAIT, private.attempts.recv())
                .await
                .unwrap()
                .unwrap()
                .admit();
            send_control(&tx, "new-epoch", Change::Rotate(access(NEXT_TOKEN))).await;
            acknowledged(&mut input, "new-epoch", false).await;
            assert!(
                timeout(Duration::from_millis(200), replacement.attempts.recv())
                    .await
                    .is_err()
            );
            stop.cancel();
            timeout(WAIT, worker).await.unwrap().unwrap().unwrap();
        });
}

#[tokio::test]
async fn hosted_outbound_worker_ignores_private_export_control() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("sentinel"), "ordinary export").unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", tcp.local_addr().unwrap());
    let mut destination = Gateway::start(tcp);
    let forbidden = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let forbidden_endpoint = format!("http://{}", forbidden.local_addr().unwrap());
    let worker = export(&endpoint, root.path()).start(|| Some(TOKEN.parse().unwrap()));
    let (tx, mut input) = destination.next(TOKEN).await.admit();
    send_control(
        &tx,
        "not-private",
        apply(
            &forbidden_endpoint,
            root.path().to_str().unwrap(),
            NEXT_TOKEN,
        ),
    )
    .await;
    send_workspace_request(&tx, "read", "read", "sentinel", None).await;
    assert_eq!(
        workspace_reply(&mut input, "read").await.content,
        "ordinary export"
    );
    assert!(
        timeout(Duration::from_millis(200), forbidden.accept())
            .await
            .is_err()
    );
    worker.shutdown().await;
}

#[tokio::test]
async fn export_waits_for_auth_and_redials_with_current_bearer_and_worker_state() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("sentinel"), "desktop bytes").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let credential = Arc::new(Mutex::new(None::<&str>));
    let (no_credential, mut skipped) = watch::channel(());
    let workers = export(&endpoint, root.path()).start({
        let credential = credential.clone();
        move || {
            let credential = *credential.lock().unwrap();
            if credential.is_none() {
                no_credential.send_replace(());
            }
            credential.map(|value| value.parse().unwrap())
        }
    });
    timeout(WAIT, skipped.changed()).await.unwrap().unwrap();
    assert!(
        timeout(Duration::from_millis(150), listener.accept())
            .await
            .is_err()
    );

    let mut gateway = Gateway::start(listener);
    *credential.lock().unwrap() = Some(TOKEN);
    let first = gateway.next(TOKEN).await;
    let worker_id = first.hello.worker_id.clone();
    let (tx, mut input) = first.admit();
    send_workspace_request(&tx, "read", "read", "sentinel", None).await;
    assert_eq!(
        workspace_reply(&mut input, "read").await.content,
        "desktop bytes"
    );
    skipped.mark_unchanged();
    *credential.lock().unwrap() = None;
    drop(input);
    drop(tx);
    timeout(WAIT, skipped.changed()).await.unwrap().unwrap();
    assert!(
        timeout(Duration::from_millis(150), gateway.attempts.recv())
            .await
            .is_err()
    );

    *credential.lock().unwrap() = Some(NEXT_TOKEN);
    let next = gateway.next(NEXT_TOKEN).await;
    assert_eq!(next.hello.worker_id, worker_id);
    let (_tx, mut input) = next.admit();
    assert_eq!(
        workspace_reply(&mut input, "read").await.content,
        "desktop bytes"
    );
    timeout(Duration::from_secs(1), workers.shutdown())
        .await
        .unwrap();
    assert!(!matches!(
        timeout(WAIT, input.message()).await.unwrap(),
        Ok(Some(_))
    ));
}

#[tokio::test]
async fn shutdown_cancels_an_in_progress_export_handshake() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut gateway = Gateway::start(listener);
    let workers = export(&endpoint, root.path()).start(|| Some(TOKEN.parse().unwrap()));
    let mut attempt = gateway.next(TOKEN).await;
    timeout(Duration::from_secs(1), workers.shutdown())
        .await
        .unwrap();
    timeout(WAIT, attempt.admit.closed()).await.unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "subprocess fixture for desktop_exports_worker_mount_to_host_without_read_access"]
async fn restricted_gateway_fixture() {
    let Some(dir) = std::env::var_os(FIXTURE).map(std::path::PathBuf::from) else {
        return;
    };
    let err = std::fs::read(dir.join("home/repo/sentinel")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let mut gateway = Gateway::start(tcp);
    std::fs::write(dir.join("endpoint.tmp"), format!("http://{addr}")).unwrap();
    std::fs::rename(dir.join("endpoint.tmp"), dir.join("endpoint")).unwrap();
    let (tx, mut input) = gateway.next(TOKEN).await.admit();
    send_workspace_request(&tx, "resolve", "resolve_working_directory", "repo", None).await;
    let directory = workspace_reply(&mut input, "resolve").await;
    assert_eq!(directory.working_directory.as_deref(), Some("repo"));
    send_workspace_request(&tx, "read", "read", "sentinel", directory.working_directory).await;
    assert_eq!(
        workspace_reply(&mut input, "read").await.content,
        "worker-owned bytes"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn desktop_exports_worker_mount_to_host_without_read_access() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let home = dir.join("home");
    let hosted_home = dir.join("hosted-home");
    std::fs::create_dir_all(home.join("repo")).unwrap();
    std::fs::create_dir_all(hosted_home.join("repo")).unwrap();
    std::fs::write(home.join("repo/sentinel"), "worker-owned bytes").unwrap();
    let profile = format!(
        "(version 1)(allow default)(deny file-read* (subpath {}))",
        serde_json::to_string(home.to_str().unwrap()).unwrap()
    );
    let mut hosted = tokio::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "restricted_gateway_fixture",
            "--nocapture",
        ])
        .env(FIXTURE, &dir)
        .env("HOME", &hosted_home)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let endpoint = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(endpoint) = std::fs::read_to_string(dir.join("endpoint")) {
                break endpoint;
            }
            assert!(hosted.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let workers = export(&endpoint, &home).start(|| Some(TOKEN.parse().unwrap()));
    let status = tokio::time::timeout(Duration::from_secs(15), hosted.wait())
        .await
        .unwrap()
        .unwrap();
    workers.shutdown().await;
    assert!(status.success(), "{status}");
}
