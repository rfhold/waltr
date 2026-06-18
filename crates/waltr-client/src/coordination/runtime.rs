//! Shared reducer runtime used by shell adapters.

use std::fmt;
use std::future::Future;
use std::rc::Rc;

use futures_channel::oneshot;
use futures_util::{StreamExt, future::Either, future::LocalBoxFuture, future::select, pin_mut};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::executor::{ConfigWatchUpdateStream, SessionWatchUpdateStream};
use crate::{
    Client, ClientEffect, ClientError, ClientEvent, ClientOperationState, ClientUpdate,
    ConfigEffect, ConfigEvent, ConfigView, GrpcEffectExecutor, McpServerView, OperationKey,
    OperationStatus, ProviderView, SessionEffect, SessionEvent, SessionView, StatusChangeStream,
    StatusEffect, StatusEvent,
};

const READY_IDLE_DELAY_MS: u64 = 1_000;

type ClientSleep = Rc<dyn Fn(u64) -> LocalBoxFuture<'static, ()>>;

/// Command accepted by the shared client runtime.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientCommand {
    /// Load the current configuration.
    RequestConfig,
    /// Add a provider to the latest configuration.
    AddProvider {
        /// Provider to add.
        provider: ProviderView,
    },
    /// Update an existing provider in the latest configuration.
    UpdateProvider {
        /// Current provider name.
        #[serde(rename = "originalName")]
        original_name: String,
        /// Replacement provider definition.
        provider: ProviderView,
    },
    /// Delete a provider from the latest configuration.
    DeleteProvider {
        /// Provider name to delete.
        name: String,
    },
    /// Add an MCP server to the latest configuration.
    AddMcpServer {
        /// MCP server to add.
        server: McpServerView,
    },
    /// Update an existing MCP server in the latest configuration.
    UpdateMcpServer {
        /// Current MCP server key.
        #[serde(rename = "originalName")]
        original_name: String,
        /// Replacement MCP server definition.
        server: McpServerView,
    },
    /// Delete an MCP server from the latest configuration.
    DeleteMcpServer {
        /// MCP server key to delete.
        name: String,
    },
    /// Register current interest in configuration watch updates.
    RegisterConfigWatchInterest,
    /// Release current interest in configuration watch updates.
    ReleaseConfigWatchInterest,
    /// Create a new server-side session.
    CreateSession,
    /// Delete a server-side session by id.
    DeleteSession {
        /// Session id to delete.
        id: String,
    },
    /// Register current interest in session watch updates.
    RegisterSessionWatchInterest,
    /// Release current interest in session watch updates.
    ReleaseSessionWatchInterest,
    /// Process one status watch update.
    WatchStatus,
    /// Keep watching status until the reducer observes readiness.
    WaitForSystemReady,
}

/// Stable snapshot of shared client state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientSnapshot {
    /// Command operation result snapshots.
    pub operations: OperationCollectionSnapshot,
    /// Configuration coordination snapshot.
    pub config: ConfigSnapshot,
    /// System status coordination snapshot.
    pub status: StatusSnapshot,
    /// Session coordination snapshot.
    pub sessions: SessionSnapshot,
}

/// Stable snapshot of command operation results.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OperationCollectionSnapshot {
    /// Operation results by runtime-local operation key.
    pub current: std::collections::BTreeMap<OperationKey, ClientOperationView>,
}

/// Stable snapshot of one command operation result.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientOperationView {
    /// Configuration patch command result.
    ConfigPatch {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Patched configuration, when the operation completed successfully.
        config: Option<ConfigView>,
    },
    /// Session creation command result.
    SessionCreate {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Created session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// Session deletion command result.
    SessionDelete {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Deleted session id.
        id: String,
    },
}

/// Stable snapshot of configuration coordination state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    /// Last known configuration snapshot.
    pub current: Option<ConfigView>,
    /// Configuration load operation snapshot.
    pub load: OperationSnapshot,
    /// Configuration patch operation snapshot.
    pub patch: OperationSnapshot,
    /// Configuration watch operation snapshot.
    pub watch: OperationSnapshot,
}

/// Stable snapshot of status coordination state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    /// Whether the last known system status is ready.
    pub ready: bool,
    /// Status watch operation snapshot.
    pub watch: OperationSnapshot,
}

/// Stable snapshot of session coordination state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    /// Last known sessions.
    pub current: Vec<SessionView>,
    /// Session create operation snapshot.
    pub create: OperationSnapshot,
    /// Session list operation snapshot.
    pub list: OperationSnapshot,
    /// Session delete operation snapshot.
    pub delete: OperationSnapshot,
    /// Session watch operation snapshot.
    pub watch: OperationSnapshot,
}

/// Stable snapshot of one operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OperationSnapshot {
    /// Stable operation status name.
    pub status: &'static str,
    /// Last operation error, when present.
    pub error: Option<String>,
    /// Runtime-local key for the latest operation of this kind.
    pub key: Option<OperationKey>,
}

/// Shared reducer plus effect executor loop used by shells.
pub struct ClientRuntime {
    reducer: Client,
    executor: GrpcEffectExecutor,
    status_watch: Option<StatusChangeStream>,
    config_watch: Option<ConfigWatchUpdateStream>,
    session_watch: Option<SessionWatchUpdateStream>,
}

/// Command sent to a shared client state stream loop.
#[derive(Debug)]
pub struct ClientCommandRequest {
    command: ClientCommand,
    result: Option<oneshot::Sender<Result<ClientSnapshot, ClientError>>>,
}

/// Result of advancing the shared client state stream loop once.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientStateStreamEvent {
    /// The stream produced a snapshot that should be published to the shell.
    Snapshot(Box<ClientSnapshot>),
    /// The command source has closed.
    CommandsClosed,
}

impl ClientCommandRequest {
    /// Create a command request without a result channel.
    #[must_use]
    pub const fn new(command: ClientCommand) -> Self {
        Self {
            command,
            result: None,
        }
    }

    /// Create a command request that replies with the command result.
    #[must_use]
    pub const fn with_result(
        command: ClientCommand,
        result: oneshot::Sender<Result<ClientSnapshot, ClientError>>,
    ) -> Self {
        Self {
            command,
            result: Some(result),
        }
    }
}

impl fmt::Debug for ClientRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientRuntime")
            .field("reducer", &self.reducer)
            .field("executor", &self.executor)
            .field("status_watch_active", &self.status_watch.is_some())
            .field("config_watch_active", &self.config_watch.is_some())
            .field("session_watch_active", &self.session_watch.is_some())
            .finish()
    }
}

impl ClientRuntime {
    /// Create a runtime for an endpoint base URL.
    #[must_use]
    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            reducer: Client::new(),
            executor: GrpcEffectExecutor::for_endpoint(endpoint),
            status_watch: None,
            config_watch: None,
            session_watch: None,
        }
    }

    /// Return the current reducer snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        ClientSnapshot::from_client(&self.reducer)
    }

    /// Convert this runtime into a startup/config state stream.
    #[must_use]
    pub fn into_state_stream<Sleep, SleepFuture>(self, sleep: Sleep) -> ClientStateStream
    where
        Sleep: Fn(u64) -> SleepFuture + 'static,
        SleepFuture: Future<Output = ()> + 'static,
    {
        ClientStateStream::new(self, sleep)
    }

    async fn execute_effect<Sleep, SleepFuture>(
        &mut self,
        effect: ClientEffect,
        sleep: Sleep,
    ) -> Vec<ClientEvent>
    where
        Sleep: FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        match effect {
            ClientEffect::Status(StatusEffect::Watch) => match self.executor.watch_status().await {
                Ok(stream) => {
                    self.status_watch = Some(stream);
                    vec![ClientEvent::Status(StatusEvent::WatchStarted)]
                }
                Err(error) => vec![ClientEvent::Status(StatusEvent::WatchFailed(error))],
            },
            ClientEffect::Config(ConfigEffect::Watch) => {
                match self.executor.watch_config_updates().await {
                    Ok(stream) => {
                        self.config_watch = Some(stream);
                        vec![ClientEvent::Config(ConfigEvent::WatchStarted)]
                    }
                    Err(error) => vec![ClientEvent::Config(ConfigEvent::WatchFailed(error))],
                }
            }
            ClientEffect::Session(SessionEffect::Watch) => {
                match self.executor.watch_session_updates().await {
                    Ok(stream) => {
                        self.session_watch = Some(stream);
                        vec![ClientEvent::Session(SessionEvent::WatchStarted)]
                    }
                    Err(error) => vec![ClientEvent::Session(SessionEvent::WatchFailed(error))],
                }
            }
            effect => self.executor.execute_effect(effect, sleep).await,
        }
    }

    async fn next_watch_event(&mut self) -> ClientEvent {
        if self.reducer.state().sessions.watch.interests > 0 {
            if let (Some(session), Some(config), Some(status)) = (
                &mut self.session_watch,
                &mut self.config_watch,
                &mut self.status_watch,
            ) {
                let session_next = session.next();
                let config_next = config.next();
                let status_next = status.next();
                pin_mut!(session_next);
                pin_mut!(config_next);
                pin_mut!(status_next);

                let data_next = select(session_next, config_next);
                pin_mut!(data_next);

                return match select(data_next, status_next).await {
                    Either::Left((Either::Left((Some(Ok(update)), _)), _)) => {
                        ClientEvent::Session(SessionEvent::WatchChanged(update))
                    }
                    Either::Left((Either::Left((Some(Err(error)), _)), _)) => {
                        self.session_watch = None;
                        ClientEvent::Session(SessionEvent::WatchFailed(error))
                    }
                    Either::Left((Either::Left((None, _)), _)) => {
                        self.session_watch = None;
                        ClientEvent::Session(SessionEvent::WatchEnded)
                    }
                    Either::Left((Either::Right((Some(Ok(update)), _)), _)) => {
                        ClientEvent::Config(ConfigEvent::WatchChanged(update))
                    }
                    Either::Left((Either::Right((Some(Err(error)), _)), _)) => {
                        self.config_watch = None;
                        ClientEvent::Config(ConfigEvent::WatchFailed(error))
                    }
                    Either::Left((Either::Right((None, _)), _)) => {
                        self.config_watch = None;
                        ClientEvent::Config(ConfigEvent::WatchEnded)
                    }
                    Either::Right((Some(Ok(status)), _)) => {
                        ClientEvent::Status(StatusEvent::WatchChanged(status))
                    }
                    Either::Right((Some(Err(error)), _)) => {
                        self.status_watch = None;
                        ClientEvent::Status(StatusEvent::WatchFailed(error))
                    }
                    Either::Right((None, _)) => {
                        self.status_watch = None;
                        ClientEvent::Status(StatusEvent::WatchEnded)
                    }
                };
            }

            if let (Some(session), Some(status)) = (&mut self.session_watch, &mut self.status_watch)
            {
                let session_next = session.next();
                let status_next = status.next();
                pin_mut!(session_next);
                pin_mut!(status_next);

                return match select(session_next, status_next).await {
                    Either::Left((Some(Ok(update)), _)) => {
                        ClientEvent::Session(SessionEvent::WatchChanged(update))
                    }
                    Either::Left((Some(Err(error)), _)) => {
                        self.session_watch = None;
                        ClientEvent::Session(SessionEvent::WatchFailed(error))
                    }
                    Either::Left((None, _)) => {
                        self.session_watch = None;
                        ClientEvent::Session(SessionEvent::WatchEnded)
                    }
                    Either::Right((Some(Ok(status)), _)) => {
                        ClientEvent::Status(StatusEvent::WatchChanged(status))
                    }
                    Either::Right((Some(Err(error)), _)) => {
                        self.status_watch = None;
                        ClientEvent::Status(StatusEvent::WatchFailed(error))
                    }
                    Either::Right((None, _)) => {
                        self.status_watch = None;
                        ClientEvent::Status(StatusEvent::WatchEnded)
                    }
                };
            }

            if let Some(stream) = &mut self.session_watch {
                return match stream.next().await {
                    Some(Ok(update)) => ClientEvent::Session(SessionEvent::WatchChanged(update)),
                    Some(Err(error)) => {
                        self.session_watch = None;
                        ClientEvent::Session(SessionEvent::WatchFailed(error))
                    }
                    None => {
                        self.session_watch = None;
                        ClientEvent::Session(SessionEvent::WatchEnded)
                    }
                };
            }
        }

        if self.reducer.state().config.watch.interests > 0 {
            if let (Some(config), Some(status)) = (&mut self.config_watch, &mut self.status_watch) {
                let config_next = config.next();
                let status_next = status.next();
                pin_mut!(config_next);
                pin_mut!(status_next);

                return match select(config_next, status_next).await {
                    Either::Left((Some(Ok(update)), _)) => {
                        ClientEvent::Config(ConfigEvent::WatchChanged(update))
                    }
                    Either::Left((Some(Err(error)), _)) => {
                        self.config_watch = None;
                        ClientEvent::Config(ConfigEvent::WatchFailed(error))
                    }
                    Either::Left((None, _)) => {
                        self.config_watch = None;
                        ClientEvent::Config(ConfigEvent::WatchEnded)
                    }
                    Either::Right((Some(Ok(status)), _)) => {
                        ClientEvent::Status(StatusEvent::WatchChanged(status))
                    }
                    Either::Right((Some(Err(error)), _)) => {
                        self.status_watch = None;
                        ClientEvent::Status(StatusEvent::WatchFailed(error))
                    }
                    Either::Right((None, _)) => {
                        self.status_watch = None;
                        ClientEvent::Status(StatusEvent::WatchEnded)
                    }
                };
            }

            if let Some(stream) = &mut self.config_watch {
                return match stream.next().await {
                    Some(Ok(update)) => ClientEvent::Config(ConfigEvent::WatchChanged(update)),
                    Some(Err(error)) => {
                        self.config_watch = None;
                        ClientEvent::Config(ConfigEvent::WatchFailed(error))
                    }
                    None => {
                        self.config_watch = None;
                        ClientEvent::Config(ConfigEvent::WatchEnded)
                    }
                };
            }
        }

        if let Some(stream) = &mut self.status_watch {
            return match stream.next().await {
                Some(Ok(status)) => ClientEvent::Status(StatusEvent::WatchChanged(status)),
                Some(Err(error)) => {
                    self.status_watch = None;
                    ClientEvent::Status(StatusEvent::WatchFailed(error))
                }
                None => {
                    self.status_watch = None;
                    ClientEvent::Status(StatusEvent::WatchEnded)
                }
            };
        }

        ClientEvent::Status(StatusEvent::ReconnectReady)
    }
}

/// Shared client state stream consumed by shell runtimes.
pub struct ClientStateStream {
    runtime: ClientRuntime,
    phase: ClientStateStreamPhase,
    effects: Vec<ClientEffect>,
    sleep: ClientSleep,
}

impl fmt::Debug for ClientStateStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientStateStream")
            .field("runtime", &self.runtime)
            .field("phase", &self.phase)
            .field("effects", &self.effects)
            .finish()
    }
}

impl ClientStateStream {
    fn new<Sleep, SleepFuture>(runtime: ClientRuntime, sleep: Sleep) -> Self
    where
        Sleep: Fn(u64) -> SleepFuture + 'static,
        SleepFuture: Future<Output = ()> + 'static,
    {
        Self {
            runtime,
            phase: ClientStateStreamPhase::WaitForSystemReady,
            effects: Vec::new(),
            sleep: Rc::new(move |delay_ms| Box::pin(sleep(delay_ms))),
        }
    }

    /// Return the next state snapshot from shared startup/config orchestration.
    pub async fn next(&mut self) -> ClientSnapshot {
        loop {
            if let Some(effect) = self.effects.pop() {
                self.apply_effect(effect).await;
                return self.snapshot();
            }

            match self.phase {
                ClientStateStreamPhase::WaitForSystemReady
                    if self
                        .runtime
                        .reducer
                        .state()
                        .status
                        .current
                        .is_some_and(|status| status.ready) =>
                {
                    self.phase = ClientStateStreamPhase::Ready;
                }
                ClientStateStreamPhase::WaitForSystemReady => {
                    if self.runtime.status_watch.is_some() {
                        self.apply_next_watch_event().await;
                    } else {
                        let update = self.runtime.reducer.maintain_watches();
                        self.extend_effects(&update);
                    }
                    return self.snapshot();
                }
                ClientStateStreamPhase::Ready => {
                    if self.runtime.reducer.state().status.watch.status == OperationStatus::Received
                        && self.runtime.reducer.state().config.watch.interests == 0
                        && self.runtime.reducer.state().sessions.watch.interests == 0
                    {
                        (self.sleep)(READY_IDLE_DELAY_MS).await;
                        let update = self.runtime.reducer.maintain_watches();
                        self.extend_effects(&update);
                        return self.snapshot();
                    }

                    if self.runtime.config_watch.is_some()
                        || self.runtime.session_watch.is_some()
                        || self.runtime.status_watch.is_some()
                    {
                        self.apply_next_watch_event().await;
                        return self.snapshot();
                    }

                    let update = self.runtime.reducer.maintain_watches();
                    self.extend_effects(&update);
                    return self.snapshot();
                }
            }
        }
    }

    /// Advance until either a client snapshot changes or a command arrives.
    pub async fn next_or_dispatch<CommandFuture>(
        &mut self,
        command: CommandFuture,
    ) -> ClientStateStreamEvent
    where
        CommandFuture: Future<Output = Option<ClientCommandRequest>>,
    {
        let request = {
            let next = self.next();
            pin_mut!(next);
            pin_mut!(command);

            match select(command, next).await {
                Either::Left((Some(request), _pending_next)) => request,
                Either::Left((None, _)) => return ClientStateStreamEvent::CommandsClosed,
                Either::Right((snapshot, _)) => {
                    return ClientStateStreamEvent::Snapshot(Box::new(snapshot));
                }
            }
        };

        ClientStateStreamEvent::Snapshot(Box::new(self.dispatch_request(request).await))
    }

    /// Return the current stream snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        self.runtime.snapshot()
    }

    async fn dispatch_request(&mut self, request: ClientCommandRequest) -> ClientSnapshot {
        let result = self
            .begin_command(request.command)
            .map(|()| self.snapshot());
        let snapshot = result.clone().unwrap_or_else(|_| self.snapshot());

        if let Some(reply) = request.result {
            drop(reply.send(result));
        }

        snapshot
    }

    fn begin_command(&mut self, command: ClientCommand) -> Result<(), ClientError> {
        let update = match command {
            ClientCommand::RequestConfig => self.runtime.reducer.request_config(),
            ClientCommand::AddProvider { provider } => {
                self.runtime.reducer.add_provider(provider.into())?
            }
            ClientCommand::UpdateProvider {
                original_name,
                provider,
            } => self
                .runtime
                .reducer
                .update_provider(&original_name, provider.into())?,
            ClientCommand::DeleteProvider { name } => {
                self.runtime.reducer.delete_provider(&name)?
            }
            ClientCommand::AddMcpServer { server } => {
                self.runtime.reducer.add_mcp_server(server.into())?
            }
            ClientCommand::UpdateMcpServer {
                original_name,
                server,
            } => self
                .runtime
                .reducer
                .update_mcp_server(&original_name, server.into())?,
            ClientCommand::DeleteMcpServer { name } => {
                self.runtime.reducer.delete_mcp_server(&name)?
            }
            ClientCommand::RegisterConfigWatchInterest => {
                self.runtime.reducer.register_config_watch_interest()
            }
            ClientCommand::ReleaseConfigWatchInterest => {
                let update = self.runtime.reducer.release_config_watch_interest();
                if self.runtime.reducer.state().config.watch.interests == 0 {
                    self.runtime.config_watch = None;
                }
                update
            }
            ClientCommand::CreateSession => self.runtime.reducer.create_session(),
            ClientCommand::DeleteSession { id } => self.runtime.reducer.delete_session(id),
            ClientCommand::RegisterSessionWatchInterest => {
                self.runtime.reducer.register_session_watch_interest()
            }
            ClientCommand::ReleaseSessionWatchInterest => {
                let update = self.runtime.reducer.release_session_watch_interest();
                if self.runtime.reducer.state().sessions.watch.interests == 0 {
                    self.runtime.session_watch = None;
                }
                update
            }
            ClientCommand::WatchStatus => self.runtime.reducer.request_status_watch(),
            ClientCommand::WaitForSystemReady => self.runtime.reducer.maintain_watches(),
        };

        self.extend_effects(&update);
        Ok(())
    }

    async fn apply_effect(&mut self, effect: ClientEffect) {
        let sleep = Rc::clone(&self.sleep);
        let events = self
            .runtime
            .execute_effect(effect, move |delay_ms| sleep(delay_ms))
            .await;

        for event in events {
            let update = self.runtime.reducer.apply(event);
            self.extend_effects(&update);
        }
    }

    async fn apply_next_watch_event(&mut self) {
        let event = self.runtime.next_watch_event().await;
        let update = self.runtime.reducer.apply(event);
        self.extend_effects(&update);
    }

    fn extend_effects(&mut self, update: &ClientUpdate) {
        self.effects.extend(update.effects().iter().cloned().rev());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientStateStreamPhase {
    WaitForSystemReady,
    Ready,
}

impl ClientSnapshot {
    fn from_client(client: &Client) -> Self {
        let state = client.state();

        Self {
            operations: OperationCollectionSnapshot {
                current: state
                    .operations
                    .iter()
                    .map(|(key, operation)| {
                        (key.clone(), ClientOperationView::from(operation.clone()))
                    })
                    .collect(),
            },
            config: ConfigSnapshot {
                current: state.config.current.clone().map(ConfigView::from),
                load: OperationSnapshot::from_state(&state.config.load),
                patch: OperationSnapshot::from_state(&state.config.patch),
                watch: OperationSnapshot::from_parts(
                    state.config.watch.status,
                    state.config.watch.error.clone(),
                ),
            },
            status: StatusSnapshot {
                ready: state.status.current.is_some_and(|status| status.ready),
                watch: OperationSnapshot::from_parts(
                    state.status.watch.status,
                    state.status.watch.error.clone(),
                ),
            },
            sessions: SessionSnapshot {
                current: state
                    .sessions
                    .current
                    .values()
                    .cloned()
                    .map(SessionView::from)
                    .collect(),
                create: OperationSnapshot::from_state(&state.sessions.create),
                list: OperationSnapshot::from_state(&state.sessions.list),
                delete: OperationSnapshot::from_state(&state.sessions.delete),
                watch: OperationSnapshot::from_parts(
                    state.sessions.watch.status,
                    state.sessions.watch.error.clone(),
                ),
            },
        }
    }
}

impl From<ClientOperationState> for ClientOperationView {
    fn from(operation: ClientOperationState) -> Self {
        match operation {
            ClientOperationState::ConfigPatch {
                status,
                error,
                config,
            } => Self::ConfigPatch {
                status: operation_status_name(status),
                error,
                config: config.map(ConfigView::from),
            },
            ClientOperationState::SessionCreate {
                status,
                error,
                session,
            } => Self::SessionCreate {
                status: operation_status_name(status),
                error,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionDelete { status, error, id } => Self::SessionDelete {
                status: operation_status_name(status),
                error,
                id,
            },
        }
    }
}

impl OperationSnapshot {
    fn from_state(state: &crate::OperationState) -> Self {
        Self {
            status: operation_status_name(state.status),
            error: state.error.clone(),
            key: state.key.clone(),
        }
    }

    const fn from_parts(status: OperationStatus, error: Option<String>) -> Self {
        Self {
            status: operation_status_name(status),
            error,
            key: None,
        }
    }
}

const fn operation_status_name(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Idle => "idle",
        OperationStatus::Loading => "loading",
        OperationStatus::Ready => "ready",
        OperationStatus::Opened => "opened",
        OperationStatus::Received => "received",
        OperationStatus::Waiting => "waiting",
        OperationStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use waltr::grpc::waltr::status::v1::SystemStatus;

    use crate::transport::ConfigWatchUpdate;

    #[tokio::test]
    async fn state_stream_yields_status_loading_before_transport_work() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);

        let snapshot = stream.next().await;

        assert_eq!(snapshot.status.watch.status, "loading");
        assert!(!snapshot.status.ready);
    }

    #[tokio::test]
    async fn state_stream_yields_failed_status_before_reconnect_sleep() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);

        drop(stream.next().await);
        let snapshot = stream.next().await;

        assert_eq!(snapshot.status.watch.status, "failed");
        assert!(snapshot.status.watch.error.is_some());
    }

    #[tokio::test]
    async fn active_config_watch_does_not_starve_status_watch_failures() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchStarted));
        runtime.status_watch = Some(Box::pin(stream::once(async {
            Err(ClientError::new("status ended"))
        })));
        runtime.config_watch = Some(Box::pin(stream::pending::<Result<ConfigWatchUpdate, _>>()));

        let event = runtime.next_watch_event().await;
        let update = runtime.reducer.apply(event);
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.status.watch.status, "failed");
        assert_eq!(snapshot.config.watch.status, "waiting");
        assert!(!snapshot.status.ready);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::SleepReconnect {
                delay_ms: 500,
            })]
        ));
    }

    #[tokio::test]
    async fn config_transport_loss_reconnects_status_before_config() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchStarted));
        runtime.status_watch = Some(Box::pin(stream::pending::<Result<SystemStatus, _>>()));
        runtime.config_watch = Some(Box::pin(stream::once(async {
            Err(ClientError::new("config transport lost"))
        })));

        let event = runtime.next_watch_event().await;
        let update = runtime.reducer.apply(event);
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.status.watch.status, "failed");
        assert!(!snapshot.status.ready);
        assert_eq!(snapshot.config.watch.status, "waiting");
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::SleepReconnect {
                delay_ms: 500,
            })]
        ));
    }

    #[tokio::test]
    async fn ready_state_without_config_interest_yields_without_waiting_for_status_watch() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime.status_watch = Some(Box::pin(stream::pending::<Result<SystemStatus, _>>()));
        let mut stream = ClientStateStream {
            runtime,
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };

        let snapshot = tokio::time::timeout(std::time::Duration::from_millis(10), stream.next())
            .await
            .expect("ready stream should not wait indefinitely for idle status watch");

        assert_eq!(snapshot.status.watch.status, "received");
        assert_eq!(snapshot.config.watch.status, "idle");
    }

    async fn no_sleep(_: u64) {}
}
