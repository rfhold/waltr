//! Process-local, non-network sandbox transport and worker ownership.

use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use hyper_util::rt::TokioIo;
use waltr_agent::sandbox_gateway::{SandboxGateway, SandboxHandle};
use tokio::{io::DuplexStream, net::UnixListener, sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server};

const WORKSPACE_ID: &str = "local";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Host-owned worker transport; embedded hosts never select the socket variant.
pub(crate) enum SandboxHost {
    Local(LocalSandbox),
    Socket(SocketSandbox),
}

impl SandboxHost {
    pub(crate) async fn start(root: PathBuf, socket: Option<PathBuf>) -> io::Result<Self> {
        match socket {
            Some(socket) => SocketSandbox::start(root, socket).await.map(Self::Socket),
            None => LocalSandbox::start(root).await.map(Self::Local),
        }
    }

    pub(crate) fn handle(&self) -> SandboxHandle {
        match self {
            Self::Local(local) => local.handle(),
            Self::Socket(socket) => socket.gateway.handle(),
        }
    }

    pub(crate) fn health(&self) -> Arc<AtomicBool> {
        match self {
            Self::Local(local) => local.health(),
            Self::Socket(socket) => socket.alive.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn stop_worker_for_test(&self) {
        match self {
            Self::Local(local) => local.stop_worker_for_test(),
            Self::Socket(_) => panic!("test only supports the embedded worker"),
        }
    }

    pub(crate) async fn shutdown(self) {
        match self {
            Self::Local(local) => local.shutdown().await,
            Self::Socket(socket) => socket.shutdown().await,
        }
    }
}

pub(crate) struct SocketSandbox {
    gateway: SandboxGateway,
    server: JoinHandle<Result<(), tonic::transport::Error>>,
    monitor: JoinHandle<()>,
    stop: CancellationToken,
    alive: Arc<AtomicBool>,
    path: PathBuf,
    inode: (u64, u64),
}

impl SocketSandbox {
    async fn start(root: PathBuf, path: PathBuf) -> io::Result<Self> {
        if !root.is_absolute() || !path.is_absolute() || !path.parent().is_some_and(|p| p.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox root and socket must have absolute paths and an existing socket directory",
            ));
        }
        match path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_socket() => {
                if tokio::net::UnixStream::connect(&path).await.is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "sandbox socket is already active",
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "sandbox socket path is not a socket",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = path.metadata()?;
        let inode = (metadata.dev(), metadata.ino());
        let gateway = SandboxGateway::new(WORKSPACE_ID.to_owned()).with_expected_root(root);
        let stop = CancellationToken::new();
        let alive = Arc::new(AtomicBool::new(false));
        let mut ready = gateway.readiness();
        let monitor_alive = alive.clone();
        let monitor_stop = stop.clone();
        let monitor = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = monitor_stop.cancelled() => break,
                    changed = ready.changed() => {
                        if changed.is_err() { break; }
                        monitor_alive.store(*ready.borrow_and_update(), Ordering::Release);
                    }
                }
            }
            monitor_alive.store(false, Ordering::Release);
        });
        let server_gateway = gateway.clone();
        let server_stop = stop.clone();
        let server_alive = alive.clone();
        let server = tokio::spawn(async move {
            let _health = HealthGuard(server_alive);
            let result = Server::builder()
                .add_service(server_gateway.service())
                .serve_with_incoming_shutdown(
                    UnixListenerStream::new(listener),
                    server_stop.clone().cancelled_owned(),
                )
                .await;
            if !server_stop.is_cancelled() {
                tracing::error!("private sandbox socket service stopped unexpectedly");
                server_gateway.shutdown();
            }
            result
        });
        let mut sandbox = Self {
            gateway,
            server,
            monitor,
            stop,
            alive,
            path,
            inode,
        };
        let connected = tokio::time::timeout(SOCKET_STARTUP_TIMEOUT, async {
            tokio::select! {
                ready = sandbox.gateway.wait_ready() => ready.map_err(io::Error::other),
                result = &mut sandbox.server => Err(io::Error::other(format!("sandbox socket service exited during startup: {result:?}"))),
            }
        }).await;
        match connected {
            Ok(Ok(())) if sandbox.gateway.handle().canonical_root().is_some() => {
                sandbox.alive.store(true, Ordering::Release);
                Ok(sandbox)
            }
            Ok(Ok(())) => {
                sandbox.shutdown().await;
                Err(io::Error::other(
                    "sandbox worker disconnected during startup",
                ))
            }
            result => {
                sandbox.shutdown().await;
                Err(match result {
                    Ok(Err(error)) => error,
                    Err(_) => io::Error::new(
                        io::ErrorKind::TimedOut,
                        "sandbox worker did not connect to private socket",
                    ),
                    Ok(Ok(())) => unreachable!(),
                })
            }
        }
    }

    async fn shutdown(self) {
        self.alive.store(false, Ordering::Release);
        self.gateway.shutdown();
        self.stop.cancel();
        let mut server = self.server;
        if tokio::time::timeout(Duration::from_secs(3), &mut server)
            .await
            .is_err()
        {
            server.abort();
            drop(server.await);
        }
        drop(self.monitor.await);
        if self.path.metadata().is_ok_and(|metadata| {
            metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == self.inode
        }) {
            if let Err(error) = std::fs::remove_file(&self.path) {
                tracing::warn!(%error, "failed to remove private sandbox socket");
            }
        }
    }
}

struct HealthGuard(Arc<AtomicBool>);

impl Drop for HealthGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One private Tonic service and its local worker.
pub(crate) struct LocalSandbox {
    gateway: SandboxGateway,
    server: JoinHandle<Result<(), tonic::transport::Error>>,
    worker: JoinHandle<Result<(), waltr_agent_sandbox::SandboxError>>,
    stop: CancellationToken,
    alive: Arc<AtomicBool>,
}

impl std::fmt::Debug for LocalSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSandbox").finish_non_exhaustive()
    }
}

impl LocalSandbox {
    pub(crate) async fn start(root: PathBuf) -> io::Result<Self> {
        let gateway = SandboxGateway::new(WORKSPACE_ID.to_owned());
        let stop = CancellationToken::new();
        let alive = Arc::new(AtomicBool::new(true));
        let (incoming, receiver) = mpsc::channel::<Result<DuplexStream, io::Error>>(1);
        let server_gateway = gateway.clone();
        let server_stop = stop.clone();
        let server_alive = alive.clone();
        let server = tokio::spawn(async move {
            let _health = HealthGuard(server_alive);
            let result = Server::builder()
                .add_service(server_gateway.service())
                .serve_with_incoming_shutdown(
                    ReceiverStream::new(receiver),
                    server_stop.clone().cancelled_owned(),
                )
                .await;
            if !server_stop.is_cancelled() {
                tracing::error!("private sandbox service stopped unexpectedly");
            }
            result
        });
        let channel = tokio::time::timeout(
            STARTUP_TIMEOUT,
            Endpoint::from_static("http://sandbox.local").connect_with_connector(
                tower::service_fn(move |_| {
                    let incoming = incoming.clone();
                    async move {
                        let (client, server) = tokio::io::duplex(64 * 1024);
                        incoming.send(Ok(server)).await.map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                "sandbox service stopped",
                            )
                        })?;
                        Ok::<_, io::Error>(TokioIo::new(client))
                    }
                }),
            ),
        )
        .await;
        let channel = match channel {
            Ok(Ok(channel)) => channel,
            result => {
                gateway.shutdown();
                stop.cancel();
                server.abort();
                drop(server.await);
                return Err(match result {
                    Ok(Err(error)) => io::Error::other(error),
                    Err(_) => {
                        io::Error::new(io::ErrorKind::TimedOut, "sandbox service did not connect")
                    }
                    Ok(Ok(_)) => unreachable!(),
                });
            }
        };
        let worker_alive = alive.clone();
        let worker_stop = stop.clone();
        let mut sandbox = Self {
            gateway,
            server,
            worker: tokio::spawn(async move {
                let _health = HealthGuard(worker_alive.clone());
                let result = waltr_agent_sandbox::run_worker_with_shutdown(
                    channel,
                    root,
                    WORKSPACE_ID,
                    worker_stop.clone(),
                    worker_alive.clone(),
                )
                .await;
                if !worker_stop.is_cancelled() {
                    tracing::error!("sandbox worker stopped unexpectedly");
                }
                result
            }),
            stop,
            alive,
        };
        let ready = tokio::time::timeout(STARTUP_TIMEOUT, async {
            tokio::select! {
                ready = sandbox.gateway.wait_ready() => ready.map_err(io::Error::other),
                result = &mut sandbox.worker => Err(io::Error::other(format!("sandbox worker exited during startup: {result:?}"))),
                result = &mut sandbox.server => Err(io::Error::other(format!("sandbox service exited during startup: {result:?}"))),
            }
        }).await;
        match ready {
            Ok(Ok(())) if sandbox.alive.load(Ordering::Acquire) => Ok(sandbox),
            Ok(Ok(())) => {
                sandbox.shutdown().await;
                Err(io::Error::other(
                    "sandbox worker disconnected during startup",
                ))
            }
            Ok(Err(error)) => {
                sandbox.shutdown().await;
                Err(error)
            }
            Err(_) => {
                sandbox.shutdown().await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "sandbox worker did not connect",
                ))
            }
        }
    }

    pub(crate) fn handle(&self) -> SandboxHandle {
        self.gateway.handle()
    }

    pub(crate) fn health(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    #[cfg(test)]
    pub(crate) fn stop_worker_for_test(&self) {
        self.worker.abort();
    }

    pub(crate) async fn shutdown(self) {
        self.alive.store(false, Ordering::Release);
        self.gateway.shutdown();
        self.stop.cancel();
        let mut worker = self.worker;
        if !worker.is_finished()
            && tokio::time::timeout(Duration::from_secs(3), &mut worker)
                .await
                .is_err()
        {
            worker.abort();
            drop(worker.await);
        }
        let mut server = self.server;
        if !server.is_finished()
            && tokio::time::timeout(Duration::from_secs(3), &mut server)
                .await
                .is_err()
        {
            server.abort();
            drop(server.await);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::UnixStream;
    use tokio::sync::Mutex;

    #[cfg(feature = "shell")]
    mod in_flight_reconnect {
        use super::*;
        use waltr::grpc::waltr::sandbox::v1::{
            self as proto,
            connect_request::Body as WorkerBody,
            connect_response::Body as HostBody,
            sandbox_service_server::{SandboxService, SandboxServiceServer},
        };
        use std::{pin::Pin, sync::atomic::AtomicUsize};
        use tokio::sync::Notify;
        use tokio_stream::Stream;
        use tonic::{Request, Response, Status};

        struct Session {
            incoming: tonic::Streaming<proto::ConnectRequest>,
            outbound: mpsc::Sender<Result<proto::ConnectResponse, Status>>,
        }

        #[derive(Clone)]
        struct Host {
            sessions: mpsc::Sender<Session>,
            attempts: Arc<AtomicUsize>,
            allow_reconnect: Arc<Notify>,
        }

        #[tonic::async_trait]
        impl SandboxService for Host {
            type ConnectStream =
                Pin<Box<dyn Stream<Item = Result<proto::ConnectResponse, Status>> + Send>>;

            async fn connect(
                &self,
                request: Request<tonic::Streaming<proto::ConnectRequest>>,
            ) -> Result<Response<Self::ConnectStream>, Status> {
                if self.attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                    self.allow_reconnect.notified().await;
                }
                let (outbound, receiver) = mpsc::channel(16);
                self.sessions
                    .send(Session {
                        incoming: request.into_inner(),
                        outbound,
                    })
                    .await
                    .map_err(|_| Status::unavailable("host stopped"))?;
                Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
            }
        }

        async fn recv(session: &mut Session) -> WorkerBody {
            tokio::time::timeout(STARTUP_TIMEOUT, session.incoming.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .body
                .unwrap()
        }

        async fn send(session: &Session, body: HostBody) {
            session
                .outbound
                .send(Ok(proto::ConnectResponse { body: Some(body) }))
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn active_execute_survives_private_stream_loss_and_replays_once() {
            let root = crate::test_support::temp_root();
            let (sessions, mut received) = mpsc::channel(4);
            let allow_reconnect = Arc::new(Notify::new());
            let host = Host {
                sessions,
                attempts: Arc::new(AtomicUsize::new(0)),
                allow_reconnect: allow_reconnect.clone(),
            };
            let (incoming, receiver) = mpsc::channel::<Result<DuplexStream, io::Error>>(4);
            let service = tokio::spawn(async move {
                Server::builder()
                    .add_service(SandboxServiceServer::new(host))
                    .serve_with_incoming(ReceiverStream::new(receiver))
                    .await
                    .unwrap();
            });
            let channel = Endpoint::from_static("http://sandbox.local")
                .connect_with_connector(tower::service_fn(move |_| {
                    let incoming = incoming.clone();
                    async move {
                        let (client, server) = tokio::io::duplex(64 * 1024);
                        incoming
                            .send(Ok(server))
                            .await
                            .map_err(|_| io::Error::other("service stopped"))?;
                        Ok::<_, io::Error>(TokioIo::new(client))
                    }
                }))
                .await
                .unwrap();
            let connected = Arc::new(AtomicBool::new(false));
            let stop = CancellationToken::new();
            let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_shutdown(
                channel,
                root.clone(),
                WORKSPACE_ID,
                stop.clone(),
                connected.clone(),
            ));
            let mut first = tokio::time::timeout(STARTUP_TIMEOUT, received.recv())
                .await
                .unwrap()
                .unwrap();
            let WorkerBody::Hello(first_hello) = recv(&mut first).await else {
                panic!("expected first hello");
            };
            let id = "active-shell";
            send(&first, HostBody::Prepare(proto::Prepare {
                invocation_id: id.into(), tool_name: "shell".into(),
                arguments_json: serde_json::json!({
                    "command": "printf ready > started; while [ ! -f release ]; do sleep 0.02; done; printf x >> effects; printf finished > finished",
                    "description": "reconnect test"
                }).to_string(),
                shell_defaults: Some(proto::ShellExecutionDefaults {
                    default_workdir: root.to_string_lossy().into_owned(),
                    default_timeout_seconds: 5, default_timeout_nanos: 0, max_output_bytes: 1024,
                }),
                timeout_millis: 2000,
            })).await;
            let WorkerBody::Prepared(prepared) = recv(&mut first).await else {
                panic!("expected prepared");
            };
            assert!(prepared.error.is_empty(), "{}", prepared.error);
            let execute = proto::Execute {
                invocation_id: id.into(),
                ticket: prepared.ticket,
                timeout_millis: 4000,
            };
            send(&first, HostBody::Execute(execute.clone())).await;
            tokio::time::timeout(STARTUP_TIMEOUT, async {
                while !root.join("started").exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(!root.join("effects").exists());
            drop(first);
            tokio::time::timeout(STARTUP_TIMEOUT, async {
                while connected.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            tokio::fs::write(root.join("release"), "go").await.unwrap();
            tokio::time::timeout(STARTUP_TIMEOUT, async {
                while !root.join("finished").exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            // No host status query and no Execute retry: permit the new Connect only after completion.
            allow_reconnect.notify_one();
            let mut second = tokio::time::timeout(STARTUP_TIMEOUT, received.recv())
                .await
                .unwrap()
                .unwrap();
            let WorkerBody::Hello(second_hello) = recv(&mut second).await else {
                panic!("expected reconnect hello");
            };
            assert_eq!(first_hello.worker_id, second_hello.worker_id);
            let completion = loop {
                if let WorkerBody::Completed(reply) = recv(&mut second).await {
                    break reply;
                }
            };
            assert_eq!(completion.invocation_id, id);
            assert!(!completion.output.as_ref().unwrap().is_error);
            send(&second, HostBody::Execute(execute)).await;
            let duplicate = loop {
                if let WorkerBody::Completed(reply) = recv(&mut second).await {
                    break reply;
                }
            };
            assert_eq!(duplicate, completion);
            assert_eq!(
                tokio::fs::read_to_string(root.join("effects"))
                    .await
                    .unwrap(),
                "x"
            );
            stop.cancel();
            tokio::time::timeout(Duration::from_secs(3), worker)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            service.abort();
            drop(service.await);
            assert!(!connected.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn private_duplex_worker_redials_same_gateway_after_stream_loss() {
        let root = crate::test_support::temp_root();
        tokio::fs::write(root.join("probe"), "reconnected")
            .await
            .unwrap();
        let gateway = SandboxGateway::new(WORKSPACE_ID.to_owned());
        let (initial_sender, initial_receiver) =
            mpsc::channel::<Result<DuplexStream, io::Error>>(4);
        let incoming = Arc::new(Mutex::new(initial_sender));
        let disconnect = Arc::new(Mutex::new(CancellationToken::new()));
        let first_service = tokio::spawn({
            let gateway = gateway.clone();
            async move {
                Server::builder()
                    .add_service(gateway.service())
                    .serve_with_incoming(ReceiverStream::new(initial_receiver))
                    .await
                    .unwrap();
            }
        });
        let channel = Endpoint::from_static("http://sandbox.local")
            .connect_with_connector(tower::service_fn({
                let incoming = incoming.clone();
                let disconnect = disconnect.clone();
                move |_| {
                    let incoming = incoming.clone();
                    let disconnect = disconnect.clone();
                    async move {
                        let disconnect = disconnect.lock().await.clone();
                        let (client, mut relay_in) = tokio::io::duplex(64 * 1024);
                        let (mut relay_out, server) = tokio::io::duplex(64 * 1024);
                        tokio::spawn(async move {
                            tokio::select! {
                                () = disconnect.cancelled() => {}
                                _ = tokio::io::copy_bidirectional(&mut relay_in, &mut relay_out) => {}
                            }
                        });
                        incoming.lock().await.send(Ok(server)).await.map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                "sandbox service stopped",
                            )
                        })?;
                        Ok::<_, io::Error>(TokioIo::new(client))
                    }
                }
            }))
            .await
            .unwrap();
        let connected = Arc::new(AtomicBool::new(false));
        let stop = CancellationToken::new();
        let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_shutdown(
            channel,
            root,
            WORKSPACE_ID,
            stop.clone(),
            connected.clone(),
        ));
        tokio::time::timeout(STARTUP_TIMEOUT, gateway.wait_ready())
            .await
            .unwrap()
            .unwrap();
        assert!(connected.load(Ordering::Acquire));
        let prepared = gateway
            .handle()
            .prepare(
                "write",
                &serde_json::json!({"filePath":"written", "content":"ticket survives reconnect"}),
            )
            .await
            .unwrap();
        disconnect.lock().await.cancel();
        first_service.abort();
        drop(first_service.await);
        tokio::time::timeout(STARTUP_TIMEOUT, async {
            while connected.load(Ordering::Acquire) || gateway.handle().canonical_root().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let queued = tokio::spawn({
            let handle = gateway.handle();
            async move { handle.read_file("probe").await }
        });
        tokio::task::yield_now().await;
        assert!(!queued.is_finished());
        let (next_sender, next_receiver) = mpsc::channel(4);
        *disconnect.lock().await = CancellationToken::new();
        *incoming.lock().await = next_sender;
        let second_service = tokio::spawn({
            let gateway = gateway.clone();
            async move {
                Server::builder()
                    .add_service(gateway.service())
                    .serve_with_incoming(ReceiverStream::new(next_receiver))
                    .await
                    .unwrap();
            }
        });
        tokio::time::timeout(STARTUP_TIMEOUT, gateway.wait_ready())
            .await
            .unwrap()
            .unwrap();
        assert!(connected.load(Ordering::Acquire));
        let result = gateway
            .handle()
            .execute(
                prepared,
                CancellationToken::new(),
                waltr_agent::ToolProgressSink::noop(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            tokio::time::timeout(STARTUP_TIMEOUT, queued)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .content,
            "reconnected"
        );
        assert_eq!(
            gateway.handle().read_file("probe").await.unwrap().content,
            "reconnected"
        );
        // A stream reconnect must not create a replacement worker with an empty ticket table.
        assert_eq!(
            gateway.handle().read_file("written").await.unwrap().content,
            "ticket survives reconnect"
        );
        gateway.shutdown();
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        second_service.abort();
        drop(second_service.await);
        assert!(!connected.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn worker_uses_private_transport_and_joins_on_shutdown() {
        let root = crate::test_support::temp_root();
        tokio::fs::write(root.join("probe.txt"), "local worker")
            .await
            .unwrap();
        let sandbox = LocalSandbox::start(root).await.unwrap();
        assert!(sandbox.health().load(Ordering::Acquire));
        let info = sandbox.handle().workspace_info().await.unwrap();
        assert_eq!(info.path_kind, "directory");
        assert_eq!(info.canonical_path, info.canonical_root);
        assert_eq!(
            sandbox.handle().canonical_root().unwrap().to_string_lossy(),
            info.canonical_root
        );
        let data = sandbox.handle().read_file("probe.txt").await.unwrap();
        assert!(data.content.contains("local worker"));
        let resolved = sandbox.handle().resolve("probe.txt").await.unwrap();
        assert_eq!(resolved.path_kind, "file");
        assert_eq!(
            resolved.canonical_path,
            format!("{}/probe.txt", info.canonical_root)
        );
        let health = sandbox.health();
        let handle = sandbox.handle();
        sandbox.shutdown().await;
        assert!(!health.load(Ordering::Acquire));
        assert!(handle.read_file("probe.txt").await.is_err());
    }

    #[tokio::test]
    async fn socket_worker_executes_and_disconnects_without_host_fallback() {
        let root = crate::test_support::temp_root();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let socket = root.join("sandbox.sock");
        let channel = Endpoint::from_static("http://sandbox.local").connect_with_connector_lazy(
            tower::service_fn({
                let socket = socket.clone();
                move |_| {
                    let socket = socket.clone();
                    async move { UnixStream::connect(socket).await.map(TokioIo::new) }
                }
            }),
        );
        let stop = CancellationToken::new();
        let worker = tokio::spawn(waltr_agent_sandbox::run_worker_with_shutdown(
            channel,
            workspace.clone(),
            WORKSPACE_ID,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
        ));
        let sandbox = SocketSandbox::start(workspace.clone(), socket.clone())
            .await
            .unwrap();
        assert!(sandbox.alive.load(Ordering::Acquire));
        let prepared = sandbox
            .gateway
            .handle()
            .prepare(
                "write",
                &serde_json::json!({"filePath":"result.txt", "content":"worker wrote this"}),
            )
            .await
            .unwrap();
        let output = sandbox
            .gateway
            .handle()
            .execute(
                prepared,
                CancellationToken::new(),
                waltr_agent::ToolProgressSink::noop(),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(
            sandbox
                .gateway
                .handle()
                .read_file("result.txt")
                .await
                .unwrap()
                .content,
            "worker wrote this"
        );
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while sandbox.alive.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(sandbox.gateway.handle().canonical_root().is_none());
        sandbox.shutdown().await;
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn socket_rejects_symlinks_without_deleting_them() {
        let root = crate::test_support::temp_root();
        let socket = root.join("sandbox.sock");
        std::os::unix::fs::symlink(root.join("target"), &socket).unwrap();
        assert!(
            SocketSandbox::start(root.clone(), socket.clone())
                .await
                .is_err()
        );
        assert!(socket.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[tokio::test]
    async fn unavailable_worker_root_fails_startup() {
        let root = crate::test_support::temp_root().join("missing");
        assert!(LocalSandbox::start(root).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_scoped_reads_reject_symlinks_outside_workspace() {
        use std::os::unix::fs::symlink;

        let root = crate::test_support::temp_root();
        let workspace = root.join("workspace");
        let host = root.join("host");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&host).unwrap();
        std::fs::write(host.join("secret.txt"), "host secret").unwrap();
        symlink(&host, workspace.join("host-alias")).unwrap();

        let sandbox = LocalSandbox::start(workspace).await.unwrap();
        assert!(
            sandbox
                .handle()
                .read_file("host-alias/secret.txt")
                .await
                .is_err()
        );
        sandbox.shutdown().await;
    }
}
