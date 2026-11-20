use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{HostBody, WorkerBody, message, proto};
use crate::{
    Sandbox,
    mcp_stdio::{
        PinnedDirectory, StdioChild, StdioExit, StdioInput, StdioLease, StdioOutput,
        StdioStartError,
    },
};

pub(crate) const VERSION: u32 = 1;
const MAX_ACTIVE_CHANNELS: usize = 8;
const MAX_STREAM_OPENS: usize = 4096;
const MAX_CATALOG_ENTRIES: usize = 32;
const MAX_CATALOG_BYTES: usize = 128 * 1024;
const MAX_NAME_BYTES: usize = 128;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 8192;
const FRAME_BYTES: usize = crate::mcp_stdio::MAX_FRAME_BYTES - 128;
pub(super) const OUTPUT_DEPTH: usize = 8;
const INPUT_DEPTH: usize = 4;

#[derive(PartialEq, Eq)]
struct Launch {
    configured_executable: String,
    arguments: Vec<OsString>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct LaunchCatalog(HashMap<String, Arc<Launch>>);

impl LaunchCatalog {
    pub(crate) async fn load(
        _root: PathBuf,
        definitions: Vec<proto::McpStdioLaunch>,
    ) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || Self::check(definitions))
            .await
            .unwrap_or_else(|_| Err(invalid_catalog()))
    }

    fn check(definitions: Vec<proto::McpStdioLaunch>) -> io::Result<Self> {
        if definitions.len() > MAX_CATALOG_ENTRIES {
            return Err(invalid_catalog());
        }
        let mut total = 0;
        let mut launches = HashMap::new();
        for spec in definitions {
            if !bounded(&spec.name, MAX_NAME_BYTES)
                || !bounded(&spec.executable, MAX_PATH_BYTES)
                || !valid_executable_path(Path::new(&spec.executable))
                || spec.arguments.len() > MAX_ARGUMENTS
                || spec
                    .arguments
                    .iter()
                    .any(|arg| arg.len() > MAX_ARGUMENT_BYTES || arg.contains('\0'))
            {
                return Err(invalid_catalog());
            }
            total += spec.name.len() + spec.executable.len();
            total += spec.arguments.iter().map(String::len).sum::<usize>();
            if total > MAX_CATALOG_BYTES || launches.contains_key(&spec.name) {
                return Err(invalid_catalog());
            }
            launches.insert(
                spec.name,
                Arc::new(Launch {
                    configured_executable: spec.executable,
                    arguments: spec.arguments.into_iter().map(OsString::from).collect(),
                }),
            );
        }
        Ok(Self(launches))
    }
}

fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn invalid_catalog() -> io::Error {
    io::Error::other("invalid worker stdio catalog")
}

fn valid_executable_path(command: &Path) -> bool {
    let mut components = command.components();
    command.is_absolute()
        || (matches!(components.next(), Some(Component::Normal(name)) if name == command.as_os_str())
            && components.next().is_none())
}

fn resolve_executable(
    root: &Path,
    command: &Path,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if command.is_absolute() {
        return executable_file(command).then(|| command.to_owned());
    }
    if !valid_executable_path(command) {
        return None;
    }
    // Ignore relative PATH entries and never resolve a bare name inside the export.
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .filter(|dir| {
            !dir.starts_with(root)
                && dir
                    .canonicalize()
                    .is_ok_and(|resolved| !resolved.starts_with(root))
        })
        .map(|dir| dir.join(command))
        .find_map(|candidate| {
            let resolved = candidate.canonicalize().ok()?;
            (!resolved.starts_with(root)
                && executable_file(&candidate)
                && executable_file(&resolved))
            .then_some(resolved)
        })
}

fn executable_file(path: &Path) -> bool {
    #[cfg(windows)]
    if !path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return false;
    }
    path.metadata().is_ok_and(|metadata| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        metadata.is_file()
    })
}

pub(super) struct Outbound {
    message: proto::ConnectRequest,
    // Only frames require a second fence at dequeue. Acknowledgements and terminal status
    // remain ordered to describe what was accepted and whether a child was joined.
    frame_fence: Option<(CancellationToken, CancellationToken)>,
    open_ready: Option<Arc<AtomicBool>>,
}

impl Outbound {
    pub(super) fn into_live_message(self) -> Option<proto::ConnectRequest> {
        if self
            .frame_fence
            .is_some_and(|(channel, catalog)| channel.is_cancelled() || catalog.is_cancelled())
        {
            None
        } else {
            if let Some(ready) = self.open_ready {
                ready.store(true, Ordering::Release);
            }
            Some(self.message)
        }
    }
}

#[derive(Clone)]
struct Outbox {
    sender: mpsc::Sender<Outbound>,
    stream: CancellationToken,
}

impl Outbox {
    fn send(
        &self,
        body: WorkerBody,
        frame_fence: Option<(CancellationToken, CancellationToken)>,
    ) -> bool {
        self.enqueue(body, frame_fence, None)
    }

    fn enqueue(
        &self,
        body: WorkerBody,
        frame_fence: Option<(CancellationToken, CancellationToken)>,
        open_ready: Option<Arc<AtomicBool>>,
    ) -> bool {
        if self.stream.is_cancelled() {
            return false;
        }
        if self
            .sender
            .try_send(Outbound {
                message: message(body),
                frame_fence,
                open_ready,
            })
            .is_err()
        {
            // An undeliverable reply makes the ephemeral tunnel ambiguous; fence the stream.
            self.stream.cancel();
            return false;
        }
        true
    }

    fn opened(&self, channel_id: String, ready: Option<Arc<AtomicBool>>) -> bool {
        self.enqueue(
            WorkerBody::McpStdioOpened(proto::McpStdioOpened {
                channel_id,
                opened: ready.is_some(),
            }),
            None,
            ready,
        )
    }

    fn catalog_ack(&self, request_id: String, applied: bool) {
        self.send(
            WorkerBody::McpStdioCatalogAck(proto::McpStdioCatalogAck {
                request_id,
                applied,
            }),
            None,
        );
    }
}

type Status = proto::McpStdioTerminalStatus;

struct Active {
    channel_id: String,
    server_name: String,
    input: mpsc::Sender<Vec<u8>>,
    ready: Arc<AtomicBool>,
    stop: CancellationToken,
    closing: Option<Status>,
}

impl Active {
    fn close(&mut self, status: Status) {
        self.closing.get_or_insert(status);
        self.stop.cancel();
    }
}

struct Granted {
    launch: Arc<Launch>,
    fence: CancellationToken,
}

type Catalog = HashMap<String, Granted>;

enum Event {
    Validated(String, io::Result<LaunchCatalog>),
    Channel(Uuid, Option<(Status, bool)>),
}

enum Replacement {
    Validating,
    Retiring(String, Catalog),
}

pub(super) struct StreamStdio {
    private: bool,
    catalog: Catalog,
    replacement: Option<Replacement>,
    seen: HashSet<Uuid>,
    active: HashMap<Uuid, Active>,
    tasks: JoinSet<Event>,
    out: Outbox,
    expiry: Option<Instant>,
    path_checks: Arc<Semaphore>,
    _drop_fence: tokio_util::sync::DropGuard,
}

impl StreamStdio {
    pub(super) fn path_checks() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(MAX_ACTIVE_CHANNELS + 1))
    }

    pub(super) fn new(
        private: bool,
        catalog: LaunchCatalog,
        sender: mpsc::Sender<Outbound>,
        stream: CancellationToken,
        expiry: Option<Instant>,
        path_checks: Arc<Semaphore>,
    ) -> Self {
        Self {
            private,
            catalog: catalog
                .0
                .into_iter()
                .map(|(name, launch)| {
                    let fence = stream.child_token();
                    (name, Granted { launch, fence })
                })
                .collect(),
            replacement: None,
            seen: HashSet::new(),
            active: HashMap::new(),
            tasks: JoinSet::new(),
            out: Outbox {
                sender,
                stream: stream.clone(),
            },
            expiry,
            path_checks,
            _drop_fence: stream.drop_guard(),
        }
    }

    pub(super) fn handle(&mut self, body: HostBody, sandbox: &Arc<Sandbox>) {
        if self.out.stream.is_cancelled()
            || self.expiry.is_some_and(|expiry| expiry <= Instant::now())
        {
            return;
        }
        match body {
            HostBody::McpStdioCatalogUpdate(update) => self.update(update),
            HostBody::McpStdioOpen(open) => self.open(open, sandbox),
            HostBody::McpStdioFrame(frame) => {
                if let Some(active) =
                    channel_id(&frame.channel_id).and_then(|id| self.active.get_mut(&id))
                    && active.closing.is_none()
                    && (!active.ready.load(Ordering::Acquire)
                        || frame.data.len() > FRAME_BYTES
                        || frame.data.contains(&b'\n')
                        || active.input.try_send(frame.data).is_err())
                {
                    active.close(Status::Failed);
                }
            }
            HostBody::McpStdioClose(close) => {
                if let Some(active) =
                    channel_id(&close.channel_id).and_then(|id| self.active.get_mut(&id))
                {
                    active.close(Status::Closed);
                }
            }
            _ => unreachable!("worker stdio messages only"),
        }
    }

    pub(super) fn export_ack(&self, request_id: String, applied: bool) {
        // Private control replies are never retained across a reconnect.
        self.out.send(
            WorkerBody::WorkspaceExportAck(proto::WorkspaceExportAck {
                request_id,
                applied,
            }),
            None,
        );
    }

    fn update(&mut self, update: proto::McpStdioCatalogUpdate) {
        if !bounded(&update.request_id, MAX_NAME_BYTES) {
            self.out.stream.cancel();
        } else if !self.private || self.replacement.is_some() {
            self.out.catalog_ack(update.request_id, false);
        } else {
            let Ok(permit) = self.path_checks.clone().try_acquire_owned() else {
                self.out.catalog_ack(update.request_id, false);
                return;
            };
            self.replacement = Some(Replacement::Validating);
            let check = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                LaunchCatalog::check(update.launches)
            });
            let stream = self.out.stream.clone();
            self.tasks.spawn(async move {
                let outcome = tokio::select! {
                    biased;
                    () = stream.cancelled() => Err(invalid_catalog()),
                    result = check => result.unwrap_or_else(|_| Err(invalid_catalog())),
                };
                Event::Validated(update.request_id, outcome)
            });
        }
    }

    fn open(&mut self, open: proto::McpStdioOpen, sandbox: &Arc<Sandbox>) {
        let Some(id) = channel_id(&open.channel_id) else {
            self.out.stream.cancel();
            return;
        };
        if self.seen.len() >= MAX_STREAM_OPENS || !self.seen.insert(id) {
            self.out.stream.cancel();
            return;
        }
        if !bounded(&open.server_name, MAX_NAME_BYTES)
            || !bounded(&open.working_directory, MAX_PATH_BYTES)
        {
            self.out.opened(open.channel_id, None);
            return;
        }
        let granted = self.catalog.get(&open.server_name).filter(|granted| {
            !granted.fence.is_cancelled() && self.active.len() < MAX_ACTIVE_CHANNELS
        });
        let Some(granted) = granted else {
            self.out.opened(open.channel_id, None);
            return;
        };
        let Ok(permit) = self.path_checks.clone().try_acquire_owned() else {
            self.out.opened(open.channel_id, None);
            return;
        };
        let (input, incoming) = mpsc::channel(INPUT_DEPTH);
        let stop = granted.fence.child_token();
        let ready = Arc::new(AtomicBool::new(false));
        self.active.insert(
            id,
            Active {
                channel_id: open.channel_id.clone(),
                server_name: open.server_name,
                input,
                ready: ready.clone(),
                stop: stop.clone(),
                closing: None,
            },
        );
        let sandbox = sandbox.clone();
        let directory = open.working_directory;
        let launch = granted.launch.clone();
        let command = launch.clone();
        let mut check = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let path = std::env::var_os("PATH");
            let executable = resolve_executable(
                sandbox.root(),
                Path::new(&command.configured_executable),
                path.as_deref(),
            )?;
            let cwd = PinnedDirectory::open(sandbox.root(), Path::new(&directory)).ok()?;
            Some((cwd, executable))
        });
        let channel = Channel {
            channel_id: open.channel_id,
            out: self.out.clone(),
            ready,
            stop,
            catalog_fence: granted.fence.clone(),
            expiry: self.expiry,
        };
        self.tasks.spawn(async move {
            let cwd = tokio::select! {
                biased;
                () = channel.stop.cancelled() => {
                    check.abort();
                    None
                }
                cwd = &mut check => cwd.ok().flatten(),
            };
            Event::Channel(id, channel.run(cwd, launch, incoming).await)
        });
    }

    pub(super) fn has_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub(super) async fn next(&mut self) {
        match self.tasks.join_next().await {
            Some(Ok(Event::Validated(request_id, Ok(catalog)))) => {
                self.retire(request_id, catalog);
            }
            Some(Ok(Event::Validated(request_id, Err(_)))) => {
                self.replacement = None;
                self.out.catalog_ack(request_id, false);
            }
            Some(Ok(Event::Channel(id, outcome))) => {
                let active = self
                    .active
                    .remove(&id)
                    .expect("tracked worker stdio channel");
                if let Some((status, reaped)) = outcome {
                    let status = active.closing.unwrap_or(status);
                    self.out.send(
                        WorkerBody::McpStdioTerminal(proto::McpStdioTerminal {
                            channel_id: active.channel_id,
                            status: status as i32,
                            reaped,
                        }),
                        None,
                    );
                    if !reaped
                        && self
                            .catalog
                            .get(&active.server_name)
                            .is_some_and(|granted| granted.fence.is_cancelled())
                    {
                        self.out.stream.cancel();
                    }
                }
                self.finish_replacement();
            }
            Some(Err(_)) => self.out.stream.cancel(),
            None => {}
        }
    }

    fn retire(&mut self, request_id: String, next: LaunchCatalog) {
        for (name, previous) in &self.catalog {
            if next.0.get(name) != Some(&previous.launch) {
                previous.fence.cancel();
            }
        }
        for active in self.active.values_mut() {
            if self.catalog[&active.server_name].fence.is_cancelled() {
                active.close(Status::Revoked);
            }
        }
        let replacement = next
            .0
            .into_iter()
            .map(|(name, launch)| {
                let fence = self
                    .catalog
                    .get(&name)
                    .filter(|previous| !previous.fence.is_cancelled())
                    .map_or_else(
                        || self.out.stream.child_token(),
                        |previous| previous.fence.clone(),
                    );
                (name, Granted { launch, fence })
            })
            .collect();
        self.replacement = Some(Replacement::Retiring(request_id, replacement));
        self.finish_replacement();
    }

    fn finish_replacement(&mut self) {
        if matches!(self.replacement, Some(Replacement::Retiring(..)))
            && !self
                .active
                .values()
                .any(|active| self.catalog[&active.server_name].fence.is_cancelled())
            && let Some(Replacement::Retiring(request_id, catalog)) = self.replacement.take()
        {
            self.catalog = catalog;
            self.out.catalog_ack(request_id, true);
        }
    }

    pub(super) async fn shutdown(mut self) {
        self.out.stream.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

fn channel_id(raw: &str) -> Option<Uuid> {
    if raw.len() != 36 {
        return None;
    }
    Uuid::parse_str(raw)
        .ok()
        .filter(|id| id.hyphenated().encode_lower(&mut Uuid::encode_buffer()) == raw)
}

struct Channel {
    channel_id: String,
    out: Outbox,
    ready: Arc<AtomicBool>,
    stop: CancellationToken,
    catalog_fence: CancellationToken,
    expiry: Option<Instant>,
}

impl Channel {
    async fn run(
        self,
        resolved: Option<(PinnedDirectory, PathBuf)>,
        launch: Arc<Launch>,
        incoming: mpsc::Receiver<Vec<u8>>,
    ) -> Option<(Status, bool)> {
        let Some((cwd, executable)) = resolved else {
            self.out.opened(self.channel_id, None);
            return None;
        };
        let (mut child, input, output) = match StdioChild::spawn_pinned(
            &executable,
            &launch.arguments,
            cwd,
            StdioLease {
                parent: self.stop.clone(),
                deadline: self.expiry,
            },
            FRAME_BYTES,
        ) {
            Ok(started) => started,
            Err(StdioStartError::BeforeLaunch(_)) => {
                self.out.opened(self.channel_id, None);
                return None;
            }
            Err(StdioStartError::AfterLaunch(_)) => {
                self.out.stream.cancel();
                return None;
            }
        };
        if !self
            .out
            .opened(self.channel_id.clone(), Some(self.ready.clone()))
        {
            let _ = child.shutdown().await;
            return None;
        }
        let status = tokio::select! {
            biased;
            () = self.stop.cancelled() => Status::Closed,
            () = super::wait_for_expiry(self.expiry) => Status::Closed,
            status = self.forward_output(output) => status,
            () = forward_input(incoming, input) => Status::Failed,
        };
        let joined = if status == Status::Exited {
            child.join_outcome().await
        } else {
            child.shutdown_outcome().await
        };
        let status = match joined.result {
            Ok(StdioExit::StdoutClosed) if status == Status::Exited => Status::Exited,
            Ok(_) if status != Status::Exited => status,
            _ => Status::Failed,
        };
        Some((status, joined.reaped))
    }

    async fn forward_output(&self, mut output: StdioOutput) -> Status {
        while let Some(data) = output.recv().await {
            if self.stop.is_cancelled()
                || self.expiry.is_some_and(|expiry| expiry <= Instant::now())
            {
                return Status::Closed;
            }
            if !self.out.send(
                WorkerBody::McpStdioFrame(proto::McpStdioFrame {
                    channel_id: self.channel_id.clone(),
                    data,
                }),
                Some((self.stop.clone(), self.catalog_fence.clone())),
            ) {
                return Status::Failed;
            }
        }
        Status::Exited
    }
}

async fn forward_input(mut incoming: mpsc::Receiver<Vec<u8>>, mut input: StdioInput) {
    while let Some(frame) = incoming.recv().await {
        if input.send(&frame).await.is_err() {
            return;
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
