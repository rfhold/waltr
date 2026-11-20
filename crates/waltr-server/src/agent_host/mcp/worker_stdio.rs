use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc};

use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::agent_host::{
    McpServerSelection,
    runtime_factory::{
        McpConnectionKind, McpConnectionResolveRequest, McpConnectionResolver,
        McpResolvedConnection,
    },
    sandbox_gateway::{
        SandboxHandle, WorkspaceScope,
        stdio::{SandboxStdioChannel, SandboxStdioOpenError, SandboxStdioTarget},
        stdio_launch,
    },
};
use crate::resource_catalog::{AgentDefinitionState, PublishedState};
use waltr::grpc::waltr::sandbox::v1::McpStdioLaunch;
use waltr_agent::AgentSessionRuntimeError;

pub(super) const MAX_FRAME_BYTES: usize = 1_048_448;

pub(super) enum WorkerOpenError {
    Rejected(mcp::Error),
    Other(mcp::Error),
}

impl From<mcp::Error> for WorkerOpenError {
    fn from(error: mcp::Error) -> Self {
        Self::Other(error)
    }
}

/// One restored workspace and its exact worker stream. Only local targets accept launch data.
#[derive(Clone)]
pub(crate) struct WorkerStdio {
    target: SandboxStdioTarget,
    scope: Option<WorkspaceScope>,
    root_uri: Option<String>,
    resolver: McpConnectionResolver,
    resources: Option<AgentDefinitionState>,
    admission: Arc<Mutex<()>>,
}

impl WorkerStdio {
    pub(crate) fn new(
        sandbox: &SandboxHandle,
        resolver: McpConnectionResolver,
        resources: Option<AgentDefinitionState>,
        admission: Arc<Mutex<()>>,
    ) -> Result<Self, AgentSessionRuntimeError> {
        let target = sandbox
            .stdio_target()
            .map_err(|error| AgentSessionRuntimeError::FailedPrecondition(error.to_string()))?;
        let scope = sandbox.scope().cloned();
        let directory = match &scope {
            Some(scope) => Some(scope.cwd()),
            None => sandbox.canonical_root(),
        };
        Ok(Self {
            target,
            scope,
            root_uri: directory
                .and_then(|directory| url::Url::from_file_path(directory).ok().map(Into::into)),
            resolver,
            resources,
            admission,
        })
    }

    pub(crate) fn same_target(&self, other: &Self) -> bool {
        self.scope == other.scope && self.target.same_connection(&other.target)
    }

    pub(super) fn root_uri(&self) -> Option<String> {
        self.root_uri.clone()
    }

    fn exported(&self) -> bool {
        self.scope
            .as_ref()
            .is_some_and(|scope| scope.account.is_some())
    }

    pub(super) async fn open(
        &self,
        expected: &McpResolvedConnection,
        lifetime: CancellationToken,
    ) -> Result<Arc<WorkerChannel>, WorkerOpenError> {
        let lease = lifetime.child_token();
        let opening = lease.clone().drop_guard();
        let mut updates = self.resources.as_ref().map(AgentDefinitionState::subscribe);
        // Remote admission never publishes; serialize only local catalog publication and open.
        let _admission = if self.exported() {
            None
        } else {
            Some(self.before_open(&lease, self.admission.lock()).await?)
        };
        if !self.exported() {
            let launches = self
                .before_open(&lease, self.local_catalog(expected))
                .await??;
            // Finish a sent publication before releasing admission, even if the caller leaves.
            self.target
                .publish_local_catalog(launches.clone())
                .await
                .map_err(transport_error)?;
            if self
                .before_open(&lease, self.local_catalog(expected))
                .await??
                != launches
            {
                return Err(revoked().into());
            }
        }
        let current = self
            .before_open(&lease, self.connections(selection(expected)))
            .await??;
        if !matches(&current, expected)
            || !drain_updates(&mut updates, expected)
            || lease.is_cancelled()
        {
            return Err(revoked().into());
        }
        let channel = if self.exported() {
            self.target
                .open_exported(&expected.key, lease.clone())
                .await
        } else {
            let launch = stdio_launch(&expected.key, &expected.command)
                .ok_or_else(|| mcp::Error::protocol("invalid local worker MCP stdio launch"))?;
            self.target.open_local(&launch, lease.clone()).await
        }
        .map_err(|error| match error {
            SandboxStdioOpenError::Rejected => WorkerOpenError::Rejected(transport_error(error)),
            SandboxStdioOpenError::Transport(error) => {
                WorkerOpenError::Other(transport_error(error))
            }
        })?;
        let worker = WorkerChannel::new(channel, lease, self.clone(), expected.clone(), updates);
        if let Err(error) = worker.validate().await {
            use mcp::client::McpFramedChannel as _;
            return Err(match worker.shutdown().await {
                Ok(()) => error,
                Err(cleanup) => mcp::Error::protocol(format!(
                    "{error}; worker MCP stdio child cleanup not confirmed: {cleanup}"
                )),
            }
            .into());
        }
        opening.disarm();
        Ok(worker)
    }

    async fn before_open<T>(
        &self,
        lease: &CancellationToken,
        future: impl Future<Output = T>,
    ) -> mcp::Result<T> {
        tokio::select! {
            biased;
            () = lease.cancelled() => Err(revoked()),
            () = self.target.retired() => Err(revoked()),
            output = future => Ok(output),
        }
    }

    async fn connections(
        &self,
        servers: McpServerSelection,
    ) -> mcp::Result<Vec<McpResolvedConnection>> {
        (self.resolver)(McpConnectionResolveRequest { servers })
            .await
            .map_err(transport_error)
    }

    async fn local_catalog(
        &self,
        expected: &McpResolvedConnection,
    ) -> mcp::Result<Vec<McpStdioLaunch>> {
        let catalog = self
            .connections(McpServerSelection::Exclude(BTreeSet::new()))
            .await?;
        if !matches(&catalog, expected) {
            return Err(revoked());
        }
        let mut launches = catalog
            .iter()
            .filter(|connection| connection.kind == McpConnectionKind::Stdio)
            .map(|connection| {
                stdio_launch(&connection.key, &connection.command)
                    .ok_or_else(|| mcp::Error::protocol("invalid local worker MCP stdio launch"))
            })
            .collect::<mcp::Result<Vec<_>>>()?;
        launches.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(launches)
    }
}

type Updates = Option<broadcast::Receiver<PublishedState>>;

fn selection(expected: &McpResolvedConnection) -> McpServerSelection {
    McpServerSelection::Include(BTreeSet::from([expected.key.clone()]))
}

fn matches(current: &[McpResolvedConnection], expected: &McpResolvedConnection) -> bool {
    let mut matching = current
        .iter()
        .filter(|connection| connection.key == expected.key);
    matching.next().is_some_and(|current| {
        current.kind == McpConnectionKind::Stdio
            && expected.kind == McpConnectionKind::Stdio
            && current.command == expected.command
    }) && matching.next().is_none()
}

fn permits(update: &PublishedState, expected: &McpResolvedConnection) -> bool {
    update
        .resource_catalog()
        .effective_configuration
        .is_some_and(|config| {
            crate::session::resolve_mcp_connections(
                &config,
                McpConnectionResolveRequest {
                    servers: selection(expected),
                },
            )
            .is_ok_and(|connections| matches(&connections, expected))
        })
}

fn drain_updates(updates: &mut Updates, expected: &McpResolvedConnection) -> bool {
    if let Some(updates) = updates {
        loop {
            match updates.try_recv() {
                Ok(update) if permits(&update, expected) => {}
                Err(broadcast::error::TryRecvError::Empty) => break,
                _ => return false,
            }
        }
    }
    true
}

fn transport_error(error: impl std::fmt::Display) -> mcp::Error {
    mcp::Error::protocol(error.to_string())
}

fn revoked() -> mcp::Error {
    mcp::Error::protocol("worker MCP stdio resource or workspace is no longer authorized")
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = mcp::Result<T>> + Send + 'a>>;
type Cleanup = Result<(), String>;

/// Own cleanup independently of request futures, including failed or dropped discovery.
pub(super) struct WorkerChannel {
    channel: Arc<SandboxStdioChannel>,
    lease: CancellationToken,
    cleanup: watch::Receiver<Option<Cleanup>>,
    worker: WorkerStdio,
    expected: McpResolvedConnection,
}

impl WorkerChannel {
    fn new(
        channel: SandboxStdioChannel,
        lease: CancellationToken,
        worker: WorkerStdio,
        expected: McpResolvedConnection,
        mut updates: Updates,
    ) -> Arc<Self> {
        let (completed, cleanup) = watch::channel(None);
        let channel = Arc::new(channel);
        let close_channel = channel.clone();
        let close_lease = lease.clone();
        let target = worker.target.clone();
        let connection = expected.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = close_lease.cancelled() => {}
                () = target.retired() => {}
                () = async {
                    if let Some(updates) = &mut updates {
                        while let Ok(update) = updates.recv().await {
                            if !permits(&update, &connection) {
                                return;
                            }
                        }
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
            close_lease.cancel();
            let outcome = close_channel
                .shutdown()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            if let Err(error) = &outcome {
                tracing::warn!(server = %connection.key, %error, "worker MCP stdio child cleanup not confirmed");
            }
            completed.send_replace(Some(outcome));
        });
        Arc::new(Self {
            channel,
            lease,
            cleanup,
            worker,
            expected,
        })
    }

    pub(super) fn alive(&self) -> bool {
        !self.lease.is_cancelled()
    }

    pub(super) fn fence(&self) {
        self.lease.cancel();
    }

    pub(super) fn is_exported(&self) -> bool {
        self.worker.exported()
    }

    async fn validate(&self) -> mcp::Result<()> {
        let permitted = tokio::select! {
            biased;
            () = self.lease.cancelled() => false,
            current = self.worker.connections(selection(&self.expected)) =>
                current.is_ok_and(|current| matches(&current, &self.expected)),
        };
        if !permitted {
            self.fence();
            return Err(revoked());
        }
        Ok(())
    }
}

impl Drop for WorkerChannel {
    fn drop(&mut self) {
        self.fence();
    }
}

impl mcp::client::McpFramedChannel for WorkerChannel {
    fn send_frame(&self, data: Vec<u8>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.validate().await?;
            let result = self.channel.send(data).await;
            if result.is_err() {
                self.fence();
            }
            result.map_err(transport_error)
        })
    }

    fn recv_frame(&self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let frame = self.channel.recv().await;
            if matches!(frame, Ok(Some(_))) {
                self.validate().await?;
            } else {
                self.fence();
            }
            frame.map_err(transport_error)
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.fence();
            let mut cleanup = self.cleanup.clone();
            cleanup
                .wait_for(Option::is_some)
                .await
                .map_err(|_| mcp::Error::protocol("worker MCP stdio child cleanup not confirmed"))?
                .as_ref()
                .expect("cleanup outcome published")
                .clone()
                .map_err(transport_error)
        })
    }
}
