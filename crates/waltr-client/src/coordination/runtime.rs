//! Shared reducer runtime used by shell adapters.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::rc::Rc;

use futures_channel::oneshot;
use futures_util::{
    FutureExt, StreamExt,
    future::{Either, LocalBoxFuture, select, select_all},
    pin_mut,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::executor::{
    ConfigWatchUpdateStream, SessionTimelineWatchUpdateStream, SessionWatchUpdateStream,
};
use crate::transport::SessionTimelineWatchUpdate;
use crate::{
    AuthAttemptState, AuthRequirement, AuthTarget, Client, ClientEffect, ClientError, ClientEvent,
    ClientOperationState, ClientUpdate, ConfigEffect, ConfigEvent, ConfigView, GrpcEffectExecutor,
    McpServerView, OperationKey, OperationStatus, ProviderView, RollbackTarget,
    RuntimeDefaultsView, SessionEffect, SessionEvent, SessionRuntimeConfigRequest,
    SessionTemplateKind, SessionTemplateOption, SessionTimelineView, SessionView,
    StatusChangeStream, StatusEffect, StatusEvent, TimelineDirection, TimelineQuery,
    TimelineWatchKey, rollback_target,
};

const READY_IDLE_DELAY_MS: u64 = 1_000;

type ClientSleep = Rc<dyn Fn(u64) -> LocalBoxFuture<'static, ()>>;

/// Command accepted by the shared client runtime.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientCommand {
    /// Load secret-safe auth statuses.
    RequestAuthStatuses,
    /// Dismiss one queued auth requirement.
    DismissAuthRequirement {
        /// Target to dismiss from the queue.
        target: ClientAuthTarget,
    },
    /// Begin manual OAuth for a target.
    BeginAuth {
        /// Target to authenticate.
        target: ClientAuthTarget,
    },
    /// Complete manual OAuth with a pasted redirect URL or code.
    CompleteAuth {
        /// Attempt id returned by begin.
        #[serde(rename = "attemptId")]
        attempt_id: String,
        /// Pasted redirect URL or code.
        input: String,
    },
    /// Cancel a manual OAuth attempt.
    CancelAuth {
        /// Attempt id returned by begin.
        #[serde(rename = "attemptId")]
        attempt_id: String,
    },
    /// Delete stored credentials for a target.
    LogoutAuth {
        /// Target to log out.
        target: ClientAuthTarget,
    },
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
    /// Replace runtime defaults for newly created sessions.
    SetRuntimeDefaults {
        /// Runtime defaults to store, or null to clear defaults.
        defaults: Option<RuntimeDefaultsView>,
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
    /// Replace runtime config metadata for a server-side session.
    UpdateSessionRuntimeConfig {
        /// Session id to update.
        id: String,
        /// Provider key to use on subsequent prompts.
        #[serde(rename = "providerKey")]
        provider_key: String,
        /// Model id to use on subsequent prompts.
        #[serde(rename = "modelId")]
        model_id: String,
        /// MCP server keys enabled for subsequent prompts.
        #[serde(rename = "enabledMcpServerKeys")]
        enabled_mcp_server_keys: Vec<String>,
        /// Optional per-session reasoning effort.
        #[serde(rename = "reasoningEffort")]
        reasoning_effort: Option<String>,
    },
    /// Register current interest in session watch updates.
    RegisterSessionWatchInterest,
    /// Release current interest in session watch updates.
    ReleaseSessionWatchInterest,
    /// Load a page of timeline events for one session.
    RequestSessionTimeline {
        /// Session id whose timeline should load.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Direction from the exclusive cursor boundary.
        direction: ClientTimelineDirection,
        /// Exclusive cursor boundary.
        cursor: Option<u64>,
        /// Maximum events to return.
        limit: u32,
        /// Display-channel filter.
        channels: Vec<String>,
    },
    /// Search templates visible to one session.
    SearchSessionTemplates {
        /// Runtime-local caller request id.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Session id whose visible templates should be searched.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Search query.
        query: String,
        /// Maximum templates to return.
        limit: u32,
        /// Template kinds to include.
        kinds: Vec<ClientSessionTemplateKind>,
        /// Template search target.
        target: ClientSessionTemplateSearchTarget,
        /// Cursor byte offset relative to query for context-aware completions.
        cursor: Option<u32>,
    },
    /// Register interest in timeline updates for one session.
    RegisterSessionTimelineWatchInterest {
        /// Session id whose timeline should be watched.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Optional cursor boundary for replay.
        #[serde(rename = "afterCursor")]
        after_cursor: Option<u64>,
        /// Display-channel filter.
        channels: Vec<String>,
        /// Optional history batch size.
        #[serde(rename = "initialLimit")]
        initial_limit: Option<u32>,
    },
    /// Release interest in timeline updates for one session.
    ReleaseSessionTimelineWatchInterest {
        /// Session id whose timeline watch interest should be released.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Submit a user message to a session.
    SubmitUserMessage {
        /// Session id to submit to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// User message text.
        text: String,
    },
    /// Respond to an external request.
    RespondExternalRequest {
        /// Session id to respond to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// External request id.
        #[serde(rename = "requestId")]
        request_id: String,
        /// JSON response payload.
        #[serde(rename = "responseJson")]
        response_json: String,
    },
    /// Interrupt an active session run.
    InterruptSession {
        /// Session id to interrupt.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Roll back a session to the end of a turn.
    RollbackSessionToTurn {
        /// Session id to roll back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Turn to preserve.
        turn: u64,
    },
    /// Roll back a session to a timeline cursor.
    RollbackSessionToCursor {
        /// Session id to roll back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Cursor to preserve.
        cursor: u64,
    },
    /// Roll back a session to immediately before a timeline cursor.
    RollbackSessionBeforeCursor {
        /// Session id to roll back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Cursor to remove.
        cursor: u64,
    },
    /// Roll back to a turn and continue from that checkpoint.
    ContinueSessionFromTurn {
        /// Session id to continue.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Turn to continue from.
        turn: u64,
    },
    /// Roll back to a timeline cursor and continue from that checkpoint.
    ContinueSessionFromCursor {
        /// Session id to continue.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Cursor to continue from.
        cursor: u64,
    },
    /// Process one status watch update.
    WatchStatus,
    /// Keep watching status until the reducer observes readiness.
    WaitForSystemReady,
}

/// Stable UI-facing auth target.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientAuthTarget {
    /// Target kind, currently `provider` or `mcpServer`.
    pub kind: String,
    /// Config key for the target.
    pub key: String,
}

/// Direction for loading a timeline page from an exclusive cursor boundary.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientTimelineDirection {
    /// Load events after the cursor.
    Forward,
    /// Load events before the cursor.
    Backward,
}

/// Template kind requested by UI clients.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientSessionTemplateKind {
    /// File templates visible to the session.
    File,
    /// Tool templates visible to the session.
    Tool,
    /// MCP resource templates visible to the session.
    Resource,
    /// Prompt templates visible to the session.
    Prompt,
}

/// Template search target requested by UI clients.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientSessionTemplateSearchTarget {
    /// Search file and tool reference templates.
    Resources,
    /// Search prompt templates such as skills or MCP prompts.
    Prompts,
}

impl Default for ClientSessionTemplateSearchTarget {
    fn default() -> Self {
        Self::Resources
    }
}

/// Stable snapshot of shared client state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientSnapshot {
    /// Auth coordination snapshot.
    pub auth: AuthSnapshot,
    /// Command operation result snapshots.
    pub operations: OperationCollectionSnapshot,
    /// Configuration coordination snapshot.
    pub config: ConfigSnapshot,
    /// System status coordination snapshot.
    pub status: StatusSnapshot,
    /// Session coordination snapshot.
    pub sessions: SessionSnapshot,
}

/// Stable auth coordination snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthSnapshot {
    /// Pending auth requirements in modal display order.
    pub pending: Vec<AuthRequirementSnapshot>,
    /// Active manual auth attempt, when one has started.
    #[serde(rename = "activeAttempt")]
    pub active_attempt: Option<AuthAttemptSnapshot>,
    /// Auth status list operation state.
    pub list: OperationSnapshot,
    /// Begin auth operation state.
    pub begin: OperationSnapshot,
    /// Complete auth operation state.
    pub complete: OperationSnapshot,
    /// Cancel auth operation state.
    pub cancel: OperationSnapshot,
    /// Logout operation state.
    pub logout: OperationSnapshot,
}

/// One pending auth requirement.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthRequirementSnapshot {
    /// Target that requires authentication.
    pub target: ClientAuthTarget,
    /// Human-readable reason.
    pub message: String,
    /// Session whose work reported the requirement.
    #[serde(rename = "sourceSessionId")]
    pub source_session_id: Option<String>,
}

/// Active manual auth attempt details.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthAttemptSnapshot {
    /// Target being authenticated.
    pub target: ClientAuthTarget,
    /// Attempt id returned by the server.
    #[serde(rename = "attemptId")]
    pub attempt_id: String,
    /// URL to open for browser-based auth.
    #[serde(rename = "authUrl")]
    pub auth_url: String,
    /// Verification URL for device-style auth, when supplied.
    #[serde(rename = "verificationUrl")]
    pub verification_url: String,
    /// User code for device-style auth, when supplied.
    #[serde(rename = "userCode")]
    pub user_code: String,
    /// Attempt expiration timestamp in milliseconds since epoch.
    #[serde(rename = "expiresAtMs")]
    pub expires_at_ms: i64,
    /// Poll interval for device-style auth.
    #[serde(rename = "pollIntervalSeconds")]
    pub poll_interval_seconds: i32,
    /// User-facing instructions returned by the server.
    pub messages: Vec<String>,
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
    /// Begin auth command result.
    AuthBegin {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target being authenticated.
        target: ClientAuthTarget,
        /// Attempt details returned by the server.
        attempt: Option<AuthAttemptSnapshot>,
    },
    /// Complete auth command result.
    AuthComplete {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
    },
    /// Cancel auth command result.
    AuthCancel {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
    },
    /// Logout command result.
    AuthLogout {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target logged out.
        target: ClientAuthTarget,
    },
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
    /// Session runtime metadata update command result.
    SessionRuntimeConfigUpdate {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// User message submit command result.
    SessionSubmitUserMessage {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id submitted to.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// External request response command result.
    SessionRespondExternalRequest {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id responded to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// External request id.
        #[serde(rename = "requestId")]
        request_id: String,
    },
    /// Session interrupt command result.
    SessionInterrupt {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id interrupted.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Session rollback command result.
    SessionRollback {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id rolled back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// Session continue command result.
    SessionContinue {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id continued.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Rollback target continued from.
        target: RollbackTargetView,
    },
}

/// Stable shell-facing rollback target view.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RollbackTargetView {
    /// Turn target, when continuing or rolling back by turn.
    pub turn: Option<u64>,
    /// Timeline cursor target, when continuing or rolling back by cursor.
    pub cursor: Option<u64>,
}

impl From<&RollbackTarget> for RollbackTargetView {
    fn from(target: &RollbackTarget) -> Self {
        match target.target {
            Some(rollback_target::Target::Turn(turn)) => Self {
                turn: Some(turn),
                cursor: None,
            },
            Some(rollback_target::Target::Cursor(cursor)) => Self {
                turn: None,
                cursor: Some(cursor),
            },
            Some(rollback_target::Target::BeforeCursor(cursor)) => Self {
                turn: None,
                cursor: Some(cursor),
            },
            None => Self {
                turn: None,
                cursor: None,
            },
        }
    }
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
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
    /// Session runtime metadata update operation snapshot.
    pub runtime_config_update: OperationSnapshot,
    /// User message submit operation snapshot.
    pub submit_user_message: OperationSnapshot,
    /// External request response operation snapshot.
    pub respond_external_request: OperationSnapshot,
    /// Session interrupt operation snapshot.
    pub interrupt: OperationSnapshot,
    /// Session rollback operation snapshot.
    pub rollback: OperationSnapshot,
    /// Session continue operation snapshot.
    pub continue_session: OperationSnapshot,
    /// Session watch operation snapshot.
    pub watch: OperationSnapshot,
    /// Session timelines by session id.
    pub timelines: BTreeMap<String, SessionTimelineView>,
    /// Latest resource search state by session id.
    pub template_searches: BTreeMap<String, SessionTemplateSearchSnapshot>,
}

/// Stable snapshot of resource search state for one session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionTemplateSearchSnapshot {
    /// Runtime-local caller request id.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Session id searched.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Search query.
    pub query: String,
    /// Stable operation status name.
    pub status: &'static str,
    /// Last search error, when present.
    pub error: Option<String>,
    /// Search options returned by the server.
    pub options: Vec<SessionTemplateOptionView>,
}

/// Stable snapshot of one session resource option.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionTemplateOptionView {
    /// Template kind name.
    pub kind: &'static str,
    /// Human-readable option label.
    pub label: String,
    /// Human-readable option detail.
    pub detail: String,
    /// Template text to insert when selected.
    pub template: String,
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
    timeline_watches: BTreeMap<TimelineWatchKey, SessionTimelineWatchUpdateStream>,
    watch_poll_offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WatchCandidate {
    Status,
    Config,
    Session,
    Timeline(TimelineWatchKey),
}

enum WatchResult {
    Status(Option<Result<crate::SystemStatus, ClientError>>),
    Config(Option<Result<crate::ConfigWatchUpdate, ClientError>>),
    Session(Option<Result<crate::SessionWatchUpdate, ClientError>>),
    Timeline(
        TimelineWatchKey,
        Option<Result<SessionTimelineWatchUpdate, ClientError>>,
    ),
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
            .field("timeline_watch_count", &self.timeline_watches.len())
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
            timeline_watches: BTreeMap::new(),
            watch_poll_offset: 0,
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
            ClientEffect::Session(SessionEffect::WatchTimeline { key }) => {
                match self
                    .executor
                    .watch_session_timeline_updates(
                        key.session_id.clone(),
                        key.after_cursor,
                        key.channels.clone(),
                        key.initial_limit,
                    )
                    .await
                {
                    Ok(stream) => {
                        self.timeline_watches.insert(key.clone(), stream);
                        vec![ClientEvent::Session(SessionEvent::TimelineWatchStarted {
                            key,
                        })]
                    }
                    Err(error) => vec![ClientEvent::Session(SessionEvent::TimelineWatchFailed {
                        key,
                        error,
                    })],
                }
            }
            effect => self.executor.execute_effect(effect, sleep).await,
        }
    }

    async fn next_watch_event(&mut self) -> ClientEvent {
        if let Some(result) = self.try_ready_watch_result() {
            return self.watch_result_event(result);
        }

        let result = self.next_pending_watch_result().await;
        self.watch_result_event(result)
    }

    fn active_watch_candidates(&self) -> Vec<WatchCandidate> {
        let mut candidates = Vec::new();

        if self.status_watch.is_some() {
            candidates.push(WatchCandidate::Status);
        }

        if self.reducer.state().config.watch.interests > 0 && self.config_watch.is_some() {
            candidates.push(WatchCandidate::Config);
        }

        if self.reducer.state().sessions.watch.interests > 0 && self.session_watch.is_some() {
            candidates.push(WatchCandidate::Session);
        }

        candidates.extend(
            self.reducer
                .state()
                .timeline_watch_keys()
                .into_iter()
                .filter(|key| self.timeline_watches.contains_key(key))
                .map(WatchCandidate::Timeline),
        );

        if !candidates.is_empty() {
            let offset = self.watch_poll_offset % candidates.len();
            candidates.rotate_left(offset);
        }

        candidates
    }

    fn try_ready_watch_result(&mut self) -> Option<WatchResult> {
        for candidate in self.active_watch_candidates() {
            let result = match candidate {
                WatchCandidate::Status => self
                    .status_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::Status),
                WatchCandidate::Config => self
                    .config_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::Config),
                WatchCandidate::Session => self
                    .session_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::Session),
                WatchCandidate::Timeline(key) => self
                    .timeline_watches
                    .get_mut(&key)
                    .and_then(|stream| stream.next().now_or_never())
                    .map(|result| WatchResult::Timeline(key, result)),
            };

            if result.is_some() {
                self.watch_poll_offset = self.watch_poll_offset.saturating_add(1);
                return result;
            }
        }

        None
    }

    async fn next_pending_watch_result(&mut self) -> WatchResult {
        let mut futures: Vec<LocalBoxFuture<'_, WatchResult>> = Vec::new();

        if let Some(stream) = &mut self.status_watch {
            futures.push(Box::pin(
                async move { WatchResult::Status(stream.next().await) },
            ));
        }

        if self.reducer.state().config.watch.interests > 0
            && let Some(stream) = &mut self.config_watch
        {
            futures.push(Box::pin(
                async move { WatchResult::Config(stream.next().await) },
            ));
        }

        if self.reducer.state().sessions.watch.interests > 0
            && let Some(stream) = &mut self.session_watch
        {
            futures.push(Box::pin(async move {
                WatchResult::Session(stream.next().await)
            }));
        }

        let active_timeline_keys = self
            .reducer
            .state()
            .timeline_watch_keys()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        for (key, stream) in self
            .timeline_watches
            .iter_mut()
            .filter(|(key, _)| active_timeline_keys.contains(*key))
        {
            let key = key.clone();
            futures.push(Box::pin(async move {
                WatchResult::Timeline(key, stream.next().await)
            }));
        }

        if futures.is_empty() {
            return WatchResult::Status(Some(Ok(crate::SystemStatus::default())));
        }

        let (result, index, _) = select_all(futures).await;
        self.watch_poll_offset = self.watch_poll_offset.saturating_add(index + 1);
        result
    }

    fn watch_result_event(&mut self, result: WatchResult) -> ClientEvent {
        match result {
            WatchResult::Status(Some(Ok(status))) => {
                ClientEvent::Status(StatusEvent::WatchChanged(status))
            }
            WatchResult::Status(Some(Err(error))) => {
                self.status_watch = None;
                ClientEvent::Status(StatusEvent::WatchFailed(error))
            }
            WatchResult::Status(None) => {
                self.status_watch = None;
                ClientEvent::Status(StatusEvent::WatchEnded)
            }
            WatchResult::Config(Some(Ok(update))) => {
                ClientEvent::Config(ConfigEvent::WatchChanged(update))
            }
            WatchResult::Config(Some(Err(error))) => {
                self.config_watch = None;
                ClientEvent::Config(ConfigEvent::WatchFailed(error))
            }
            WatchResult::Config(None) => {
                self.config_watch = None;
                ClientEvent::Config(ConfigEvent::WatchEnded)
            }
            WatchResult::Session(Some(Ok(update))) => {
                ClientEvent::Session(SessionEvent::WatchChanged(update))
            }
            WatchResult::Session(Some(Err(error))) => {
                self.session_watch = None;
                ClientEvent::Session(SessionEvent::WatchFailed(error))
            }
            WatchResult::Session(None) => {
                self.session_watch = None;
                ClientEvent::Session(SessionEvent::WatchEnded)
            }
            WatchResult::Timeline(key, Some(Ok(update))) => {
                ClientEvent::Session(SessionEvent::TimelineWatchChanged { key, update })
            }
            WatchResult::Timeline(key, Some(Err(error))) => {
                self.timeline_watches.remove(&key);
                ClientEvent::Session(SessionEvent::TimelineWatchFailed { key, error })
            }
            WatchResult::Timeline(key, None) => {
                self.timeline_watches.remove(&key);
                ClientEvent::Session(SessionEvent::TimelineWatchEnded { key })
            }
        }
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
            .finish_non_exhaustive()
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
                        && self
                            .runtime
                            .reducer
                            .state()
                            .timeline_watch_keys()
                            .is_empty()
                    {
                        (self.sleep)(READY_IDLE_DELAY_MS).await;
                        let update = self.runtime.reducer.maintain_watches();
                        self.extend_effects(&update);
                        return self.snapshot();
                    }

                    if self.runtime.config_watch.is_some()
                        || self.runtime.session_watch.is_some()
                        || !self.runtime.timeline_watches.is_empty()
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

        ClientStateStreamEvent::Snapshot(Box::new(self.dispatch_request(request)))
    }

    /// Return the current stream snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        self.runtime.snapshot()
    }

    fn dispatch_request(&mut self, request: ClientCommandRequest) -> ClientSnapshot {
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
            ClientCommand::RequestAuthStatuses
            | ClientCommand::DismissAuthRequirement { .. }
            | ClientCommand::BeginAuth { .. }
            | ClientCommand::CompleteAuth { .. }
            | ClientCommand::CancelAuth { .. }
            | ClientCommand::LogoutAuth { .. } => self.begin_auth_command(command),
            ClientCommand::RequestConfig
            | ClientCommand::AddProvider { .. }
            | ClientCommand::UpdateProvider { .. }
            | ClientCommand::DeleteProvider { .. }
            | ClientCommand::AddMcpServer { .. }
            | ClientCommand::UpdateMcpServer { .. }
            | ClientCommand::DeleteMcpServer { .. }
            | ClientCommand::SetRuntimeDefaults { .. }
            | ClientCommand::RegisterConfigWatchInterest
            | ClientCommand::ReleaseConfigWatchInterest => self.begin_config_command(command)?,
            ClientCommand::CreateSession
            | ClientCommand::DeleteSession { .. }
            | ClientCommand::UpdateSessionRuntimeConfig { .. }
            | ClientCommand::SearchSessionTemplates { .. }
            | ClientCommand::RegisterSessionWatchInterest
            | ClientCommand::ReleaseSessionWatchInterest
            | ClientCommand::SubmitUserMessage { .. }
            | ClientCommand::RespondExternalRequest { .. }
            | ClientCommand::InterruptSession { .. }
            | ClientCommand::RollbackSessionToTurn { .. }
            | ClientCommand::RollbackSessionToCursor { .. }
            | ClientCommand::RollbackSessionBeforeCursor { .. }
            | ClientCommand::ContinueSessionFromTurn { .. }
            | ClientCommand::ContinueSessionFromCursor { .. } => {
                self.begin_session_command(command)
            }
            ClientCommand::RequestSessionTimeline { .. }
            | ClientCommand::RegisterSessionTimelineWatchInterest { .. }
            | ClientCommand::ReleaseSessionTimelineWatchInterest { .. } => {
                self.begin_timeline_command(command)
            }
            ClientCommand::WatchStatus => self.runtime.reducer.request_status_watch(),
            ClientCommand::WaitForSystemReady => self.runtime.reducer.maintain_watches(),
        };

        self.extend_effects(&update);
        Ok(())
    }

    fn begin_auth_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::RequestAuthStatuses => self.runtime.reducer.request_auth_statuses(),
            ClientCommand::DismissAuthRequirement { target } => self
                .runtime
                .reducer
                .dismiss_auth_requirement(target.into_auth_target()),
            ClientCommand::BeginAuth { target } => {
                self.runtime.reducer.begin_auth(target.into_auth_target())
            }
            ClientCommand::CompleteAuth { attempt_id, input } => {
                self.runtime.reducer.complete_auth(attempt_id, input)
            }
            ClientCommand::CancelAuth { attempt_id } => {
                self.runtime.reducer.cancel_auth(attempt_id)
            }
            ClientCommand::LogoutAuth { target } => {
                self.runtime.reducer.logout_auth(target.into_auth_target())
            }
            _ => unreachable!("non-auth command routed to auth handler"),
        }
    }

    fn begin_config_command(
        &mut self,
        command: ClientCommand,
    ) -> Result<ClientUpdate, ClientError> {
        Ok(match command {
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
            ClientCommand::SetRuntimeDefaults { defaults } => self
                .runtime
                .reducer
                .set_runtime_defaults(defaults.map(Into::into))?,
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
            _ => unreachable!("non-config command routed to config handler"),
        })
    }

    fn begin_session_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::CreateSession => self.runtime.reducer.create_session(),
            ClientCommand::DeleteSession { id } => self.runtime.reducer.delete_session(id),
            ClientCommand::UpdateSessionRuntimeConfig {
                id,
                provider_key,
                model_id,
                enabled_mcp_server_keys,
                reasoning_effort,
            } => self.runtime.reducer.update_session_runtime_config(
                id,
                SessionRuntimeConfigRequest {
                    provider_key,
                    model_id,
                    enabled_mcp_server_keys,
                    reasoning_effort,
                },
            ),
            ClientCommand::SearchSessionTemplates {
                request_id,
                session_id,
                query,
                limit,
                kinds,
                target,
                cursor,
            } => self.runtime.reducer.search_session_templates(
                request_id, session_id, query, limit, kinds, target, cursor,
            ),
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
            ClientCommand::SubmitUserMessage { session_id, text } => {
                self.runtime.reducer.submit_user_message(session_id, text)
            }
            ClientCommand::RespondExternalRequest {
                session_id,
                request_id,
                response_json,
            } => {
                self.runtime
                    .reducer
                    .respond_external_request(session_id, request_id, response_json)
            }
            ClientCommand::InterruptSession { session_id } => {
                self.runtime.reducer.interrupt_session(session_id)
            }
            ClientCommand::RollbackSessionToTurn { session_id, turn } => {
                self.runtime.reducer.rollback_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Turn(turn)),
                    },
                )
            }
            ClientCommand::RollbackSessionToCursor { session_id, cursor } => {
                self.runtime.reducer.rollback_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Cursor(cursor)),
                    },
                )
            }
            ClientCommand::RollbackSessionBeforeCursor { session_id, cursor } => {
                self.runtime.reducer.rollback_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::BeforeCursor(cursor)),
                    },
                )
            }
            ClientCommand::ContinueSessionFromTurn { session_id, turn } => {
                self.runtime.reducer.continue_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Turn(turn)),
                    },
                )
            }
            ClientCommand::ContinueSessionFromCursor { session_id, cursor } => {
                self.runtime.reducer.continue_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Cursor(cursor)),
                    },
                )
            }
            _ => unreachable!("non-session command routed to session handler"),
        }
    }

    fn begin_timeline_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::RequestSessionTimeline {
                session_id,
                direction,
                cursor,
                limit,
                channels,
            } => self.runtime.reducer.request_session_timeline(
                session_id,
                Some(TimelineQuery {
                    direction: timeline_direction(direction) as i32,
                    cursor,
                    limit,
                    channels,
                }),
            ),
            ClientCommand::RegisterSessionTimelineWatchInterest {
                session_id,
                after_cursor,
                channels,
                initial_limit,
            } => self
                .runtime
                .reducer
                .register_session_timeline_watch_interest(
                    session_id,
                    after_cursor,
                    channels,
                    initial_limit,
                ),
            ClientCommand::ReleaseSessionTimelineWatchInterest { session_id } => {
                let update = self
                    .runtime
                    .reducer
                    .release_session_timeline_watch_interest(session_id.clone());
                let active = self
                    .runtime
                    .reducer
                    .state()
                    .sessions
                    .timelines
                    .get(&session_id)
                    .is_some_and(|timeline| timeline.watch.interests > 0);
                if !active {
                    self.runtime
                        .timeline_watches
                        .retain(|key, _| key.session_id != session_id);
                }
                update
            }
            _ => unreachable!("non-timeline command routed to timeline handler"),
        }
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

const fn timeline_direction(direction: ClientTimelineDirection) -> TimelineDirection {
    match direction {
        ClientTimelineDirection::Forward => TimelineDirection::Forward,
        ClientTimelineDirection::Backward => TimelineDirection::Backward,
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
            auth: AuthSnapshot {
                pending: state
                    .auth
                    .pending
                    .iter()
                    .cloned()
                    .map(AuthRequirementSnapshot::from)
                    .collect(),
                active_attempt: state
                    .auth
                    .active_attempt
                    .clone()
                    .map(AuthAttemptSnapshot::from),
                list: OperationSnapshot::from_state(&state.auth.list),
                begin: OperationSnapshot::from_state(&state.auth.begin),
                complete: OperationSnapshot::from_state(&state.auth.complete),
                cancel: OperationSnapshot::from_state(&state.auth.cancel),
                logout: OperationSnapshot::from_state(&state.auth.logout),
            },
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
                runtime_config_update: OperationSnapshot::from_state(
                    &state.sessions.runtime_config_update,
                ),
                submit_user_message: OperationSnapshot::from_state(
                    &state.sessions.submit_user_message,
                ),
                respond_external_request: OperationSnapshot::from_state(
                    &state.sessions.respond_external_request,
                ),
                interrupt: OperationSnapshot::from_state(&state.sessions.interrupt),
                rollback: OperationSnapshot::from_state(&state.sessions.rollback),
                continue_session: OperationSnapshot::from_state(&state.sessions.continue_session),
                watch: OperationSnapshot::from_parts(
                    state.sessions.watch.status,
                    state.sessions.watch.error.clone(),
                ),
                timelines: state
                    .sessions
                    .timelines
                    .iter()
                    .map(|(session_id, timeline)| {
                        (
                            session_id.clone(),
                            SessionTimelineView {
                                events: timeline.events.values().cloned().map(Into::into).collect(),
                                load: OperationSnapshot::from_state(&timeline.load),
                                watch: OperationSnapshot::from_parts(
                                    timeline.watch.status,
                                    timeline.watch.error.clone(),
                                ),
                            },
                        )
                    })
                    .collect(),
                template_searches: state
                    .sessions
                    .template_searches
                    .iter()
                    .map(|(session_id, search)| {
                        (
                            session_id.clone(),
                            SessionTemplateSearchSnapshot {
                                request_id: search.request_id.clone(),
                                session_id: search.session_id.clone(),
                                query: search.query.clone(),
                                status: operation_status_name(search.status),
                                error: search.error.clone(),
                                options: search
                                    .options
                                    .iter()
                                    .cloned()
                                    .map(SessionTemplateOptionView::from)
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
            },
        }
    }
}

impl From<SessionTemplateOption> for SessionTemplateOptionView {
    fn from(option: SessionTemplateOption) -> Self {
        Self {
            kind: match SessionTemplateKind::try_from(option.kind).ok() {
                Some(SessionTemplateKind::File) => "file",
                Some(SessionTemplateKind::Tool) => "tool",
                Some(SessionTemplateKind::Prompt) => "prompt",
                _ => "unknown",
            },
            label: option.label,
            detail: option.detail,
            template: option.template,
        }
    }
}

impl ClientAuthTarget {
    fn into_auth_target(self) -> AuthTarget {
        let target = match self.kind.as_str() {
            "provider" => waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(self.key),
            "mcpServer" | "mcp_server" => {
                waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(self.key)
            }
            _ => waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(self.key),
        };

        AuthTarget {
            target: Some(target),
        }
    }
}

impl From<AuthTarget> for ClientAuthTarget {
    fn from(target: AuthTarget) -> Self {
        match target.target {
            Some(waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(key)) => Self {
                kind: "provider".to_owned(),
                key,
            },
            Some(waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(key)) => Self {
                kind: "mcpServer".to_owned(),
                key,
            },
            None => Self {
                kind: "unknown".to_owned(),
                key: String::new(),
            },
        }
    }
}

impl From<AuthRequirement> for AuthRequirementSnapshot {
    fn from(requirement: AuthRequirement) -> Self {
        Self {
            target: ClientAuthTarget::from(requirement.target),
            message: requirement.message,
            source_session_id: requirement.source_session_id,
        }
    }
}

impl From<AuthAttemptState> for AuthAttemptSnapshot {
    fn from(attempt: AuthAttemptState) -> Self {
        Self {
            target: ClientAuthTarget::from(attempt.target),
            attempt_id: attempt.attempt_id,
            auth_url: attempt.auth_url,
            verification_url: attempt.verification_url,
            user_code: attempt.user_code,
            expires_at_ms: attempt.expires_at_ms,
            poll_interval_seconds: attempt.poll_interval_seconds,
            messages: attempt.messages,
        }
    }
}

impl From<ClientOperationState> for ClientOperationView {
    fn from(operation: ClientOperationState) -> Self {
        match operation {
            ClientOperationState::AuthBegin {
                status,
                error,
                target,
                attempt,
            } => Self::AuthBegin {
                status: operation_status_name(status),
                error,
                target: ClientAuthTarget::from(target),
                attempt: attempt.map(AuthAttemptSnapshot::from),
            },
            ClientOperationState::AuthComplete { status, error, .. } => Self::AuthComplete {
                status: operation_status_name(status),
                error,
            },
            ClientOperationState::AuthCancel { status, error } => Self::AuthCancel {
                status: operation_status_name(status),
                error,
            },
            ClientOperationState::AuthLogout {
                status,
                error,
                target,
                ..
            } => Self::AuthLogout {
                status: operation_status_name(status),
                error,
                target: ClientAuthTarget::from(target),
            },
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
            ClientOperationState::SessionRuntimeConfigUpdate {
                status,
                error,
                session,
            } => Self::SessionRuntimeConfigUpdate {
                status: operation_status_name(status),
                error,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionSubmitUserMessage {
                status,
                error,
                session_id,
            } => Self::SessionSubmitUserMessage {
                status: operation_status_name(status),
                error,
                session_id,
            },
            ClientOperationState::SessionRespondExternalRequest {
                status,
                error,
                session_id,
                request_id,
            } => Self::SessionRespondExternalRequest {
                status: operation_status_name(status),
                error,
                session_id,
                request_id,
            },
            ClientOperationState::SessionInterrupt {
                status,
                error,
                session_id,
            } => Self::SessionInterrupt {
                status: operation_status_name(status),
                error,
                session_id,
            },
            ClientOperationState::SessionRollback {
                status,
                error,
                session_id,
                session,
            } => Self::SessionRollback {
                status: operation_status_name(status),
                error,
                session_id,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionContinue {
                status,
                error,
                session_id,
                target,
            } => Self::SessionContinue {
                status: operation_status_name(status),
                error,
                session_id,
                target: RollbackTargetView::from(&target),
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

    use crate::TimelineEvent;
    use crate::transport::{
        ConfigWatchUpdate, SessionTimelineWatchEvent, SessionTimelineWatchUpdate,
    };

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
    async fn busy_timeline_watch_does_not_starve_status_watch_failures() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        let key = TimelineWatchKey::new("session".to_owned(), Some(0), Vec::new(), None);
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime.reducer.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestRegistered {
                session_id: key.session_id.clone(),
                after_cursor: key.after_cursor,
                channels: key.channels.clone(),
                initial_limit: key.initial_limit,
            },
        ));
        runtime
            .reducer
            .apply(ClientEvent::Session(SessionEvent::TimelineWatchStarted {
                key: key.clone(),
            }));
        runtime.status_watch = Some(Box::pin(stream::once(async {
            Err(ClientError::new("status transport lost"))
        })));
        runtime.timeline_watches.insert(
            key,
            Box::pin(stream::iter((1..=16).map(|cursor| {
                Ok(SessionTimelineWatchUpdate {
                    sequence: cursor,
                    event: SessionTimelineWatchEvent::Event(timeline_event("session", cursor)),
                })
            }))),
        );

        let event = runtime.next_watch_event().await;
        let update = runtime.reducer.apply(event);
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.status.watch.status, "failed");
        assert!(!snapshot.status.ready);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::SleepReconnect { .. })]
        ));
    }

    #[tokio::test]
    async fn multiple_ready_timeline_watches_are_polled_fairly() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        let first =
            TimelineWatchKey::new("session".to_owned(), Some(0), vec!["a".to_owned()], None);
        let second =
            TimelineWatchKey::new("session".to_owned(), Some(0), vec!["b".to_owned()], None);
        register_timeline_watch(&mut runtime, &first);
        register_timeline_watch(&mut runtime, &second);
        runtime.timeline_watches.insert(
            first,
            Box::pin(stream::iter((1..=16).map(|cursor| {
                Ok(SessionTimelineWatchUpdate {
                    sequence: cursor,
                    event: SessionTimelineWatchEvent::Event(timeline_event("session", cursor)),
                })
            }))),
        );
        runtime.timeline_watches.insert(
            second,
            Box::pin(stream::once(async {
                Ok(SessionTimelineWatchUpdate {
                    sequence: 100,
                    event: SessionTimelineWatchEvent::Event(timeline_event("session", 100)),
                })
            })),
        );

        let first_event = runtime.next_watch_event().await;
        runtime.reducer.apply(first_event);
        let second_event = runtime.next_watch_event().await;
        runtime.reducer.apply(second_event);
        let timeline = runtime
            .reducer
            .state()
            .sessions
            .timelines
            .get("session")
            .expect("timeline should exist");

        assert!(timeline.events.contains_key(&1));
        assert!(timeline.events.contains_key(&100));
    }

    #[tokio::test]
    async fn config_transport_loss_reconnects_config_without_failing_status() {
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

        assert_eq!(snapshot.status.watch.status, "received");
        assert!(snapshot.status.ready);
        assert_eq!(snapshot.config.watch.status, "failed");
        assert!(matches!(
            update.effects(),
            [ClientEffect::Config(ConfigEffect::SleepReconnect {
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

    #[test]
    fn request_timeline_command_uses_requested_direction() {
        let mut runtime = ClientStateStream {
            runtime: ClientRuntime::for_endpoint("http://127.0.0.1:0"),
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };

        let update = runtime.begin_timeline_command(ClientCommand::RequestSessionTimeline {
            session_id: "session".to_owned(),
            direction: ClientTimelineDirection::Backward,
            cursor: Some(42),
            limit: 25,
            channels: vec!["tool_result".to_owned()],
        });

        assert!(matches!(
            update.effects(),
            [ClientEffect::Session(SessionEffect::GetTimeline { query, .. })]
                if query.as_ref().is_some_and(|query| {
                    query.direction == TimelineDirection::Backward as i32
                        && query.cursor == Some(42)
                        && query.limit == 25
                        && query.channels == ["tool_result"]
                })
        ));
    }

    async fn no_sleep(_: u64) {}

    fn register_timeline_watch(runtime: &mut ClientRuntime, key: &TimelineWatchKey) {
        runtime.reducer.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestRegistered {
                session_id: key.session_id.clone(),
                after_cursor: key.after_cursor,
                channels: key.channels.clone(),
                initial_limit: key.initial_limit,
            },
        ));
        runtime
            .reducer
            .apply(ClientEvent::Session(SessionEvent::TimelineWatchStarted {
                key: key.clone(),
            }));
    }

    fn timeline_event(session_id: &str, cursor: u64) -> TimelineEvent {
        TimelineEvent {
            session_id: session_id.to_owned(),
            cursor,
            item_id: format!("item-{cursor}"),
            ..TimelineEvent::default()
        }
    }
}
