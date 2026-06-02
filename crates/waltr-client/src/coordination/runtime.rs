//! Shared reducer runtime used by shell adapters.

use std::fmt;
use std::future::Future;

use futures_channel::oneshot;
use futures_util::{StreamExt, future::Either, future::select, pin_mut};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::executor::ConfigWatchUpdateStream;
use crate::{
    Client, ClientEffect, ClientError, ClientEvent, ClientUpdate, ConfigEffect, ConfigEvent,
    ConfigView, GrpcEffectExecutor, McpServerView, OperationStatus, ProviderView,
    StatusChangeStream, StatusEffect, StatusEvent,
};

const READY_IDLE_DELAY_MS: u64 = 1_000;

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
    /// Process one status watch update.
    WatchStatus,
    /// Keep watching status until the reducer observes readiness.
    WaitForSystemReady,
}

/// Stable snapshot of shared client state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientSnapshot {
    /// Configuration coordination snapshot.
    pub config: ConfigSnapshot,
    /// System status coordination snapshot.
    pub status: StatusSnapshot,
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

/// Stable snapshot of one operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OperationSnapshot {
    /// Stable operation status name.
    pub status: &'static str,
    /// Last operation error, when present.
    pub error: Option<String>,
}

/// Shared reducer plus effect executor loop used by shells.
pub struct ClientRuntime {
    reducer: Client,
    executor: GrpcEffectExecutor,
    status_watch: Option<StatusChangeStream>,
    config_watch: Option<ConfigWatchUpdateStream>,
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
        }
    }

    /// Return the current reducer snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        ClientSnapshot::from_client(&self.reducer)
    }

    /// Convert this runtime into a startup/config state stream.
    #[must_use]
    pub const fn into_state_stream(self) -> ClientStateStream {
        ClientStateStream::new(self)
    }

    /// Dispatch a shell command through the shared reducer/effect loop.
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatched command's effect fails and the
    /// reducer records an operation error.
    pub async fn dispatch<Sleep, SleepFuture>(
        &mut self,
        command: ClientCommand,
        sleep: Sleep,
    ) -> Result<ClientSnapshot, ClientError>
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        match command {
            ClientCommand::RequestConfig => {
                let update = self.reducer.request_config();
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.load.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::AddProvider { provider } => {
                let update = self.reducer.add_provider(provider.into())?;
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.patch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::UpdateProvider {
                original_name,
                provider,
            } => {
                let update = self
                    .reducer
                    .update_provider(&original_name, provider.into())?;
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.patch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::DeleteProvider { name } => {
                let update = self.reducer.delete_provider(&name)?;
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.patch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::AddMcpServer { server } => {
                let update = self.reducer.add_mcp_server(server.into())?;
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.patch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::UpdateMcpServer {
                original_name,
                server,
            } => {
                let update = self
                    .reducer
                    .update_mcp_server(&original_name, server.into())?;
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.patch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::DeleteMcpServer { name } => {
                let update = self.reducer.delete_mcp_server(&name)?;
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.patch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::RegisterConfigWatchInterest => {
                let update = self.reducer.register_config_watch_interest();
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().config.watch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::ReleaseConfigWatchInterest => {
                let update = self.reducer.release_config_watch_interest();
                if self.reducer.state().config.watch.interests == 0 {
                    self.config_watch = None;
                }
                self.run_update(update, sleep).await;
            }
            ClientCommand::WatchStatus => {
                let update = self.reducer.request_status_watch();
                self.run_update(update, sleep).await;

                if let Some(error) = self.reducer.state().status.watch.error.as_deref() {
                    return Err(ClientError::new(error));
                }
            }
            ClientCommand::WaitForSystemReady => {
                while !self
                    .reducer
                    .state()
                    .status
                    .current
                    .is_some_and(|status| status.ready)
                {
                    let update = self.reducer.maintain_watches();
                    self.run_update(update, sleep).await;
                    if !self
                        .reducer
                        .state()
                        .status
                        .current
                        .is_some_and(|status| status.ready)
                    {
                        self.apply_next_watch_event(sleep).await;
                    }
                }
            }
        }

        Ok(self.snapshot())
    }

    async fn run_update<Sleep, SleepFuture>(&mut self, update: ClientUpdate, sleep: Sleep)
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        let mut effects = update.effects().to_vec();

        while let Some(effect) = effects.pop() {
            let events = self.execute_effect(effect, sleep).await;

            for event in events {
                let update = self.reducer.apply(event);
                effects.extend(update.effects().iter().cloned().rev());
            }
        }
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
            effect => self.executor.execute_effect(effect, sleep).await,
        }
    }

    async fn apply_next_watch_event<Sleep, SleepFuture>(&mut self, sleep: Sleep)
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        let event = self.next_watch_event().await;
        let update = self.reducer.apply(event);
        self.run_update(update, sleep).await;
    }

    async fn next_watch_event(&mut self) -> ClientEvent {
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
    const fn new(runtime: ClientRuntime) -> Self {
        Self {
            runtime,
            phase: ClientStateStreamPhase::WaitForSystemReady,
            effects: Vec::new(),
        }
    }

    /// Return the next state snapshot from shared startup/config orchestration.
    pub async fn next<Sleep, SleepFuture>(&mut self, sleep: Sleep) -> ClientSnapshot
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        loop {
            if let Some(effect) = self.effects.pop() {
                self.apply_effect(effect, sleep).await;
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
                    {
                        sleep(READY_IDLE_DELAY_MS).await;
                        let update = self.runtime.reducer.maintain_watches();
                        self.extend_effects(&update);
                        return self.snapshot();
                    }

                    if self.runtime.config_watch.is_some() || self.runtime.status_watch.is_some() {
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
    pub async fn next_or_dispatch<Sleep, SleepFuture, CommandFuture>(
        &mut self,
        command: CommandFuture,
        sleep: Sleep,
    ) -> ClientStateStreamEvent
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
        CommandFuture: Future<Output = Option<ClientCommandRequest>>,
    {
        let request = {
            let next = self.next(sleep);
            pin_mut!(next);
            pin_mut!(command);

            match select(next, command).await {
                Either::Left((snapshot, _)) => {
                    return ClientStateStreamEvent::Snapshot(Box::new(snapshot));
                }
                Either::Right((Some(request), _pending_next)) => request,
                Either::Right((None, _)) => return ClientStateStreamEvent::CommandsClosed,
            }
        };

        ClientStateStreamEvent::Snapshot(Box::new(self.dispatch_request(request, sleep).await))
    }

    /// Return the current stream snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        self.runtime.snapshot()
    }

    /// Dispatch a one-off command against the stream-owned runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when the command cannot be reduced or executed.
    pub async fn dispatch<Sleep, SleepFuture>(
        &mut self,
        command: ClientCommand,
        sleep: Sleep,
    ) -> Result<ClientSnapshot, ClientError>
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        self.runtime.dispatch(command, sleep).await
    }

    async fn dispatch_request<Sleep, SleepFuture>(
        &mut self,
        request: ClientCommandRequest,
        sleep: Sleep,
    ) -> ClientSnapshot
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        let result = self.dispatch(request.command, sleep).await;
        let snapshot = result.clone().unwrap_or_else(|_| self.snapshot());

        if let Some(reply) = request.result {
            drop(reply.send(result));
        }

        snapshot
    }

    async fn apply_effect<Sleep, SleepFuture>(&mut self, effect: ClientEffect, sleep: Sleep)
    where
        Sleep: Copy + FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        let events = self.runtime.execute_effect(effect, sleep).await;

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
            config: ConfigSnapshot {
                current: state.config.current.clone().map(ConfigView::from),
                load: OperationSnapshot::from_parts(
                    state.config.load.status,
                    state.config.load.error.clone(),
                ),
                patch: OperationSnapshot::from_parts(
                    state.config.patch.status,
                    state.config.patch.error.clone(),
                ),
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
        }
    }
}

impl OperationSnapshot {
    const fn from_parts(status: OperationStatus, error: Option<String>) -> Self {
        Self {
            status: operation_status_name(status),
            error,
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
        let mut stream = ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream();

        let snapshot = stream.next(no_sleep).await;

        assert_eq!(snapshot.status.watch.status, "loading");
        assert!(!snapshot.status.ready);
    }

    #[tokio::test]
    async fn state_stream_yields_failed_status_before_reconnect_sleep() {
        let mut stream = ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream();

        drop(stream.next(no_sleep).await);
        let snapshot = stream.next(no_sleep).await;

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
        };

        let snapshot =
            tokio::time::timeout(std::time::Duration::from_millis(10), stream.next(no_sleep))
                .await
                .expect("ready stream should not wait indefinitely for idle status watch");

        assert_eq!(snapshot.status.watch.status, "received");
        assert_eq!(snapshot.config.watch.status, "idle");
    }

    async fn no_sleep(_: u64) {}
}
