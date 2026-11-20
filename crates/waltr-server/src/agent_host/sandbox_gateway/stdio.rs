use std::collections::BTreeMap;

use tokio::sync::Semaphore;

use super::*;

const VERSION: u32 = 1;
const MAX_ACTIVE: usize = 8;
const MAX_ATTEMPTS: usize = 4096;
const FRAME_BYTES: usize = 1_048_448;
const FRAME_DEPTH: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(10);

type Terminal = proto::McpStdioTerminalStatus;
type Catalog = BTreeMap<String, proto::McpStdioLaunch>;

#[derive(Debug)]
pub(crate) enum SandboxStdioOpenError {
    /// The worker acknowledged this open without starting the configured process.
    Rejected,
    /// The gateway did not obtain a definitive worker rejection.
    Transport(LlmError),
}

impl From<LlmError> for SandboxStdioOpenError {
    fn from(error: LlmError) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Display for SandboxStdioOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected => formatter.write_str("sandbox stdio open rejected"),
            Self::Transport(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SandboxStdioOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected => None,
            Self::Transport(error) => Some(error),
        }
    }
}

struct CatalogChange {
    id: String,
    next: Catalog,
    reply: oneshot::Sender<bool>,
}

pub(super) struct ConnectionStdio {
    version: u32,
    catalog: Option<Catalog>,
    updating: Option<CatalogChange>,
    active: HashMap<String, ActiveChannel>,
    attempts: usize,
    frames: Arc<Semaphore>,
}

struct ActiveChannel {
    name: String,
    opened: Option<oneshot::Sender<bool>>,
    frame: mpsc::Sender<Vec<u8>>,
    terminal: watch::Sender<Option<(Terminal, bool)>>,
    fenced: CancellationToken,
    write_fence: CancellationToken,
}

impl ConnectionStdio {
    pub(super) fn new(version: u32) -> Self {
        Self {
            version,
            catalog: None,
            updating: None,
            active: HashMap::new(),
            attempts: 0,
            frames: Arc::new(Semaphore::new(FRAME_DEPTH)),
        }
    }

    pub(super) fn supported(&self) -> bool {
        self.version == VERSION
    }

    pub(super) fn dispatch(
        &mut self,
        body: WorkerBody,
        private: bool,
    ) -> std::result::Result<(), Status> {
        if !self.supported() {
            return Err(Status::invalid_argument("worker stdio version unsupported"));
        }
        match body {
            WorkerBody::McpStdioCatalogAck(ack) => {
                if !private {
                    return Err(Status::permission_denied(
                        "stdio catalog control is private",
                    ));
                }
                if self
                    .updating
                    .as_ref()
                    .is_some_and(|change| change.id == ack.request_id)
                {
                    let change = self.updating.take().expect("matched catalog update");
                    if ack.applied {
                        if self.active.values().any(|channel| {
                            self.catalog
                                .as_ref()
                                .and_then(|catalog| catalog.get(&channel.name))
                                != change.next.get(&channel.name)
                        }) {
                            return Err(protocol_violation());
                        }
                        self.catalog = Some(change.next);
                    } else {
                        // The previous worker catalog may still be present but cannot open locally.
                        self.catalog = None;
                    }
                    let _ = change.reply.send(ack.applied);
                }
            }
            WorkerBody::McpStdioOpened(ack) => {
                if let Some(channel) = self.active.get_mut(&ack.channel_id) {
                    let reply = channel.opened.take().ok_or_else(protocol_violation)?;
                    if !ack.opened {
                        self.active.remove(&ack.channel_id);
                    }
                    let _ = reply.send(ack.opened);
                }
            }
            WorkerBody::McpStdioFrame(frame) => {
                if let Some(channel) = self.active.get(&frame.channel_id)
                    && !channel.fenced.is_cancelled()
                    && (channel.opened.is_some()
                        || !valid_frame(&frame.data)
                        || channel.frame.try_send(frame.data).is_err())
                {
                    return Err(protocol_violation());
                }
            }
            WorkerBody::McpStdioTerminal(terminal) => {
                if let Some(channel) = self.active.remove(&terminal.channel_id) {
                    let status = Terminal::try_from(terminal.status)
                        .ok()
                        .filter(|status| *status != Terminal::Unspecified)
                        .filter(|_| channel.opened.is_none())
                        .ok_or_else(protocol_violation)?;
                    if status != Terminal::Exited || !terminal.reaped {
                        channel.fenced.cancel();
                    }
                    channel.write_fence.cancel();
                    channel
                        .terminal
                        .send_replace(Some((status, terminal.reaped)));
                }
            }
            _ => unreachable!("worker stdio messages only"),
        }
        Ok(())
    }
}

fn protocol_violation() -> Status {
    Status::invalid_argument("invalid or undeliverable worker stdio message")
}

fn valid_frame(data: &[u8]) -> bool {
    data.len() <= FRAME_BYTES && !data.contains(&b'\n')
}

fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

pub(super) fn validate_catalog(launches: &[proto::McpStdioLaunch]) -> Result<()> {
    let invalid = || error("invalid sandbox stdio catalog");
    if launches.len() > 32 {
        return Err(invalid());
    }
    let mut keys = std::collections::HashSet::new();
    let mut bytes = 0;
    for launch in launches {
        if !bounded(&launch.name, 128)
            || !bounded(&launch.executable, 4096)
            || launch.arguments.len() > 64
            || launch
                .arguments
                .iter()
                .any(|arg| arg.len() > 8192 || arg.contains('\0'))
        {
            return Err(invalid());
        }
        bytes += launch.name.len()
            + launch.executable.len()
            + launch.arguments.iter().map(String::len).sum::<usize>();
        if bytes > 128 * 1024 || !keys.insert(&launch.name) {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Parse a materialized stdio command exactly as the existing runtime does.
pub(crate) fn stdio_launch(name: &str, command: &str) -> Option<proto::McpStdioLaunch> {
    let mut parts = command.split_whitespace();
    Some(proto::McpStdioLaunch {
        name: name.into(),
        executable: parts.next()?.into(),
        arguments: parts.map(str::to_owned).collect(),
    })
}

/// Stdio authority for exactly one live connection and resolved workspace directory.
#[derive(Clone)]
pub(crate) struct SandboxStdioTarget {
    inner: Arc<Inner>,
    epoch: u64,
    retired: CancellationToken,
    expires: Option<Instant>,
    directory: String,
}

impl SandboxHandle {
    /// Pin stdio to this handle's current stream. Hosted handles must already be workspace-bound.
    pub(crate) fn stdio_target(&self) -> Result<SandboxStdioTarget> {
        let state = self.inner.state.lock().unwrap();
        let connection = state
            .connection
            .as_ref()
            .filter(|connection| connection.live())
            .ok_or_else(|| error("sandbox worker not connected"))?;
        if !connection.stdio.supported() {
            return Err(error("sandbox worker does not support stdio version 1"));
        }
        if let Some(scope) = self.scope() {
            if self.scope_epoch != Some(connection.id)
                || scope.canonical_root != connection.root
                || scope.machine_id != self.inner.workspace_id
                || scope.account != self.inner.account
            {
                return Err(error(
                    "sandbox workspace was resolved on a different stream",
                ));
            }
        } else if !self.inner.export_control {
            return Err(error("sandbox stdio requires a bound workspace"));
        }
        Ok(SandboxStdioTarget {
            inner: self.inner.clone(),
            epoch: connection.id,
            retired: connection.retired.clone(),
            expires: connection.expires,
            directory: self
                .scope()
                .map_or(".", |scope| scope.working_directory.as_str())
                .into(),
        })
    }
}

impl SandboxStdioTarget {
    /// Epoch within this gateway only.
    pub(crate) fn connection_epoch(&self) -> u64 {
        self.epoch
    }

    /// Compare both the gateway identity and its live-stream epoch.
    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner) && self.epoch == other.epoch
    }

    pub(crate) async fn retired(&self) {
        tokio::select! {
            () = self.retired.cancelled() => {}
            () = wait_for_expiry(self.expires) => {}
        }
    }

    fn local(&self) -> bool {
        self.inner.export_control && self.inner.account.is_none()
    }

    fn connection<'a>(&self, state: &'a mut State) -> Result<&'a mut Connection> {
        state
            .connection
            .as_mut()
            .filter(|c| c.id == self.epoch && c.live())
            .ok_or_else(|| error("sandbox stdio connection retired"))
    }

    /// Replace the complete local catalog. Every new stream starts unpublished.
    /// An in-flight replacement blocks opens; competing replacements fail without publishing.
    pub(crate) async fn publish_local_catalog(
        &self,
        launches: Vec<proto::McpStdioLaunch>,
    ) -> Result<()> {
        if !self.local() {
            return Err(error("sandbox stdio catalog control is private"));
        }
        validate_catalog(&launches)?;
        let next: Catalog = launches
            .into_iter()
            .map(|launch| (launch.name.clone(), launch))
            .collect();
        let id = Uuid::now_v7().to_string();
        let (reply, received) = oneshot::channel();
        {
            let mut state = self.inner.state.lock().unwrap();
            let connection = self.connection(&mut state)?;
            if connection.stdio.updating.is_some() {
                return Err(error(
                    "sandbox stdio catalog replacement is already pending",
                ));
            }
            if connection.stdio.catalog.as_ref() == Some(&next) {
                return Ok(());
            }
            for active in connection.stdio.active.values() {
                if connection
                    .stdio
                    .catalog
                    .as_ref()
                    .and_then(|catalog| catalog.get(&active.name))
                    != next.get(&active.name)
                {
                    active.fenced.cancel();
                }
            }
            if connection
                .outbound
                .try_send(
                    HostBody::McpStdioCatalogUpdate(proto::McpStdioCatalogUpdate {
                        request_id: id.clone(),
                        launches: next.values().cloned().collect(),
                    })
                    .into(),
                )
                .is_err()
            {
                // The old grant must not survive an undeliverable revocation.
                drop(state);
                self.inner.disconnect(self.epoch);
                return Err(error("sandbox stdio catalog delivery unavailable"));
            }
            connection.stdio.updating = Some(CatalogChange { id, next, reply });
        }
        let retire = RetireOnDrop(Some(self));
        let applied = within(Instant::now() + TIMEOUT, async {
            received
                .await
                .map_err(|_| error("sandbox stdio catalog acknowledgement unavailable"))
        })
        .await?;
        if !applied {
            return Err(error("sandbox stdio catalog rejected"));
        }
        self.connection(&mut self.inner.state.lock().unwrap())?;
        retire.disarm();
        Ok(())
    }

    /// Open only if the exact launch still belongs to the acknowledged local catalog.
    pub(crate) async fn open_local(
        &self,
        launch: &proto::McpStdioLaunch,
        cancellation: CancellationToken,
    ) -> std::result::Result<SandboxStdioChannel, SandboxStdioOpenError> {
        if !self.local() {
            return Err(error("local stdio is unavailable on a hosted stream").into());
        }
        self.open(&launch.name, Some(launch), cancellation).await
    }

    /// Open only by name on the already authenticated, account-bound worker stream.
    pub(crate) async fn open_exported(
        &self,
        name: &str,
        cancellation: CancellationToken,
    ) -> std::result::Result<SandboxStdioChannel, SandboxStdioOpenError> {
        if self.inner.account.is_none() {
            return Err(error("exported stdio requires an account-bound stream").into());
        }
        self.open(name, None, cancellation).await
    }

    async fn open(
        &self,
        name: &str,
        local: Option<&proto::McpStdioLaunch>,
        cancellation: CancellationToken,
    ) -> std::result::Result<SandboxStdioChannel, SandboxStdioOpenError> {
        if !bounded(name, 128) || !bounded(&self.directory, 4096) || cancellation.is_cancelled() {
            return Err(error("invalid or cancelled sandbox stdio open").into());
        }
        let id = Uuid::now_v7().to_string();
        let (opened, waiting) = oneshot::channel();
        let (frame, frames) = mpsc::channel(FRAME_DEPTH);
        let (terminal, ended) = watch::channel(None);
        let fenced = cancellation.child_token();
        let write_fence = fenced.child_token();
        let close = CancellationToken::new();
        {
            let mut state = self.inner.state.lock().unwrap();
            let connection = self.connection(&mut state)?;
            let stdio = &mut connection.stdio;
            if local.is_some()
                && (stdio.updating.is_some()
                    || stdio.catalog.as_ref().and_then(|catalog| catalog.get(name)) != local)
            {
                return Err(
                    error("sandbox stdio launch is not in the acknowledged catalog").into(),
                );
            }
            if stdio.active.len() == MAX_ACTIVE || stdio.attempts == MAX_ATTEMPTS {
                return Err(error("sandbox stdio channel limit reached").into());
            }
            connection
                .outbound
                .try_send(
                    HostBody::McpStdioOpen(proto::McpStdioOpen {
                        channel_id: id.clone(),
                        server_name: name.into(),
                        working_directory: self.directory.clone(),
                    })
                    .into(),
                )
                .map_err(|_| error("sandbox stdio open delivery unavailable"))?;
            stdio.attempts += 1;
            stdio.active.insert(
                id.clone(),
                ActiveChannel {
                    name: name.into(),
                    opened: Some(opened),
                    frame,
                    terminal,
                    fenced: fenced.clone(),
                    write_fence: write_fence.clone(),
                },
            );
        }
        let channel = SandboxStdioChannel {
            target: self.clone(),
            id,
            frames: tokio::sync::Mutex::new(frames),
            ended,
            fenced,
            write_fence,
            close,
            cancellation,
        };
        channel.watch_close();
        tokio::select! {
            biased;
            () = self.retired() => return Err(error("sandbox stdio connection retired during open").into()),
            () = channel.cancellation.cancelled() => return Err(error("sandbox stdio open cancelled").into()),
            () = channel.fenced.cancelled() => return Err(error("sandbox stdio open revoked").into()),
            result = within(Instant::now() + TIMEOUT, async {
                waiting.await.map_err(|_| error("sandbox stdio open acknowledgement unavailable"))
            }) => {
                if !result? {
                    return Err(SandboxStdioOpenError::Rejected);
                }
            }
        }
        Ok(channel)
    }
}

struct RetireOnDrop<'a>(Option<&'a SandboxStdioTarget>);

impl RetireOnDrop<'_> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for RetireOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(target) = self.0 {
            target.inner.disconnect(target.epoch);
        }
    }
}

/// Owns one worker stdio channel; send and receive may run concurrently.
pub(crate) struct SandboxStdioChannel {
    target: SandboxStdioTarget,
    id: String,
    frames: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    ended: watch::Receiver<Option<(Terminal, bool)>>,
    fenced: CancellationToken,
    write_fence: CancellationToken,
    close: CancellationToken,
    cancellation: CancellationToken,
}

impl SandboxStdioChannel {
    /// Queue one opaque frame, without a trailing newline. This is not a delivery receipt.
    pub(crate) async fn send(&self, data: Vec<u8>) -> Result<()> {
        if !valid_frame(&data) {
            return Err(error("invalid sandbox stdio frame"));
        }
        let (outbound, credits) = {
            let mut state = self.target.inner.state.lock().unwrap();
            let connection = self.target.connection(&mut state)?;
            (connection.outbound.clone(), connection.stdio.frames.clone())
        };
        let write = async {
            let credit = credits
                .acquire_owned()
                .await
                .map_err(|_| error("sandbox stdio unavailable"))?;
            let permit = outbound
                .reserve()
                .await
                .map_err(|_| error("sandbox stdio connection retired"))?;
            let mut state = self.target.inner.state.lock().unwrap();
            let connection = self.target.connection(&mut state)?;
            if self.cancellation.is_cancelled()
                || self.fenced.is_cancelled()
                || !connection.stdio.active.contains_key(&self.id)
            {
                return Err(error("sandbox stdio channel closed"));
            }
            permit.send(OutboundMessage {
                body: HostBody::McpStdioFrame(proto::McpStdioFrame {
                    channel_id: self.id.clone(),
                    data,
                }),
                frame: Some((self.write_fence.clone(), credit)),
            });
            Ok(())
        };
        tokio::select! {
            biased;
            () = self.fenced.cancelled() => Err(error("sandbox stdio channel closed")),
            () = self.cancellation.cancelled() => Err(error("sandbox stdio channel cancelled")),
            () = self.target.retired() => Err(error("sandbox stdio connection retired")),
            result = within(Instant::now() + TIMEOUT, write) => {
                if result.is_err() { self.request_close(); }
                result
            }
        }
    }

    /// Receive one opaque frame, or EOF after a clean, confirmed child exit.
    pub(crate) async fn recv(&self) -> Result<Option<Vec<u8>>> {
        let read = async { self.frames.lock().await.recv().await };
        tokio::select! {
            biased;
            () = self.fenced.cancelled() => Err(error("sandbox stdio channel closed")),
            () = self.cancellation.cancelled() => Err(error("sandbox stdio channel cancelled")),
            () = self.target.retired() => Err(error("sandbox stdio connection retired")),
            data = read => match data {
                Some(data) => Ok(Some(data)),
                None if matches!(*self.ended.borrow(), Some((Terminal::Exited, true))) => Ok(None),
                None => Err(error("sandbox stdio channel ended without clean exit")),
            }
        }
    }

    /// Close and wait for a correlated terminal affirming direct-child reaping.
    pub(crate) async fn shutdown(&self) -> Result<Terminal> {
        self.request_close();
        let mut ended = self.ended.clone();
        within(Instant::now() + TIMEOUT, async {
            ended
                .wait_for(Option::is_some)
                .await
                .map_err(|_| error("sandbox stdio cleanup not confirmed"))?
                .as_ref()
                .filter(|(_, reaped)| *reaped)
                .map(|(status, _)| *status)
                .ok_or_else(|| error("sandbox stdio cleanup not confirmed"))
        })
        .await
    }

    fn request_close(&self) {
        self.fenced.cancel();
        self.close.cancel();
    }

    fn watch_close(&self) {
        let target = self.target.clone();
        let close = self.close.clone();
        let fenced = self.fenced.clone();
        let cancellation = self.cancellation.clone();
        let mut ended = self.ended.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = ended.wait_for(Option::is_some) => return,
                () = target.retired() => return,
                () = cancellation.cancelled() => {}
                () = close.cancelled() => {}
            }
            fenced.cancel();
            let queued = {
                let mut state = target.inner.state.lock().unwrap();
                let Ok(connection) = target.connection(&mut state) else {
                    return;
                };
                if !connection.stdio.active.contains_key(&id) {
                    return;
                }
                connection
                    .outbound
                    .try_send(
                        HostBody::McpStdioClose(proto::McpStdioClose { channel_id: id }).into(),
                    )
                    .is_ok()
            };
            if !queued
                || timeout_at(Instant::now() + TIMEOUT, ended.wait_for(Option::is_some))
                    .await
                    .is_err()
            {
                target.inner.disconnect(target.epoch);
            }
        });
    }
}

impl Drop for SandboxStdioChannel {
    fn drop(&mut self) {
        self.request_close();
    }
}

#[cfg(test)]
mod tests;
