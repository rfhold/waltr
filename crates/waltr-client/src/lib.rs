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
    StatusEffect, StatusEvent, StreamEvent, StreamId,
};
pub use coordination::runtime::{
    ClientCommand, ClientCommandRequest, ClientRuntime, ClientSnapshot, ClientStateStream,
    ClientStateStreamEvent, ConfigSnapshot, OperationSnapshot, StatusSnapshot,
};
pub use coordination::state::{
    ClientState, ConfigClientState, OperationState, OperationStatus, StatusClientState,
    StreamClientState, StreamState, StreamStatus, WatchState, watch_reconnect_delay_ms,
};
pub use error::ClientError;
pub use executor::{ConfigChange, ConfigChangeStream, GrpcEffectExecutor, StatusChangeStream};
pub use waltr::grpc::waltr::config::v1::{
    Configuration, McpServer, McpServerType, Model, ModelCapabilities, ModelLimits, ModelOptions,
    ModelType, Provider, ProviderType,
};
pub use waltr::grpc::waltr::status::v1::SystemStatus;
pub use transport::{ConfigWatchUpdate, WaltrTransport};
pub use view::{
    ConfigView, McpServerTypeView, McpServerView, ModelCapabilitiesView, ModelLimitsView,
    ModelOptionsView, ModelTypeView, ModelView, ProviderKindView, ProviderView,
};
