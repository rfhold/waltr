//! Shell-neutral client events and side-effect intents.

use crate::transport::{ConfigWatchUpdate, SessionWatchUpdate};
use crate::{ClientError, Configuration, OperationKey, Provider, SessionSummary, SystemStatus};

/// Runtime-local identifier for a streamed response.
pub type StreamId = u64;

/// Event applied to the headless client state machine.
#[derive(Clone, Debug, PartialEq)]
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
    /// A caller requested the current session list.
    ListRequested,
    /// The current session list loaded successfully.
    Listed(Vec<SessionSummary>),
    /// The current session list failed to load.
    ListFailed(ClientError),
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
}

/// Configuration side-effect intent emitted by the client.
#[derive(Clone, Debug, PartialEq)]
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
    /// Load the current server-side session list.
    List,
    /// Open or continue the session watch stream.
    Watch,
    /// Sleep before attempting a session watch reconnect.
    SleepReconnect {
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
