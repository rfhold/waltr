//! Shell-neutral client events and side-effect intents.

use crate::transport::{ConfigWatchUpdate, SessionTimelineWatchUpdate, SessionWatchUpdate};
use crate::{
    ClientError, Configuration, OperationKey, Provider, RuntimeDefaults,
    SessionRuntimeConfigRequest, SessionSummary, SystemStatus, TimelineEvent, TimelineQuery,
    TimelineWatchKey,
};

/// Runtime-local identifier for a streamed response.
pub type StreamId = u64;

/// Event applied to the headless client state machine.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ClientEvent {
    /// Configuration coordination event.
    Config(ConfigEvent),
    /// Status coordination event.
    Status(StatusEvent),
    /// Session coordination event.
    Session(SessionEvent),
    /// A model or tool stream coordination event.
    Stream(StreamEvent),
}

/// Configuration coordination event applied to the reducer.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigEvent {
    /// A caller requested the current configuration snapshot.
    LoadRequested,
    /// The current configuration loaded successfully.
    Loaded(Configuration),
    /// The current configuration failed to load.
    LoadFailed(ClientError),
    /// A caller requested a configuration patch.
    PatchRequested(OperationKey),
    /// The configuration patch completed successfully.
    Patched {
        /// Operation key allocated when the patch was requested.
        operation_key: OperationKey,
        /// Patched configuration returned by the server.
        config: Configuration,
    },
    /// The configuration patch failed.
    PatchFailed {
        /// Operation key allocated when the patch was requested.
        operation_key: OperationKey,
        /// Failure returned by the patch effect.
        error: ClientError,
    },
    /// A caller registered interest in configuration watch updates.
    WatchInterestRegistered,
    /// A caller released interest in configuration watch updates.
    WatchInterestReleased,
    /// The configuration watch stream opened.
    WatchStarted,
    /// The configuration watch stream produced an update.
    WatchChanged(ConfigWatchUpdate),
    /// The configuration watch stream failed.
    WatchFailed(ClientError),
    /// The configuration watch stream ended.
    WatchEnded,
    /// The configuration watch retry delay elapsed.
    ReconnectReady,
}

/// Status coordination event applied to the reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusEvent {
    /// A caller requested status watch maintenance.
    WatchRequested,
    /// The status watch stream opened.
    WatchStarted,
    /// The status watch stream produced a status snapshot.
    WatchChanged(SystemStatus),
    /// The status watch stream failed.
    WatchFailed(ClientError),
    /// The status watch stream ended.
    WatchEnded,
    /// The status watch retry delay elapsed.
    ReconnectReady,
}

/// Session coordination event applied to the reducer.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// A caller requested a new session.
    CreateRequested(OperationKey),
    /// A session was created successfully.
    Created {
        /// Operation key allocated when session creation was requested.
        operation_key: OperationKey,
        /// Created session returned by the server.
        session: SessionSummary,
    },
    /// Session creation failed.
    CreateFailed {
        /// Operation key allocated when session creation was requested.
        operation_key: OperationKey,
        /// Failure returned by the create effect.
        error: ClientError,
    },
    /// A caller requested session deletion.
    DeleteRequested {
        /// Operation key allocated when session deletion was requested.
        operation_key: OperationKey,
        /// Session id to delete.
        id: String,
    },
    /// Session deletion completed successfully.
    Deleted {
        /// Operation key allocated when session deletion was requested.
        operation_key: OperationKey,
        /// Deleted session id.
        id: String,
    },
    /// Session deletion failed.
    DeleteFailed {
        /// Operation key allocated when session deletion was requested.
        operation_key: OperationKey,
        /// Session id that failed to delete.
        id: String,
        /// Failure returned by the delete effect.
        error: ClientError,
    },
    /// A caller requested session runtime metadata replacement.
    RuntimeConfigUpdateRequested {
        /// Operation key allocated when the update was requested.
        operation_key: OperationKey,
        /// Session id to update.
        id: String,
        /// Runtime config request to resolve and store.
        runtime_config: Box<SessionRuntimeConfigRequest>,
    },
    /// Session runtime metadata replacement completed successfully.
    RuntimeConfigUpdated {
        /// Operation key allocated when the update was requested.
        operation_key: OperationKey,
        /// Updated session returned by the server.
        session: SessionSummary,
    },
    /// Session runtime metadata replacement failed.
    RuntimeConfigUpdateFailed {
        /// Operation key allocated when the update was requested.
        operation_key: OperationKey,
        /// Failure returned by the update effect.
        error: ClientError,
    },
    /// A caller requested the current session list.
    ListRequested,
    /// The current session list loaded successfully.
    Listed(Vec<SessionSummary>),
    /// The current session list failed to load.
    ListFailed(ClientError),
    /// A caller requested a session timeline page.
    TimelineRequested {
        /// Session id whose timeline should load.
        session_id: String,
        /// Timeline query to submit.
        query: Option<TimelineQuery>,
    },
    /// A session timeline page loaded successfully.
    TimelineLoaded {
        /// Session id whose timeline loaded.
        session_id: String,
        /// Timeline events returned by the server.
        events: Vec<TimelineEvent>,
    },
    /// A session timeline page failed to load.
    TimelineLoadFailed {
        /// Session id whose timeline failed to load.
        session_id: String,
        /// Failure returned by the load effect.
        error: ClientError,
    },
    /// A caller registered interest in timeline updates for one session.
    TimelineWatchInterestRegistered {
        /// Session id whose timeline should be watched.
        session_id: String,
        /// Optional cursor boundary for the watch.
        after_cursor: Option<u64>,
        /// Optional display-channel filter.
        channels: Vec<String>,
        /// Optional initial history batch size.
        initial_limit: Option<u32>,
    },
    /// A caller released interest in timeline updates for one session.
    TimelineWatchInterestReleased {
        /// Session id whose timeline interest should be released.
        session_id: String,
    },
    /// A session timeline watch stream opened.
    TimelineWatchStarted {
        /// Timeline watch identity that opened.
        key: TimelineWatchKey,
    },
    /// A session timeline watch stream produced an update.
    TimelineWatchChanged {
        /// Timeline watch identity that produced the update.
        key: TimelineWatchKey,
        /// Timeline update returned by the transport.
        update: SessionTimelineWatchUpdate,
    },
    /// A session timeline watch stream failed.
    TimelineWatchFailed {
        /// Timeline watch identity that failed.
        key: TimelineWatchKey,
        /// Failure returned by the watch stream.
        error: ClientError,
    },
    /// A session timeline watch stream ended.
    TimelineWatchEnded {
        /// Timeline watch identity that ended.
        key: TimelineWatchKey,
    },
    /// A timeline watch retry delay elapsed.
    TimelineReconnectReady {
        /// Timeline watch identity that can reconnect.
        key: TimelineWatchKey,
    },
    /// A caller requested a user message submission.
    UserMessageSubmitRequested {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Session id to submit to.
        session_id: String,
        /// User message text.
        text: String,
    },
    /// A user message submission completed successfully.
    UserMessageSubmitted {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
    },
    /// A user message submission failed.
    UserMessageSubmitFailed {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Failure returned by the submit effect.
        error: ClientError,
    },
    /// A caller requested a response to an external request.
    ExternalRequestRespondRequested {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Session id to respond to.
        session_id: String,
        /// External request id.
        request_id: String,
        /// JSON response payload.
        response_json: String,
    },
    /// An external request response completed successfully.
    ExternalRequestResponded {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
    },
    /// An external request response failed.
    ExternalRequestRespondFailed {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Failure returned by the response effect.
        error: ClientError,
    },
    /// A caller requested an active session interruption.
    InterruptRequested {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Session id to interrupt.
        session_id: String,
    },
    /// Session interruption completed successfully.
    Interrupted {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
    },
    /// Session interruption failed.
    InterruptFailed {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Failure returned by the interrupt effect.
        error: ClientError,
    },
    /// A caller registered interest in session watch updates.
    WatchInterestRegistered,
    /// A caller released interest in session watch updates.
    WatchInterestReleased,
    /// The session watch stream opened.
    WatchStarted,
    /// The session watch stream produced an update.
    WatchChanged(SessionWatchUpdate),
    /// The session watch stream failed.
    WatchFailed(ClientError),
    /// The session watch stream ended.
    WatchEnded,
    /// The session watch retry delay elapsed.
    ReconnectReady,
}

/// Side-effect intent emitted by the client for shells or runtimes to execute.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ClientEffect {
    /// Configuration side-effect intent.
    Config(ConfigEffect),
    /// Status side-effect intent.
    Status(StatusEffect),
    /// Session side-effect intent.
    Session(SessionEffect),
}

/// Sparse top-level configuration patch emitted by the client.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigPatch {
    /// Replacement providers map. `None` leaves providers unchanged.
    pub providers: Option<std::collections::HashMap<String, crate::Provider>>,
    /// Replacement MCP servers map. `None` leaves MCP servers unchanged.
    pub mcp_servers: Option<std::collections::HashMap<String, crate::McpServer>>,
    /// Replacement runtime defaults. `None` leaves defaults unchanged.
    pub runtime_defaults: Option<RuntimeDefaults>,
}

/// Configuration side-effect intent emitted by the client.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ConfigEffect {
    /// Load the current configuration snapshot.
    Load,
    /// Patch top-level configuration sections using an expected entity tag.
    Patch {
        /// Operation key allocated when the patch was requested.
        operation_key: OperationKey,
        /// Top-level section patch to submit.
        patch: ConfigPatch,
        /// Entity tag expected by the caller.
        expected_etag: String,
    },
    /// Open or continue the configuration watch stream.
    Watch,
    /// Sleep before attempting a configuration watch reconnect.
    SleepReconnect {
        /// Delay duration in milliseconds.
        delay_ms: u64,
    },
}

/// Status side-effect intent emitted by the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusEffect {
    /// Open or continue the status watch stream.
    Watch,
    /// Sleep before attempting a status watch reconnect.
    SleepReconnect {
        /// Delay duration in milliseconds.
        delay_ms: u64,
    },
}

/// Session side-effect intent emitted by the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEffect {
    /// Create a server-side session.
    Create {
        /// Operation key allocated when session creation was requested.
        operation_key: OperationKey,
    },
    /// Delete a server-side session.
    Delete {
        /// Operation key allocated when session deletion was requested.
        operation_key: OperationKey,
        /// Session id to delete.
        id: String,
    },
    /// Replace server-side session runtime metadata.
    UpdateRuntimeConfig {
        /// Operation key allocated when the update was requested.
        operation_key: OperationKey,
        /// Session id to update.
        id: String,
        /// Runtime config request to resolve and store.
        runtime_config: Box<SessionRuntimeConfigRequest>,
    },
    /// Load the current server-side session list.
    List,
    /// Load a timeline page for one session.
    GetTimeline {
        /// Session id whose timeline should load.
        session_id: String,
        /// Timeline query to submit.
        query: Option<TimelineQuery>,
    },
    /// Open or continue the session watch stream.
    Watch,
    /// Open or continue one session timeline watch stream.
    WatchTimeline {
        /// Timeline watch identity to open.
        key: TimelineWatchKey,
    },
    /// Submit a user message to a session.
    SubmitUserMessage {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Session id to submit to.
        session_id: String,
        /// User message text.
        text: String,
    },
    /// Respond to an external session request.
    RespondExternalRequest {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Session id to respond to.
        session_id: String,
        /// External request id.
        request_id: String,
        /// JSON response payload.
        response_json: String,
    },
    /// Interrupt an active session run.
    Interrupt {
        /// Operation key allocated when the command was requested.
        operation_key: OperationKey,
        /// Session id to interrupt.
        session_id: String,
    },
    /// Sleep before attempting a session watch reconnect.
    SleepReconnect {
        /// Delay duration in milliseconds.
        delay_ms: u64,
    },
    /// Sleep before attempting a timeline watch reconnect.
    SleepTimelineReconnect {
        /// Timeline watch identity that should reconnect.
        key: TimelineWatchKey,
        /// Delay duration in milliseconds.
        delay_ms: u64,
    },
}

/// Provider edit snapshot used to detect stale drafts.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderEditBaseline {
    /// Provider name at the time editing started.
    pub name: String,
    /// Configuration entity tag at the time editing started.
    pub etag: String,
    /// Provider snapshot at the time editing started.
    pub provider: Provider,
}

/// Stream update consumed by the shared client state machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    /// A chunk of text is available.
    Chunk {
        /// Runtime-local stream identifier.
        stream_id: StreamId,
        /// Text chunk to append.
        text: String,
    },
    /// A stream completed successfully.
    Finished {
        /// Runtime-local stream identifier.
        stream_id: StreamId,
    },
    /// A stream failed.
    Error {
        /// Runtime-local stream identifier.
        stream_id: StreamId,
        /// Human-readable error message.
        message: String,
    },
}
