//! Filesystem-backed configuration pubsub events and broadcast implementation.

use crate::config::{ConfigError, ConfigPubSub};
use waltr::grpc::waltr::config::v1::Configuration;
use opentelemetry::{global, metrics::ObservableGauge};
use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_stream::{Stream, wrappers::BroadcastStream, wrappers::errors::BroadcastStreamRecvError};

use super::{FilesystemConfigStore, spawn_config_watcher};

const WATCH_CHANNEL_CAPACITY: usize = 128;

/// Event emitted by the configuration watcher.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigWatchEvent {
    /// The config file was loaded successfully and changed from the previous state.
    Changed {
        /// Previous etag.
        previous_etag: String,
        /// Current etag.
        etag: String,
        /// Top-level config keys that changed.
        changed_keys: Vec<String>,
        /// Configuration containing only the changed fields currently supported by the API.
        config: Configuration,
    },
    /// The config file changed but could not be loaded.
    Error {
        /// Human-readable load error.
        error: String,
    },
}

/// Filesystem-backed broadcast-channel implementation of configuration pubsub.
#[derive(Clone, Debug)]
pub struct FilesystemConfigPubSub {
    sender: broadcast::Sender<ConfigWatchEvent>,
    watcher_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    active_watchers: Arc<AtomicU64>,
    #[allow(
        dead_code,
        reason = "observable gauge callback is active while the handle is held"
    )]
    active_watchers_metric: ObservableGauge<u64>,
}

impl FilesystemConfigPubSub {
    /// Create a broadcast pubsub channel.
    #[must_use]
    pub fn new() -> Self {
        tracing::debug!(
            capacity = WATCH_CHANNEL_CAPACITY,
            "creating config watch pubsub"
        );
        let (sender, _) = broadcast::channel(WATCH_CHANNEL_CAPACITY);
        let active_watchers = Arc::new(AtomicU64::new(0));
        let active_watchers_metric = active_watchers_metric(active_watchers.clone());
        Self {
            sender,
            watcher_handle: Arc::new(Mutex::new(None)),
            active_watchers,
            active_watchers_metric,
        }
    }

    /// Create a broadcast pubsub channel with an owned filesystem watcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the filesystem watcher cannot start.
    pub fn watching_file_store(store: FilesystemConfigStore) -> Result<Self, ConfigError> {
        let pubsub = Self::new();
        let watcher = spawn_config_watcher(store, pubsub.clone())?;
        if let Ok(mut watcher_handle) = pubsub.watcher_handle.lock() {
            watcher_handle.replace(watcher);
            tracing::debug!("config watcher handle stored");
        } else {
            tracing::warn!("failed to store config watcher handle");
        }
        Ok(pubsub)
    }

    #[cfg(test)]
    fn active_watchers(&self) -> u64 {
        self.active_watchers.load(Ordering::Relaxed)
    }
}

impl Default for FilesystemConfigPubSub {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigPubSub for FilesystemConfigPubSub {
    type Subscription = ConfigWatchSubscription;

    fn subscribe(&self) -> Self::Subscription {
        let active = self.active_watchers.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            active_watchers = active,
            "subscribing to config watch events"
        );
        ConfigWatchSubscription {
            stream: BroadcastStream::new(self.sender.subscribe()),
            active_watchers: self.active_watchers.clone(),
        }
    }

    fn publish(&self, event: ConfigWatchEvent) {
        match self.sender.send(event) {
            Ok(receivers) => tracing::debug!(receivers, "published config watch event"),
            Err(error) => tracing::debug!(%error, "no active config watch subscribers"),
        }
    }
}

/// Active configuration watch subscription.
#[derive(Debug)]
pub struct ConfigWatchSubscription {
    stream: BroadcastStream<ConfigWatchEvent>,
    active_watchers: Arc<AtomicU64>,
}

impl Stream for ConfigWatchSubscription {
    type Item = Result<ConfigWatchEvent, BroadcastStreamRecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

impl Drop for ConfigWatchSubscription {
    fn drop(&mut self) {
        let active = self.active_watchers.fetch_sub(1, Ordering::Relaxed) - 1;
        tracing::debug!(
            active_watchers = active,
            "config watch subscription dropped"
        );
    }
}

fn active_watchers_metric(active_watchers: Arc<AtomicU64>) -> ObservableGauge<u64> {
    global::meter("waltr-server")
        .u64_observable_gauge("waltr.config.watchers.active")
        .with_description("Active config watch subscriptions")
        .with_unit("{subscription}")
        .with_callback(move |observer| {
            observer.observe(active_watchers.load(Ordering::Relaxed), &[]);
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_lifecycle_updates_active_watchers() {
        let pubsub = FilesystemConfigPubSub::new();
        assert_eq!(pubsub.active_watchers(), 0);

        let first = pubsub.subscribe();
        assert_eq!(pubsub.active_watchers(), 1);

        let second = pubsub.subscribe();
        assert_eq!(pubsub.active_watchers(), 2);

        drop(first);
        assert_eq!(pubsub.active_watchers(), 1);

        drop(second);
        assert_eq!(pubsub.active_watchers(), 0);
    }
}
