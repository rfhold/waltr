//! Shared async executor for client side-effect intents.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;

use crate::transport::{ConfigWatchUpdate, DefaultGrpcTransport, WaltrTransport, StatusWatchStream};
use crate::{
    ClientEffect, ClientError, ClientEvent, ConfigEffect, ConfigEvent, ConfigPatch, ConfigView,
    StatusEffect, StatusEvent,
};

/// Stream of configuration changes observed through the gRPC executor.
pub type ConfigChangeStream = Pin<Box<dyn Stream<Item = Result<ConfigChange, ClientError>>>>;

pub type ConfigWatchUpdateStream =
    Pin<Box<dyn Stream<Item = Result<ConfigWatchUpdate, ClientError>>>>;

/// Stream of system readiness changes observed through the gRPC executor.
pub type StatusChangeStream = StatusWatchStream;

/// Stable shell-facing configuration watch change.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChange {
    /// Previous configuration version.
    pub previous_etag: String,
    /// New configuration version.
    pub etag: String,
    /// Configuration keys changed by the update.
    pub changed_keys: Vec<String>,
    /// Server-reported watch error, if any.
    pub error: String,
    /// New configuration snapshot when provided by the server.
    pub config: Option<ConfigView>,
}

/// Shared gRPC-backed executor for client side-effect intents.
#[derive(Clone, Debug)]
pub struct GrpcEffectExecutor {
    transport: DefaultGrpcTransport,
}

impl GrpcEffectExecutor {
    /// Create a gRPC effect executor for an endpoint base URL.
    #[must_use]
    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            transport: DefaultGrpcTransport::new(endpoint),
        }
    }

    /// Load the current configuration as the stable shell-facing view model.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot load configuration.
    pub async fn load_config(&self) -> Result<ConfigView, ClientError> {
        self.transport.get_config().await.map(ConfigView::from)
    }

    /// Patch the current configuration using the stable shell-facing view model.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport rejects or cannot apply the patch.
    pub async fn patch_config(
        &self,
        config: ConfigView,
        expected_etag: impl Into<String>,
    ) -> Result<ConfigView, ClientError> {
        let config: crate::Configuration = config.into();
        self.transport
            .patch_config(ConfigPatch::from(config), expected_etag.into())
            .await
            .map(ConfigView::from)
    }

    /// Subscribe to configuration changes as stable shell-facing view models.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot open the config watch stream.
    pub async fn watch_config(&self) -> Result<ConfigChangeStream, ClientError> {
        let stream = self
            .transport
            .watch_config()
            .await?
            .map(|event| event.map(ConfigChange::from));

        Ok(Box::pin(stream))
    }

    pub(crate) async fn watch_config_updates(
        &self,
    ) -> Result<ConfigWatchUpdateStream, ClientError> {
        let stream = self.transport.watch_config().await?;

        Ok(Box::pin(stream))
    }

    /// Subscribe to system readiness changes.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot open the status watch stream.
    pub async fn watch_status(&self) -> Result<StatusChangeStream, ClientError> {
        self.transport.watch_status().await
    }

    /// Execute one client side-effect intent and return client events for the reducer.
    pub async fn execute_effect<Sleep, SleepFuture>(
        &self,
        effect: ClientEffect,
        sleep: Sleep,
    ) -> Vec<ClientEvent>
    where
        Sleep: FnOnce(u64) -> SleepFuture,
        SleepFuture: Future<Output = ()>,
    {
        match effect {
            ClientEffect::Config(ConfigEffect::Load) => match self.transport.get_config().await {
                Ok(config) => vec![ClientEvent::Config(ConfigEvent::Loaded(config))],
                Err(error) => vec![ClientEvent::Config(ConfigEvent::LoadFailed(error))],
            },
            ClientEffect::Config(ConfigEffect::Patch {
                patch,
                expected_etag,
            }) => match self.transport.patch_config(patch, expected_etag).await {
                Ok(config) => vec![ClientEvent::Config(ConfigEvent::Patched(config))],
                Err(error) => vec![ClientEvent::Config(ConfigEvent::PatchFailed(error))],
            },
            ClientEffect::Config(ConfigEffect::Watch) => {
                match self.transport.watch_config().await {
                    Ok(_) => vec![ClientEvent::Config(ConfigEvent::WatchStarted)],
                    Err(error) => vec![ClientEvent::Config(ConfigEvent::WatchFailed(error))],
                }
            }
            ClientEffect::Status(StatusEffect::Watch) => {
                match self.transport.watch_status().await {
                    Ok(_) => vec![ClientEvent::Status(StatusEvent::WatchStarted)],
                    Err(error) => vec![ClientEvent::Status(StatusEvent::WatchFailed(error))],
                }
            }
            ClientEffect::Config(ConfigEffect::SleepReconnect { delay_ms }) => {
                sleep(delay_ms).await;
                vec![ClientEvent::Config(ConfigEvent::ReconnectReady)]
            }
            ClientEffect::Status(StatusEffect::SleepReconnect { delay_ms }) => {
                sleep(delay_ms).await;
                vec![ClientEvent::Status(StatusEvent::ReconnectReady)]
            }
        }
    }
}

impl From<crate::Configuration> for ConfigPatch {
    fn from(config: crate::Configuration) -> Self {
        Self {
            providers: Some(config.providers),
            mcp_servers: Some(config.mcp_servers),
        }
    }
}

impl From<ConfigWatchUpdate> for ConfigChange {
    fn from(update: ConfigWatchUpdate) -> Self {
        Self {
            previous_etag: update.previous_etag,
            etag: update.etag,
            changed_keys: update.changed_keys,
            error: update.error,
            config: update.config.map(ConfigView::from),
        }
    }
}
