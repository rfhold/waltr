//! Shared Waltr shell client reducer, executor, and stable view models.

#![allow(clippy::future_not_send)]

mod client;
mod coordination;
mod error;
mod executor;
mod transport;
mod view;

#[cfg(all(target_arch = "wasm32", feature = "wasm"))]
mod wasm;

pub use client::{Client, ClientUpdate};
pub use coordination::event::{
    ClientEffect, ClientEvent, ConfigEffect, ConfigEvent, ConfigPatch, ProviderEditBaseline,
    SessionEffect, SessionEvent, StatusEffect, StatusEvent, StreamEvent, StreamId,
};
pub use coordination::runtime::{
    ClientCommand, ClientCommandRequest, ClientOperationView, ClientRuntime, ClientSnapshot,
    ClientStateStream, ClientStateStreamEvent, ConfigSnapshot, OperationCollectionSnapshot,
    OperationSnapshot, SessionSnapshot, StatusSnapshot,
};
pub use coordination::state::{
    ClientOperationState, ClientState, ConfigClientState, OperationKey, OperationState,
    OperationStatus, SessionClientState, StatusClientState, StreamClientState, StreamState,
    StreamStatus, WatchState, watch_reconnect_delay_ms,
};
pub use error::ClientError;
pub use executor::{
    ConfigChange, ConfigChangeStream, GrpcEffectExecutor, SessionChangeStream, StatusChangeStream,
};
pub use waltr::grpc::waltr::config::v1::{
    Configuration, McpServer, McpServerType, Model, ModelCapabilities, ModelLimits, ModelOptions,
    ModelType, Provider, ProviderType, RuntimeDefaults,
};
pub use waltr::grpc::waltr::session::v1::{
    ExternalRequest, ExternalRequestKind, PendingWork, PendingWorkStatus, RuntimeConfigMatch,
    SessionMcpServerRuntime, SessionPhase, SessionProviderRuntime, SessionRuntimeConfig,
    SessionRuntimeConfigRequest, SessionStatus, SessionSummary, Suspension, SuspensionKind,
};
pub use waltr::grpc::waltr::status::v1::SystemStatus;
pub use transport::{ConfigWatchUpdate, WaltrTransport, SessionWatchEvent, SessionWatchUpdate};
pub use view::{
    ConfigView, McpServerTypeView, McpServerView, ModelCapabilitiesView, ModelLimitsView,
    ModelOptionsView, ModelTypeView, ModelView, ProviderKindView, ProviderView,
    RuntimeConfigMatchView, RuntimeDefaultsView, SessionMcpServerRuntimeView, SessionPhaseView,
    SessionProviderRuntimeView, SessionRuntimeConfigView, SessionStatusView, SessionView,
    TimestampView,
};
