//! Private, single-worker sandbox RPC boundary. No host-side tool execution occurs here.

use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use waltr::grpc::waltr::sandbox::v1::{
    self as proto,
    connect_request::Body as WorkerBody,
    connect_response::Body as HostBody,
    sandbox_service_server::{SandboxService, SandboxServiceServer},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout_at};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{LlmError, Result, ToolProgressSink};

const MAX_PENDING: usize = 128;
const OUTBOUND_CAPACITY: usize = 128;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
const WORKSPACE_TIMEOUT: Duration = Duration::from_secs(30);
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(60);
const DELIVERY_GRACE: Duration = Duration::from_secs(15);
const MAX_SHELL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

fn error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "sandbox",
        message: message.into(),
    }
}

fn message(body: HostBody) -> proto::ConnectResponse {
    proto::ConnectResponse { body: Some(body) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerId {
    id: String,
    generation: u64,
}

/// A prepared invocation tied to the worker that issued its ticket.
#[derive(Debug)]
pub struct PreparedSandboxCall {
    /// Worker-supplied permission claims. Interpret these before executing.
    pub claims: Vec<proto::Claim>,
    invocation_id: String,
    ticket: String,
    worker: WorkerId,
    execution_timeout: Duration,
    cancel_on_drop: Option<SandboxHandle>,
}

impl PreparedSandboxCall {
    /// Unique ID of this worker invocation.
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    /// Cancel an unused ticket on its worker, including after a stream reconnect.
    pub fn cancel(mut self, _handle: &SandboxHandle) {
        self.cancel_unused();
    }

    fn cancel_unused(&mut self) {
        if let Some(handle) = self.cancel_on_drop.take() {
            handle.cancel(&self.invocation_id, self.worker.clone());
        }
    }
}

impl Drop for PreparedSandboxCall {
    fn drop(&mut self) {
        self.cancel_unused();
    }
}

/// Final worker tool output. A tool-level error is distinct from a transport failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutput {
    /// Text returned by the tool.
    pub content: String,
    /// Whether the tool reported a domain-level failure.
    pub is_error: bool,
}

/// Workspace read or search data returned by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceData {
    /// File content or search output.
    pub content: String,
    /// Matching paths for listing operations.
    pub entries: Vec<String>,
    /// Canonical worker-local path for info/resolve.
    pub canonical_path: String,
    /// Worker-reported path type.
    pub path_kind: String,
    /// Canonical worker-local workspace root.
    pub canonical_root: String,
}

#[derive(Debug, Clone, Copy)]
struct SnapshotBounds {
    max_entries: usize,
    max_depth: usize,
    max_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
enum WorkspaceOperation<'a> {
    Info,
    Resolve {
        path: &'a str,
    },
    Read {
        path: &'a str,
    },
    FormattedRead {
        path: &'a str,
    },
    Glob {
        path: &'a str,
        pattern: &'a str,
    },
    SearchFiles {
        query: &'a str,
        limit: usize,
    },
    Snapshot {
        path: &'a str,
        bounds: SnapshotBounds,
    },
}

impl WorkspaceOperation<'_> {
    fn to_proto(self, request_id: String) -> proto::WorkspaceRequest {
        let mut request = proto::WorkspaceRequest {
            request_id,
            ..Default::default()
        };
        match self {
            Self::Info => {
                request.operation = "info".into();
            }
            Self::Resolve { path } => {
                request.operation = "resolve".into();
                request.path = path.into();
            }
            Self::Read { path } => {
                request.operation = "read".into();
                request.path = path.into();
            }
            Self::FormattedRead { path } => {
                request.operation = "formatted_read".into();
                request.path = path.into();
            }
            Self::Glob { path, pattern } => {
                request.operation = "glob".into();
                request.path = path.into();
                request.pattern = pattern.into();
            }
            Self::SearchFiles { query, limit } => {
                request.operation = "search_files".into();
                request.path = ".".into();
                request.pattern = query.into();
                request.max_entries = capped_u32(limit);
            }
            Self::Snapshot { path, bounds } => {
                request.operation = "snapshot".into();
                request.path = path.into();
                request.max_entries = capped_u32(bounds.max_entries);
                request.max_depth = capped_u32(bounds.max_depth);
                request.max_bytes = capped_u32(bounds.max_bytes);
            }
        }
        request
    }
}

fn capped_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn shell_execution_timeout(
    arguments: &serde_json::Value,
    defaults: Option<&proto::ShellExecutionDefaults>,
) -> Result<Duration> {
    let default = defaults.map_or(Duration::from_secs(30), |defaults| {
        Duration::new(
            defaults.default_timeout_seconds,
            defaults.default_timeout_nanos,
        )
    });
    let command = arguments
        .get("timeout")
        .and_then(serde_json::Value::as_u64)
        .map_or(default, Duration::from_secs);
    if command > MAX_SHELL_TIMEOUT {
        return Err(error("shell timeout exceeds gateway limit"));
    }
    Ok(command + DELIVERY_GRACE)
}

async fn within<T>(
    deadline: Instant,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    timeout_at(deadline, future)
        .await
        .map_err(|_| error("sandbox request timed out"))?
}

enum Pending {
    Prepare(oneshot::Sender<proto::Prepared>),
    Execute {
        reply: oneshot::Sender<proto::Completed>,
        progress: ToolProgressSink,
        sequence: u64,
    },
    Workspace(oneshot::Sender<proto::WorkspaceResponse>),
}

struct Connection {
    id: u64,
    worker: WorkerId,
    root: PathBuf,
    outbound: mpsc::Sender<std::result::Result<proto::ConnectResponse, Status>>,
}

struct PendingRequest {
    response: Pending,
    worker: Option<WorkerId>,
    sent: bool,
}

#[derive(Default)]
struct State {
    connection: Option<Connection>,
    next_connection: u64,
    pending: HashMap<String, PendingRequest>,
    shutdown: bool,
}

struct Inner {
    workspace_id: String,
    expected_generation: Option<u64>,
    expected_root: Option<PathBuf>,
    state: Mutex<State>,
    ready: watch::Sender<bool>,
    stopped: CancellationToken,
}

/// Host endpoint for one workspace and at most one active worker.
#[derive(Clone)]
pub struct SandboxGateway {
    inner: Arc<Inner>,
}

/// Cloneable request client for the current sandbox connection.
#[derive(Clone)]
pub struct SandboxHandle {
    inner: Arc<Inner>,
}

impl fmt::Debug for SandboxGateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxGateway")
            .field("workspace_id", &self.inner.workspace_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SandboxHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxHandle").finish_non_exhaustive()
    }
}

impl SandboxGateway {
    /// Create a gateway bound to one workspace (any worker generation by default).
    pub fn new(workspace_id: String) -> Self {
        let (ready, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                workspace_id,
                expected_generation: None,
                expected_root: None,
                state: Mutex::new(State::default()),
                ready,
                stopped: CancellationToken::new(),
            }),
        }
    }

    /// Require a particular worker generation in the initial hello.
    pub fn with_expected_generation(mut self, generation: u64) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("configure generation before cloning")
            .expected_generation = Some(generation);
        self
    }

    /// Require a worker-reported root matching the host's configured workspace path.
    pub fn with_expected_root(mut self, root: PathBuf) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("configure root before cloning")
            .expected_root = Some(root);
        self
    }

    /// Observe live worker connectivity, including reconnects and shutdown.
    pub fn readiness(&self) -> watch::Receiver<bool> {
        self.inner.ready.subscribe()
    }

    /// Return the private tonic service to mount on a trusted listener.
    pub fn service(&self) -> SandboxServiceServer<Self> {
        SandboxServiceServer::new(self.clone())
    }

    /// Wait for a connected worker, or fail if the gateway is shut down.
    pub async fn wait_ready(&self) -> Result<()> {
        let mut ready = self.inner.ready.subscribe();
        loop {
            if self.inner.stopped.is_cancelled() {
                return Err(error("gateway shut down"));
            }
            if *ready.borrow_and_update() {
                return Ok(());
            }
            tokio::select! {
                changed = ready.changed() => if changed.is_err() { return Err(error("gateway shut down")); },
                () = self.inner.stopped.cancelled() => return Err(error("gateway shut down")),
            }
        }
    }

    /// Return a cloneable request handle.
    pub fn handle(&self) -> SandboxHandle {
        SandboxHandle {
            inner: self.inner.clone(),
        }
    }

    /// Close the active stream, reject new connections and fail pending requests.
    pub fn shutdown(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.shutdown = true;
        state.connection = None;
        state.pending.clear();
        self.inner.ready.send_replace(false);
        self.inner.stopped.cancel();
    }
}

// Removing a pending entry on dropped futures prevents orphaned requests from exhausting the cap.
struct PendingGuard {
    inner: Arc<Inner>,
    id: String,
    cancel: bool,
}

struct ConnectionGuard {
    inner: Arc<Inner>,
    epoch: u64,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.inner.disconnect(self.epoch);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(request) = state.pending.remove(&self.id)
            && request.sent
            && self.cancel
            && let Some(worker) = request.worker
        {
            drop(state);
            SandboxHandle {
                inner: self.inner.clone(),
            }
            .cancel(&self.id, worker);
        }
    }
}

impl SandboxHandle {
    /// Worker-reported canonical root for the current connection, if available.
    pub fn canonical_root(&self) -> Option<PathBuf> {
        self.inner
            .state
            .lock()
            .unwrap()
            .connection
            .as_ref()
            .map(|c| c.root.clone())
    }

    async fn send(
        &self,
        id: String,
        pending: Pending,
        body: HostBody,
        expected: Option<WorkerId>,
        cancel: bool,
    ) -> Result<(WorkerId, PendingGuard)> {
        let mut changed = self.inner.ready.subscribe();
        {
            let mut state = self.inner.state.lock().unwrap();
            if state.shutdown {
                return Err(error("gateway shut down"));
            }
            if state.pending.len() >= MAX_PENDING {
                return Err(error("too many pending sandbox requests"));
            }
            if state.pending.contains_key(&id) {
                return Err(error("duplicate sandbox request id"));
            }
            state.pending.insert(
                id.clone(),
                PendingRequest {
                    response: pending,
                    worker: expected.clone(),
                    sent: false,
                },
            );
        }
        let guard = PendingGuard {
            inner: self.inner.clone(),
            id: id.clone(),
            cancel,
        };
        loop {
            // Subscribe before inspecting state, so a connection change cannot be missed.
            drop(changed.borrow_and_update());
            let connection = {
                let state = self.inner.state.lock().unwrap();
                if state.shutdown {
                    return Err(error("gateway shut down"));
                }
                state
                    .connection
                    .as_ref()
                    .map(|c| (c.id, c.worker.clone(), c.outbound.clone()))
            };
            if let Some((epoch, worker, outbound)) = connection {
                if expected
                    .as_ref()
                    .is_some_and(|expected| expected != &worker)
                {
                    return Err(error("stale sandbox ticket"));
                }
                let permit = tokio::select! {
                    permit = outbound.reserve() => permit,
                    () = self.inner.stopped.cancelled() => return Err(error("gateway shut down")),
                    _ = changed.changed() => continue,
                };
                if let Ok(permit) = permit {
                    let mut state = self.inner.state.lock().unwrap();
                    if state.shutdown {
                        return Err(error("gateway shut down"));
                    }
                    if state.connection.as_ref().is_some_and(|c| c.id == epoch) {
                        let request = state
                            .pending
                            .get_mut(&id)
                            .expect("guard holds pending request");
                        request.worker = Some(worker.clone());
                        request.sent = true;
                        permit.send(Ok(message(body)));
                        return Ok((worker, guard));
                    }
                }
            }
            tokio::select! {
                _ = changed.changed() => {},
                () = self.inner.stopped.cancelled() => return Err(error("gateway shut down")),
            }
        }
    }

    fn cancel(&self, id: &str, worker: WorkerId) {
        {
            let state = self.inner.state.lock().unwrap();
            if let Some(connection) = &state.connection {
                if connection.worker != worker {
                    return;
                }
                let cancel = Ok(message(HostBody::Cancel(proto::Cancel {
                    invocation_id: id.to_owned(),
                })));
                match connection.outbound.try_send(cancel) {
                    Ok(()) => return,
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                }
            }
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let handle = self.clone();
            let id = id.to_owned();
            runtime.spawn(async move {
                let _ = timeout_at(Instant::now() + PREPARE_TIMEOUT, async {
                    let mut changed = handle.inner.ready.subscribe();
                    loop {
                        drop(changed.borrow_and_update());
                        let connection = {
                            let state = handle.inner.state.lock().unwrap();
                            if state.shutdown {
                                return;
                            }
                            state
                                .connection
                                .as_ref()
                                .map(|c| (c.id, c.worker.clone(), c.outbound.clone()))
                        };
                        if let Some((epoch, current, outbound)) = connection {
                            if current != worker {
                                return;
                            }
                            let reserved = tokio::select! {
                                reserved = outbound.reserve() => reserved,
                                _ = changed.changed() => continue,
                                () = handle.inner.stopped.cancelled() => return,
                            };
                            if let Ok(permit) = reserved {
                                let state = handle.inner.state.lock().unwrap();
                                if state.connection.as_ref().is_some_and(|c| c.id == epoch) {
                                    permit.send(Ok(message(HostBody::Cancel(proto::Cancel {
                                        invocation_id: id,
                                    }))));
                                    return;
                                }
                            }
                        }
                        tokio::select! {
                            _ = changed.changed() => {},
                            () = handle.inner.stopped.cancelled() => return,
                        }
                    }
                })
                .await;
            });
        }
    }

    /// Request worker claims and an opaque execution ticket.
    pub async fn prepare(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<PreparedSandboxCall> {
        self.prepare_with_shell_defaults(tool_name, arguments, None)
            .await
    }

    pub(crate) async fn prepare_with_shell_defaults(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
        shell_defaults: Option<proto::ShellExecutionDefaults>,
    ) -> Result<PreparedSandboxCall> {
        let id = Uuid::now_v7().to_string();
        let execution_timeout = if tool_name == "shell" {
            shell_execution_timeout(arguments, shell_defaults.as_ref())?
        } else {
            EXECUTE_TIMEOUT
        };
        let deadline = Instant::now() + PREPARE_TIMEOUT;
        let (tx, rx) = oneshot::channel();
        let (worker, _guard) = within(
            deadline,
            self.send(
                id.clone(),
                Pending::Prepare(tx),
                HostBody::Prepare(proto::Prepare {
                    invocation_id: id.clone(),
                    tool_name: tool_name.to_owned(),
                    arguments_json: serde_json::to_string(arguments)?,
                    shell_defaults,
                    timeout_millis: millis(PREPARE_TIMEOUT - DELIVERY_GRACE),
                }),
                None,
                true,
            ),
        )
        .await?;
        let prepared = within(deadline, async {
            rx.await
                .map_err(|_| error("gateway shut down during prepare"))
        })
        .await?;
        if !prepared.error.is_empty() {
            return Err(error(prepared.error));
        }
        Ok(PreparedSandboxCall {
            claims: prepared.claims,
            invocation_id: id,
            ticket: prepared.ticket,
            worker,
            execution_timeout,
            cancel_on_drop: Some(self.clone()),
        })
    }

    /// Execute an approved ticket, forwarding correlated progress to the sink.
    pub async fn execute(
        &self,
        mut prepared: PreparedSandboxCall,
        cancellation: CancellationToken,
        progress: ToolProgressSink,
    ) -> Result<SandboxOutput> {
        if cancellation.is_cancelled() {
            return Err(error("sandbox invocation cancelled"));
        }
        let deadline = Instant::now() + prepared.execution_timeout;
        let (tx, rx) = oneshot::channel();
        let (_, _guard) = tokio::select! {
            result = within(deadline, self.send(
            prepared.invocation_id.clone(),
            Pending::Execute {
                reply: tx,
                progress,
                sequence: 0,
            },
            HostBody::Execute(proto::Execute {
                invocation_id: prepared.invocation_id.clone(),
                ticket: prepared.ticket.clone(),
                timeout_millis: millis(prepared.execution_timeout.saturating_sub(Duration::from_secs(10))),
            }),
            Some(prepared.worker.clone()),
            true,
            )) => result?,
            () = cancellation.cancelled() => return Err(error("sandbox invocation cancelled")),
        };
        prepared.cancel_on_drop = None;
        let completed = tokio::select! {
            result = within(deadline, async { rx.await.map_err(|_| error("gateway shut down during execute")) }) => result?,
            () = cancellation.cancelled() => return Err(error("sandbox invocation cancelled")),
        };
        if !completed.error.is_empty() {
            return Err(error(completed.error));
        }
        let output = completed
            .output
            .ok_or_else(|| error("missing sandbox tool output"))?;
        Ok(SandboxOutput {
            content: output.content,
            is_error: output.is_error,
        })
    }

    /// Return the worker's canonical workspace root and its path kind.
    pub async fn workspace_info(&self) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::Info).await
    }

    /// Resolve a worker-local path and return its canonical path and kind.
    pub async fn resolve(&self, path: &str) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::Resolve { path })
            .await
    }

    /// Read the raw contents of a workspace file.
    pub async fn read_file(&self, path: &str) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::Read { path })
            .await
    }

    /// Read a workspace file using the worker's tool formatting.
    pub async fn formatted_read(&self, path: &str) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::FormattedRead { path })
            .await
    }

    /// Find matching workspace files under a path.
    pub async fn glob(&self, path: &str, pattern: &str) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::Glob { path, pattern })
            .await
    }

    /// Search matching file paths with worker-side ranking and a bounded reply.
    pub async fn search_files(&self, query: &str, limit: usize) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::SearchFiles { query, limit })
            .await
    }

    /// Request a worker-bounded, root-relative workspace snapshot.
    pub async fn snapshot(
        &self,
        path: &str,
        max_entries: usize,
        max_depth: usize,
        max_bytes: usize,
    ) -> Result<WorkspaceData> {
        self.workspace_request(WorkspaceOperation::Snapshot {
            path,
            bounds: SnapshotBounds {
                max_entries,
                max_depth,
                max_bytes,
            },
        })
        .await
    }

    async fn workspace_request(&self, operation: WorkspaceOperation<'_>) -> Result<WorkspaceData> {
        let id = Uuid::now_v7().to_string();
        let deadline = Instant::now() + WORKSPACE_TIMEOUT;
        let (tx, rx) = oneshot::channel();
        let mut request = operation.to_proto(id.clone());
        request.timeout_millis = millis(WORKSPACE_TIMEOUT - DELIVERY_GRACE);
        let (_, _guard) = within(
            deadline,
            self.send(
                id.clone(),
                Pending::Workspace(tx),
                HostBody::WorkspaceRequest(request),
                None,
                false,
            ),
        )
        .await?;
        let result = within(deadline, async {
            rx.await
                .map_err(|_| error("gateway shut down during workspace request"))
        })
        .await?;
        if !result.error.is_empty() {
            return Err(error(result.error));
        }
        Ok(WorkspaceData {
            content: result.content,
            entries: result.entries,
            canonical_path: result.canonical_path,
            path_kind: result.path_kind,
            canonical_root: result.canonical_root,
        })
    }
}

impl Inner {
    fn disconnect(&self, epoch: u64) {
        let mut state = self.state.lock().unwrap();
        if state.connection.as_ref().is_some_and(|c| c.id == epoch) {
            state.connection = None;
            self.ready.send_replace(false);
        }
    }

    fn dispatch(&self, epoch: u64, body: WorkerBody) -> std::result::Result<(), Status> {
        let mut state = self.state.lock().unwrap();
        if state.connection.as_ref().is_none_or(|c| c.id != epoch) {
            return Err(Status::unavailable("worker disconnected"));
        }
        match body {
            WorkerBody::Prepared(reply) => {
                if matches!(
                    state.pending.get(&reply.invocation_id),
                    Some(PendingRequest { response: Pending::Prepare(_), sent: true, worker: Some(worker) })
                        if state.connection.as_ref().is_some_and(|c| &c.worker == worker)
                ) {
                    if let Some(PendingRequest {
                        response: Pending::Prepare(tx),
                        ..
                    }) = state.pending.remove(&reply.invocation_id)
                    {
                        drop(tx.send(reply));
                    }
                }
            }
            WorkerBody::Completed(reply) => {
                if matches!(
                    state.pending.get(&reply.invocation_id),
                    Some(PendingRequest { response: Pending::Execute { .. }, sent: true, worker: Some(worker) })
                        if state.connection.as_ref().is_some_and(|c| &c.worker == worker)
                ) {
                    if let Some(PendingRequest {
                        response: Pending::Execute { reply: tx, .. },
                        ..
                    }) = state.pending.remove(&reply.invocation_id)
                    {
                        drop(tx.send(reply));
                    }
                }
            }
            WorkerBody::WorkspaceResponse(reply) => {
                if matches!(
                    state.pending.get(&reply.request_id),
                    Some(PendingRequest { response: Pending::Workspace(_), sent: true, worker: Some(worker) })
                        if state.connection.as_ref().is_some_and(|c| &c.worker == worker)
                ) {
                    if let Some(PendingRequest {
                        response: Pending::Workspace(tx),
                        ..
                    }) = state.pending.remove(&reply.request_id)
                    {
                        drop(tx.send(reply));
                    }
                }
            }
            WorkerBody::Progress(event) => {
                let worker = state.connection.as_ref().map(|c| c.worker.clone());
                let progress = if let Some(PendingRequest {
                    response:
                        Pending::Execute {
                            progress, sequence, ..
                        },
                    sent: true,
                    worker: Some(expected),
                }) = state.pending.get_mut(&event.invocation_id)
                    && worker.as_ref() == Some(expected)
                {
                    if event.sequence > *sequence {
                        *sequence = event.sequence;
                        Some(progress.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };
                drop(state);
                if let Some(progress) = progress {
                    drop(if event.level == "error" {
                        progress.error(event.text)
                    } else {
                        progress.info(event.text)
                    });
                }
                return Ok(());
            }
            WorkerBody::Hello(_) => return Err(Status::invalid_argument("unexpected hello")),
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl SandboxService for SandboxGateway {
    type ConnectStream =
        Pin<Box<dyn Stream<Item = std::result::Result<proto::ConnectResponse, Status>> + Send>>;

    async fn connect(
        &self,
        request: Request<tonic::Streaming<proto::ConnectRequest>>,
    ) -> std::result::Result<Response<Self::ConnectStream>, Status> {
        let mut incoming = request.into_inner();
        let first = tokio::select! {
            message = incoming.message() => message?,
            () = self.inner.stopped.cancelled() => return Err(Status::unavailable("gateway shut down")),
        }.ok_or_else(|| Status::invalid_argument("missing hello"))?;
        let Some(WorkerBody::Hello(hello)) = first.body else {
            return Err(Status::invalid_argument("first message must be hello"));
        };
        if hello.worker_id.is_empty()
            || hello.workspace_id != self.inner.workspace_id
            || self
                .inner
                .expected_generation
                .is_some_and(|generation| generation != hello.generation)
        {
            return Err(Status::permission_denied(
                "sandbox worker identity mismatch",
            ));
        }
        if !std::path::Path::new(&hello.canonical_root).is_absolute() {
            return Err(Status::invalid_argument("worker root must be absolute"));
        }
        if self
            .inner
            .expected_root
            .as_ref()
            .is_some_and(|root| root != std::path::Path::new(&hello.canonical_root))
        {
            return Err(Status::permission_denied("sandbox worker root mismatch"));
        }
        let (outbound, receiver) = mpsc::channel(OUTBOUND_CAPACITY);
        let epoch = {
            let mut state = self.inner.state.lock().unwrap();
            if state.shutdown {
                return Err(Status::unavailable("gateway shut down"));
            }
            if state.connection.is_some() {
                return Err(Status::already_exists("worker already connected"));
            }
            state.next_connection += 1;
            let epoch = state.next_connection;
            state.connection = Some(Connection {
                id: epoch,
                worker: WorkerId {
                    id: hello.worker_id,
                    generation: hello.generation,
                },
                root: PathBuf::from(hello.canonical_root),
                outbound: outbound.clone(),
            });
            self.inner.ready.send_replace(true);
            epoch
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _connection_guard = ConnectionGuard {
                inner: inner.clone(),
                epoch,
            };
            loop {
                let received = tokio::select! {
                    () = inner.stopped.cancelled() => break,
                    () = outbound.closed() => break,
                    message = incoming.message() => message,
                };
                match received {
                    Ok(Some(proto::ConnectRequest { body: Some(body) })) => {
                        if inner.dispatch(epoch, body).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr::grpc::waltr::sandbox::v1::sandbox_service_client::SandboxServiceClient;
    use std::collections::HashSet;
    use tokio::{
        net::TcpListener,
        time::{Duration, timeout},
    };
    use tokio_stream::wrappers::TcpListenerStream;

    async fn server(
        gateway: SandboxGateway,
    ) -> (
        SandboxServiceClient<tonic::transport::Channel>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(gateway.service())
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let client = SandboxServiceClient::new(channel);
        (client, task)
    }

    fn hello(workspace: &str, generation: u64) -> proto::ConnectRequest {
        proto::ConnectRequest {
            body: Some(WorkerBody::Hello(proto::Hello {
                worker_id: "worker".into(),
                workspace_id: workspace.into(),
                generation,
                tools: vec![],
                canonical_root: "/worker/root".into(),
            })),
        }
    }

    async fn connected(
        client: &mut SandboxServiceClient<tonic::transport::Channel>,
    ) -> (
        mpsc::Sender<proto::ConnectRequest>,
        tonic::Streaming<proto::ConnectResponse>,
    ) {
        let (tx, rx) = mpsc::channel(16);
        tx.send(hello("workspace", 3)).await.unwrap();
        let stream = client
            .connect(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        (tx, stream)
    }

    #[tokio::test]
    async fn configured_root_rejects_a_worker_from_another_workspace() {
        let gateway = SandboxGateway::new("workspace".into())
            .with_expected_root(PathBuf::from("/configured/workspace"));
        let (mut client, task) = server(gateway.clone()).await;
        let (tx, rx) = mpsc::channel(1);
        tx.send(hello("workspace", 3)).await.unwrap();
        let error = client.connect(ReceiverStream::new(rx)).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::PermissionDenied);
        assert!(gateway.handle().canonical_root().is_none());
        gateway.shutdown();
        task.abort();
        drop(task.await);
    }

    #[test]
    fn workspace_operations_encode_only_their_named_fields() {
        let cases = [
            (WorkspaceOperation::Info, "info", "", ""),
            (
                WorkspaceOperation::Resolve { path: "src" },
                "resolve",
                "src",
                "",
            ),
            (
                WorkspaceOperation::Read { path: "a.rs" },
                "read",
                "a.rs",
                "",
            ),
            (
                WorkspaceOperation::FormattedRead { path: "a.rs" },
                "formatted_read",
                "a.rs",
                "",
            ),
            (
                WorkspaceOperation::Glob {
                    path: "src",
                    pattern: "**/*.rs",
                },
                "glob",
                "src",
                "**/*.rs",
            ),
        ];
        for (operation, name, path, pattern) in cases {
            let request = operation.to_proto("id".into());
            assert_eq!(request.request_id, "id");
            assert_eq!(
                (
                    request.operation.as_str(),
                    request.path.as_str(),
                    request.pattern.as_str(),
                ),
                (name, path, pattern)
            );
            assert!(request.include.is_empty());
            assert_eq!(
                (request.max_entries, request.max_depth, request.max_bytes),
                (0, 0, 0)
            );
        }
        let search = WorkspaceOperation::SearchFiles {
            query: "main",
            limit: usize::MAX,
        }
        .to_proto("search".into());
        assert_eq!(
            (
                search.operation.as_str(),
                search.path.as_str(),
                search.pattern.as_str()
            ),
            ("search_files", ".", "main")
        );
        assert_eq!(
            (search.max_entries, search.max_depth, search.max_bytes),
            (u32::MAX, 0, 0)
        );
        let snapshot = WorkspaceOperation::Snapshot {
            path: "src",
            bounds: SnapshotBounds {
                max_entries: 0,
                max_depth: 4,
                max_bytes: usize::MAX,
            },
        }
        .to_proto("snapshot".into());
        assert_eq!(
            (snapshot.operation.as_str(), snapshot.path.as_str()),
            ("snapshot", "src")
        );
        assert_eq!(
            (snapshot.max_entries, snapshot.max_depth, snapshot.max_bytes),
            (0, 4, u32::MAX)
        );
        assert!(snapshot.pattern.is_empty() && snapshot.include.is_empty());
    }

    #[test]
    fn shell_budget_exceeds_command_timeout() {
        let defaults = proto::ShellExecutionDefaults {
            default_timeout_seconds: 70,
            ..Default::default()
        };
        assert_eq!(
            shell_execution_timeout(&serde_json::json!({}), Some(&defaults)).unwrap(),
            Duration::from_secs(85)
        );
        assert_eq!(
            shell_execution_timeout(&serde_json::json!({"timeout": 120}), Some(&defaults)).unwrap(),
            Duration::from_secs(135)
        );
        assert!(
            shell_execution_timeout(&serde_json::json!({"timeout": 86401}), Some(&defaults))
                .is_err()
        );
    }

    #[tokio::test]
    async fn correlates_concurrent_requests_progress_and_workspace() {
        let gateway = SandboxGateway::new("workspace".into()).with_expected_generation(3);
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        gateway.wait_ready().await.unwrap();
        assert_eq!(
            gateway.handle().canonical_root(),
            Some(PathBuf::from("/worker/root"))
        );

        let handle = gateway.handle();
        let first = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .prepare("read", &serde_json::json!({"filePath": "a"}))
                    .await
            }
        });
        let second = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .prepare("read", &serde_json::json!({"filePath": "b"}))
                    .await
            }
        });
        let one = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::Prepare(p) => p,
            _ => panic!("expected prepare"),
        };
        let two = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::Prepare(p) => p,
            _ => panic!("expected prepare"),
        };
        assert_ne!(one.invocation_id, two.invocation_id);
        assert_eq!(one.timeout_millis, millis(PREPARE_TIMEOUT - DELIVERY_GRACE));
        for request in [&two, &one] {
            tx.send(proto::ConnectRequest {
                body: Some(WorkerBody::Prepared(proto::Prepared {
                    invocation_id: request.invocation_id.clone(),
                    ticket: request.arguments_json.clone(),
                    claims: vec![proto::Claim {
                        operation: "read".into(),
                        resource_kind: "path".into(),
                        resource: "a".into(),
                    }],
                    error: String::new(),
                })),
            })
            .await
            .unwrap();
        }
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(first.claims.len(), 1);
        assert_eq!(second.claims.len(), 1);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = ToolProgressSink::timeline(
            "session",
            1,
            crate::TimelineItemId::from_first_cursor(1.into()),
            {
                let seen = seen.clone();
                move |event| {
                    seen.lock().unwrap().push(event);
                    Ok(())
                }
            },
        );
        let executing = tokio::spawn({
            let handle = handle.clone();
            async move { handle.execute(first, CancellationToken::new(), sink).await }
        });
        let request = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::Execute(e) => e,
            _ => panic!("expected execute"),
        };
        assert_eq!(
            request.timeout_millis,
            millis(EXECUTE_TIMEOUT - Duration::from_secs(10))
        );
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Progress(proto::Progress {
                invocation_id: request.invocation_id.clone(),
                sequence: 1,
                level: "info".into(),
                text: "working".into(),
            })),
        })
        .await
        .unwrap();
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Completed(proto::Completed {
                invocation_id: request.invocation_id,
                output: Some(proto::ToolOutput {
                    content: "done".into(),
                    is_error: true,
                }),
                error: String::new(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert_eq!(
            executing.await.unwrap().unwrap(),
            SandboxOutput {
                content: "done".into(),
                is_error: true
            }
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
        second.cancel(&handle);
        assert!(matches!(
            stream.message().await.unwrap().unwrap().body,
            Some(HostBody::Cancel(_))
        ));

        let workspace = tokio::spawn({
            let handle = handle.clone();
            async move { handle.glob(".", "*.rs").await }
        });
        let request = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::WorkspaceRequest(r) => r,
            _ => panic!("expected workspace"),
        };
        assert_eq!(request.pattern, "*.rs");
        assert_eq!(
            request.timeout_millis,
            millis(WORKSPACE_TIMEOUT - DELIVERY_GRACE)
        );
        assert_eq!(request.operation, "glob");
        assert!(request.include.is_empty());
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: request.request_id,
                content: "ok".into(),
                entries: vec!["a.rs".into()],
                error: String::new(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert_eq!(
            workspace.await.unwrap().unwrap(),
            WorkspaceData {
                content: "ok".into(),
                entries: vec!["a.rs".into()],
                canonical_path: String::new(),
                path_kind: String::new(),
                canonical_root: String::new(),
            }
        );
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn snapshot_passes_explicit_bounds_without_using_glob() {
        let gateway = SandboxGateway::new("workspace".into());
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let pending = tokio::spawn(async move { handle.snapshot("src", 0, 4, 40).await });
        let request = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::WorkspaceRequest(request) => request,
            _ => panic!("expected snapshot request"),
        };
        assert_eq!(request.operation, "snapshot");
        assert_eq!(request.path, "src");
        assert_eq!(
            (request.max_entries, request.max_depth, request.max_bytes),
            (0, 4, 40)
        );
        assert!(request.pattern.is_empty());
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: request.request_id,
                content: "- <snapshot truncated>".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap().content,
            "- <snapshot truncated>"
        );
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn search_files_forwards_limit_and_worker_error() {
        let gateway = SandboxGateway::new("workspace".into());
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let pending = tokio::spawn(async move { handle.search_files("readme", 7).await });
        let request = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::WorkspaceRequest(request) => request,
            _ => panic!("expected workspace request"),
        };
        assert_eq!(request.operation, "search_files");
        assert_eq!(request.pattern, "readme");
        assert_eq!(request.path, ".");
        assert_eq!(
            (request.max_entries, request.max_depth, request.max_bytes),
            (7, 0, 0)
        );
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: request.request_id,
                error: "worker search failed".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert!(
            pending
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("worker search failed")
        );
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn rejects_wrong_hello_and_duplicate_worker_then_preserves_pending_on_disconnect() {
        let gateway = SandboxGateway::new("workspace".into()).with_expected_generation(3);
        let (mut client, server) = server(gateway.clone()).await;
        let invalid = tokio_stream::iter(vec![hello("other", 3)]);
        let error = client.connect(invalid).await.err().unwrap();
        assert_eq!(error.code(), tonic::Code::PermissionDenied);
        let wrong_generation = client
            .connect(tokio_stream::iter(vec![hello("workspace", 4)]))
            .await
            .err()
            .unwrap();
        assert_eq!(wrong_generation.code(), tonic::Code::PermissionDenied);
        let (tx, mut stream) = connected(&mut client).await;
        let duplicate = client
            .connect(tokio_stream::iter(vec![hello("workspace", 3)]))
            .await
            .err()
            .unwrap();
        assert_eq!(duplicate.code(), tonic::Code::AlreadyExists);
        let pending = tokio::spawn({
            let handle = gateway.handle();
            async move { handle.read_file("a").await }
        });
        let id = match stream.message().await.unwrap().unwrap().body {
            Some(HostBody::WorkspaceRequest(request)) => request.request_id,
            _ => panic!("expected workspace request"),
        };
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(gateway.handle().canonical_root().is_none());
        let (tx, _stream) = connected(&mut client).await;
        gateway.wait_ready().await.unwrap();
        assert!(!pending.is_finished());
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: id,
                content: "reconnected".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert_eq!(pending.await.unwrap().unwrap().content, "reconnected");
        gateway.shutdown();
        assert!(gateway.wait_ready().await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn cancelled_execution_sends_cancel_and_ignores_late_completion() {
        let gateway = SandboxGateway::new("workspace".into()).with_expected_generation(3);
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let preparing = tokio::spawn({
            let handle = handle.clone();
            async move { handle.prepare("read", &serde_json::json!({})).await }
        });
        let request = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::Prepare(r) => r,
            _ => panic!("expected prepare"),
        };
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Prepared(proto::Prepared {
                invocation_id: request.invocation_id.clone(),
                ticket: "ticket".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let prepared = preparing.await.unwrap().unwrap();
        let cancellation = CancellationToken::new();
        let executing = tokio::spawn({
            let handle = handle.clone();
            let cancellation = cancellation.clone();
            async move {
                handle
                    .execute(prepared, cancellation, ToolProgressSink::noop())
                    .await
            }
        });
        assert!(matches!(
            stream.message().await.unwrap().unwrap().body,
            Some(HostBody::Execute(_))
        ));
        cancellation.cancel();
        assert!(executing.await.unwrap().is_err());
        let cancel = stream.message().await.unwrap().unwrap();
        assert!(
            matches!(cancel.body, Some(HostBody::Cancel(c)) if c.invocation_id == request.invocation_id)
        );
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Completed(proto::Completed {
                invocation_id: request.invocation_id,
                output: None,
                error: "late".into(),
            })),
        })
        .await
        .unwrap();
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn caps_pending_requests_and_dropped_futures_release_capacity() {
        let gateway = SandboxGateway::new("workspace".into()).with_expected_generation(3);
        let (mut client, server) = server(gateway.clone()).await;
        let (_tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let mut pending = Vec::new();
        for _ in 0..MAX_PENDING {
            let (reply, _rx) = oneshot::channel();
            let id = Uuid::now_v7().to_string();
            let (_, guard) = handle
                .send(
                    id.clone(),
                    Pending::Workspace(reply),
                    HostBody::WorkspaceRequest(proto::WorkspaceRequest {
                        request_id: id,
                        ..Default::default()
                    }),
                    None,
                    false,
                )
                .await
                .unwrap();
            pending.push(guard);
        }
        for _ in 0..MAX_PENDING {
            assert!(matches!(
                stream.message().await.unwrap().unwrap().body,
                Some(HostBody::WorkspaceRequest(_))
            ));
        }
        let error = handle.read_file("a").await.unwrap_err();
        assert!(error.to_string().contains("too many pending"));
        drop(pending);
        let (reply, _rx) = oneshot::channel();
        let id = Uuid::now_v7().to_string();
        assert!(
            handle
                .send(
                    id.clone(),
                    Pending::Workspace(reply),
                    HostBody::WorkspaceRequest(proto::WorkspaceRequest {
                        request_id: id,
                        ..Default::default()
                    }),
                    None,
                    false
                )
                .await
                .is_ok()
        );
        drop(stream.message().await);
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn dropped_prepared_tickets_and_unpolled_execute_release_worker_capacity() {
        let gateway = SandboxGateway::new("workspace".into()).with_expected_generation(3);
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let worker = tokio::spawn(async move {
            let mut tickets = HashSet::new();
            while let Ok(Some(response)) = stream.message().await {
                match response.body.unwrap() {
                    HostBody::Prepare(request) => {
                        let error = if tickets.is_empty() {
                            tickets.insert(request.invocation_id.clone());
                            String::new()
                        } else {
                            "worker ticket capacity exhausted".into()
                        };
                        tx.send(proto::ConnectRequest {
                            body: Some(WorkerBody::Prepared(proto::Prepared {
                                invocation_id: request.invocation_id,
                                ticket: "ticket".into(),
                                error,
                                ..Default::default()
                            })),
                        })
                        .await
                        .unwrap();
                    }
                    HostBody::Cancel(cancel) => {
                        assert!(tickets.remove(&cancel.invocation_id));
                    }
                    HostBody::Execute(_) => panic!("unpolled execute must not reach worker"),
                    _ => panic!("unexpected worker request"),
                }
            }
        });
        let handle = gateway.handle();
        timeout(Duration::from_secs(5), async {
            for index in 0..=MAX_PENDING {
                let prepared = handle
                    .prepare("read", &serde_json::json!({}))
                    .await
                    .unwrap();
                if index == 0 {
                    let unpolled = handle.execute(
                        prepared,
                        CancellationToken::new(),
                        ToolProgressSink::noop(),
                    );
                    drop(unpolled);
                } else {
                    drop(prepared);
                }
            }
        })
        .await
        .unwrap();
        gateway.shutdown();
        worker.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn queued_before_connect_and_full_outbound_wait_for_capacity() {
        let gateway = SandboxGateway::new("workspace".into());
        let handle = gateway.handle();
        let pending = tokio::spawn(async move { handle.read_file("queued").await });
        tokio::task::yield_now().await;
        assert_eq!(gateway.inner.state.lock().unwrap().pending.len(), 1);
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let id = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::WorkspaceRequest(request) => request.request_id,
            _ => panic!("expected queued request"),
        };
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::WorkspaceResponse(proto::WorkspaceResponse {
                request_id: id,
                content: "ok".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert_eq!(pending.await.unwrap().unwrap().content, "ok");

        let (outbound, mut receiver) = mpsc::channel(1);
        outbound
            .send(Ok(message(HostBody::Cancel(proto::Cancel::default()))))
            .await
            .unwrap();
        let worker = WorkerId {
            id: "worker".into(),
            generation: 3,
        };
        gateway.inner.state.lock().unwrap().connection = Some(Connection {
            id: 42,
            worker,
            root: PathBuf::new(),
            outbound,
        });
        let handle = gateway.handle();
        let sending = tokio::spawn(async move {
            let (reply, rx) = oneshot::channel();
            let (_, _guard) = handle
                .send(
                    "full".into(),
                    Pending::Prepare(reply),
                    HostBody::Prepare(proto::Prepare {
                        invocation_id: "full".into(),
                        ..Default::default()
                    }),
                    None,
                    false,
                )
                .await
                .unwrap();
            rx.await.unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!sending.is_finished());
        drop(receiver.recv().await);
        assert!(matches!(
            receiver.recv().await.unwrap().unwrap().body,
            Some(HostBody::Prepare(_))
        ));
        gateway
            .inner
            .dispatch(
                42,
                WorkerBody::Prepared(proto::Prepared {
                    invocation_id: "full".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        sending.await.unwrap();
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn disconnected_queue_is_capped_and_shutdown_wakes_it() {
        let gateway = SandboxGateway::new("workspace".into());
        let handle = gateway.handle();
        let mut queued = Vec::new();
        for index in 0..MAX_PENDING {
            let handle = handle.clone();
            queued.push(tokio::spawn(async move {
                handle.read_file(&index.to_string()).await
            }));
        }
        timeout(Duration::from_secs(2), async {
            while gateway.inner.state.lock().unwrap().pending.len() != MAX_PENDING {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            handle
                .read_file("overflow")
                .await
                .unwrap_err()
                .to_string()
                .contains("too many pending")
        );
        gateway.shutdown();
        for request in queued {
            assert!(request.await.unwrap().is_err());
        }
        assert!(gateway.inner.state.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn wrong_worker_and_old_epoch_cannot_complete_pending_or_use_ticket() {
        let gateway = SandboxGateway::new("workspace".into());
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let preparing = tokio::spawn({
            let handle = handle.clone();
            async move { handle.prepare("read", &serde_json::json!({})).await }
        });
        let id = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::Prepare(request) => request.invocation_id,
            _ => panic!("expected prepare"),
        };
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (tx, stream) = connected(&mut client).await;
        assert!(
            gateway
                .inner
                .dispatch(
                    1,
                    WorkerBody::Prepared(proto::Prepared {
                        invocation_id: id.clone(),
                        ..Default::default()
                    })
                )
                .is_err()
        );
        assert!(!preparing.is_finished());
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Prepared(proto::Prepared {
                invocation_id: id,
                ticket: "ticket".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let prepared = preparing.await.unwrap().unwrap();
        let old_worker = prepared.worker.clone();
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut replacement = hello("workspace", 3);
        if let Some(WorkerBody::Hello(ref mut h)) = replacement.body {
            h.worker_id = "replacement".into();
        }
        let (tx, rx) = mpsc::channel(16);
        tx.send(replacement).await.unwrap();
        let mut stream = client
            .connect(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        gateway.wait_ready().await.unwrap();
        let (reply, mut waiting) = oneshot::channel();
        gateway.inner.state.lock().unwrap().pending.insert(
            "old-pending".into(),
            PendingRequest {
                response: Pending::Prepare(reply),
                worker: Some(old_worker.clone()),
                sent: true,
            },
        );
        let epoch = gateway
            .inner
            .state
            .lock()
            .unwrap()
            .connection
            .as_ref()
            .unwrap()
            .id;
        gateway
            .inner
            .dispatch(
                epoch,
                WorkerBody::Prepared(proto::Prepared {
                    invocation_id: "old-pending".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert!(matches!(
            waiting.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let result = handle
            .execute(prepared, CancellationToken::new(), ToolProgressSink::noop())
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("stale sandbox ticket")
        );
        assert!(
            timeout(Duration::from_millis(30), stream.message())
                .await
                .is_err()
        );
        assert_ne!(
            gateway
                .inner
                .state
                .lock()
                .unwrap()
                .connection
                .as_ref()
                .unwrap()
                .worker,
            old_worker
        );
        gateway.shutdown();
        assert!(waiting.await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn execute_disconnect_does_not_resend_and_missing_reply_times_out() {
        let gateway = SandboxGateway::new("workspace".into());
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let worker = gateway
            .inner
            .state
            .lock()
            .unwrap()
            .connection
            .as_ref()
            .unwrap()
            .worker
            .clone();
        let prepared = PreparedSandboxCall {
            claims: vec![],
            invocation_id: "execution".into(),
            ticket: "ticket".into(),
            worker,
            execution_timeout: Duration::from_millis(150),
            cancel_on_drop: Some(handle.clone()),
        };
        let executing = tokio::spawn(async move {
            handle
                .execute(prepared, CancellationToken::new(), ToolProgressSink::noop())
                .await
        });
        let sent = stream.message().await.unwrap().unwrap();
        assert!(matches!(
            sent.body,
            Some(HostBody::Execute(proto::Execute {
                timeout_millis: 0,
                ..
            }))
        ));
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (_tx, mut stream) = connected(&mut client).await;
        assert!(
            timeout(Duration::from_millis(20), stream.message())
                .await
                .is_err()
        );
        let err = timeout(Duration::from_secs(2), executing)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        assert!(gateway.inner.state.lock().unwrap().pending.is_empty());
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn execute_reply_after_reconnect_is_correlated_once() {
        let gateway = SandboxGateway::new("workspace".into());
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let worker = gateway
            .inner
            .state
            .lock()
            .unwrap()
            .connection
            .as_ref()
            .unwrap()
            .worker
            .clone();
        let prepared = PreparedSandboxCall {
            claims: vec![],
            invocation_id: "execution".into(),
            ticket: "ticket".into(),
            worker,
            execution_timeout: Duration::from_secs(2),
            cancel_on_drop: Some(handle.clone()),
        };
        let executing = tokio::spawn(async move {
            handle
                .execute(prepared, CancellationToken::new(), ToolProgressSink::noop())
                .await
        });
        assert!(matches!(
            stream.message().await.unwrap().unwrap().body,
            Some(HostBody::Execute(_))
        ));
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (tx, mut stream) = connected(&mut client).await;
        assert!(
            timeout(Duration::from_millis(20), stream.message())
                .await
                .is_err()
        );
        let reply = proto::Completed {
            invocation_id: "execution".into(),
            output: Some(proto::ToolOutput {
                content: "recovered".into(),
                is_error: false,
            }),
            ..Default::default()
        };
        for _ in 0..2 {
            tx.send(proto::ConnectRequest {
                body: Some(WorkerBody::Completed(reply.clone())),
            })
            .await
            .unwrap();
        }
        assert_eq!(executing.await.unwrap().unwrap().content, "recovered");
        assert!(gateway.inner.state.lock().unwrap().pending.is_empty());
        gateway.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn ticket_executes_on_same_worker_after_reconnect_and_unused_ticket_cancels() {
        let gateway = SandboxGateway::new("workspace".into());
        let (mut client, server) = server(gateway.clone()).await;
        let (tx, mut stream) = connected(&mut client).await;
        let handle = gateway.handle();
        let preparing = tokio::spawn({
            let handle = handle.clone();
            async move { handle.prepare("read", &serde_json::json!({})).await }
        });
        let id = match stream.message().await.unwrap().unwrap().body.unwrap() {
            HostBody::Prepare(request) => request.invocation_id,
            _ => panic!("expected prepare"),
        };
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Prepared(proto::Prepared {
                invocation_id: id.clone(),
                ticket: "ticket".into(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        let prepared = preparing.await.unwrap().unwrap();
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (tx, mut stream) = connected(&mut client).await;
        let executing = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .execute(prepared, CancellationToken::new(), ToolProgressSink::noop())
                    .await
            }
        });
        assert!(matches!(stream.message().await.unwrap().unwrap().body,
            Some(HostBody::Execute(request)) if request.invocation_id == id && request.ticket == "ticket"));
        tx.send(proto::ConnectRequest {
            body: Some(WorkerBody::Completed(proto::Completed {
                invocation_id: id,
                output: Some(proto::ToolOutput {
                    content: "done".into(),
                    is_error: false,
                }),
                ..Default::default()
            })),
        })
        .await
        .unwrap();
        assert_eq!(executing.await.unwrap().unwrap().content, "done");

        let worker = gateway
            .inner
            .state
            .lock()
            .unwrap()
            .connection
            .as_ref()
            .unwrap()
            .worker
            .clone();
        let unused = PreparedSandboxCall {
            claims: vec![],
            invocation_id: "unused".into(),
            ticket: "ticket".into(),
            worker,
            execution_timeout: EXECUTE_TIMEOUT,
            cancel_on_drop: Some(handle),
        };
        drop(stream);
        drop(tx);
        timeout(Duration::from_secs(3), async {
            while gateway.inner.state.lock().unwrap().connection.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(unused);
        let (_tx, mut stream) = connected(&mut client).await;
        assert!(
            matches!(timeout(Duration::from_secs(2), stream.message()).await.unwrap().unwrap().unwrap().body,
            Some(HostBody::Cancel(cancel)) if cancel.invocation_id == "unused")
        );
        gateway.shutdown();
        server.abort();
    }
}
