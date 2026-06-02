//! Shell-neutral client events and side-effect intents.

use crate::transport::ConfigWatchUpdate;
use crate::{ClientError, Configuration, Provider, SystemStatus};

/// Runtime-local identifier for a streamed response.
pub type StreamId = u64;

/// Event applied to the headless client state machine.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent {
    /// Configuration coordination event.
    Config(ConfigEvent),
    /// Status coordination event.
    Status(StatusEvent),
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
    PatchRequested,
    /// The configuration patch completed successfully.
    Patched(Configuration),
    /// The configuration patch failed.
    PatchFailed(ClientError),
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

/// Side-effect intent emitted by the client for shells or runtimes to execute.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientEffect {
    /// Configuration side-effect intent.
    Config(ConfigEffect),
    /// Status side-effect intent.
    Status(StatusEffect),
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
