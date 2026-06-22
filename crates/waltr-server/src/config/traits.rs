//! Configuration backend traits.

use crate::config::{ConfigError, ConfigWatchEvent};
use waltr::grpc::waltr::config::v1::{Configuration, McpServer, Provider, RuntimeDefaults};
use std::{collections::HashMap, future::Future};
use tokio_stream::{Stream, wrappers::errors::BroadcastStreamRecvError};

/// Configuration read/write abstraction.
pub trait ConfigStore: Send + Sync + 'static {
    /// Load the current configuration.
    fn load(&self) -> impl Future<Output = Result<Configuration, ConfigError>> + Send;

    /// Write the current configuration.
    fn save(
        &self,
        config: Configuration,
    ) -> impl Future<Output = Result<Configuration, ConfigError>> + Send;

    /// Merge a configuration patch using etag conflict protection.
    fn patch(
        &self,
        patch: ConfigPatch,
        expected_etag: &str,
    ) -> impl Future<Output = Result<Configuration, ConfigError>> + Send;
}

/// Sparse top-level configuration patch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigPatch {
    /// Replacement providers map. `None` leaves providers unchanged.
    pub providers: Option<HashMap<String, Provider>>,
    /// Replacement MCP servers map. `None` leaves MCP servers unchanged.
    pub mcp_servers: Option<HashMap<String, McpServer>>,
    /// Replacement runtime defaults. `None` leaves runtime defaults unchanged.
    pub runtime_defaults: Option<RuntimeDefaults>,
}

/// Configuration pubsub abstraction.
pub trait ConfigPubSub: Send + Sync + 'static {
    /// Subscription stream type.
    type Subscription: Stream<Item = Result<ConfigWatchEvent, BroadcastStreamRecvError>>
        + Send
        + Unpin
        + 'static;

    /// Subscribe to future configuration events.
    fn subscribe(&self) -> Self::Subscription;

    /// Publish a configuration event to live subscribers.
    fn publish(&self, event: ConfigWatchEvent);
}
