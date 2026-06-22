//! Shared in-memory config handle maintained from config watch events.

use std::sync::Arc;

use waltr::{
    config::{ConfigWatchApplyOutcome, ConfigWatchUpdate, apply_config_watch_update},
    grpc::waltr::config::v1::Configuration,
};
use tokio::sync::RwLock;
use tokio_stream::StreamExt as _;

use crate::config::{ConfigPubSub, ConfigStore, ConfigWatchEvent};

/// Read-only config handle for services that need the current watched config.
#[derive(Debug, Clone, Default)]
pub struct ConfigHandle {
    current: Arc<RwLock<Configuration>>,
}

impl ConfigHandle {
    /// Create a handle with an initial config snapshot.
    #[must_use]
    pub fn new(initial: Configuration) -> Self {
        Self {
            current: Arc::new(RwLock::new(initial)),
        }
    }

    /// Create a handle and spawn a maintainer driven by config pubsub updates.
    #[must_use]
    pub fn watching<S, P>(store: S, pubsub: P) -> Self
    where
        S: ConfigStore + Clone,
        P: ConfigPubSub + Clone,
    {
        let handle = Self::default();
        tokio::spawn(maintain_config(handle.clone(), store, pubsub));
        handle
    }

    /// Return the current config snapshot.
    pub async fn load(&self) -> Configuration {
        self.current.read().await.clone()
    }

    async fn replace(&self, config: Configuration) {
        *self.current.write().await = config;
    }
}

async fn maintain_config<S, P>(handle: ConfigHandle, store: S, pubsub: P)
where
    S: ConfigStore + Clone,
    P: ConfigPubSub + Clone,
{
    reload_from_store(&handle, &store).await;

    loop {
        let mut subscription = pubsub.subscribe();
        while let Some(event) = subscription.next().await {
            match event {
                Ok(event) => apply_event(&handle, &store, event).await,
                Err(error) => {
                    tracing::warn!(%error, "config handle watch lagged; reloading from store");
                    reload_from_store(&handle, &store).await;
                    break;
                }
            }
        }

        tracing::warn!("config handle watch ended; resubscribing");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        reload_from_store(&handle, &store).await;
    }
}

async fn apply_event<S>(handle: &ConfigHandle, store: &S, event: ConfigWatchEvent)
where
    S: ConfigStore,
{
    let update = config_watch_event_to_update(event);
    let current = handle.load().await;
    match apply_config_watch_update(Some(&current), &update) {
        ConfigWatchApplyOutcome::Updated(config) => handle.replace(config).await,
        ConfigWatchApplyOutcome::ReloadRequired => reload_from_store(handle, store).await,
        ConfigWatchApplyOutcome::Retry(error) => {
            tracing::warn!(error, "config handle watch update reported an error");
        }
    }
}

async fn reload_from_store<S>(handle: &ConfigHandle, store: &S)
where
    S: ConfigStore,
{
    match store.load().await {
        Ok(config) => handle.replace(config).await,
        Err(error) => tracing::warn!(%error, "config handle reload failed"),
    }
}

fn config_watch_event_to_update(event: ConfigWatchEvent) -> ConfigWatchUpdate {
    match event {
        ConfigWatchEvent::Changed {
            previous_etag,
            etag,
            changed_keys,
            config,
        } => ConfigWatchUpdate {
            previous_etag,
            etag,
            changed_keys,
            error: String::new(),
            config: Some(config),
        },
        ConfigWatchEvent::Error { error } => ConfigWatchUpdate {
            error,
            ..ConfigWatchUpdate::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use waltr::grpc::waltr::config::v1::{Configuration, Provider};

    use super::ConfigHandle;

    #[tokio::test]
    async fn load_returns_current_snapshot() {
        let handle = ConfigHandle::new(Configuration {
            etag: "v1".to_owned(),
            providers: HashMap::from([("anthropic".to_owned(), Provider::default())]),
            ..Configuration::default()
        });

        let config = handle.load().await;

        assert_eq!(config.etag, "v1");
        assert!(config.providers.contains_key("anthropic"));
    }
}
