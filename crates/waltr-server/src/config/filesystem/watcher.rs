//! Filesystem-driven configuration watch publisher.

use crate::config::{ConfigError, ConfigPubSub, ConfigStore, ConfigWatchEvent};
use waltr::grpc::waltr::config::v1::Configuration;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

use super::FilesystemConfigStore;

const WATCH_EVENT_CHANNEL_CAPACITY: usize = 64;
const CONFIG_WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Start a long-lived watcher that publishes config changes observed on disk.
///
/// # Errors
///
/// Returns an error when the filesystem watcher cannot start.
pub fn spawn_config_watcher(
    store: FilesystemConfigStore,
    pubsub: impl ConfigPubSub,
) -> Result<JoinHandle<()>, ConfigError> {
    let root = store.root_path().to_owned();
    let config_dir = store.config_dir();
    let config_path = store.config_path();
    tracing::debug!(root = %root.display(), config_dir = %config_dir.display(), config_path = %config_path.display(), "starting config watcher");
    let (events_tx, mut events_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
    let overflowed = Arc::new(AtomicBool::new(false));
    let overflowed_tx = overflowed.clone();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            if events_tx.try_send(event).is_err() {
                overflowed_tx.store(true, Ordering::Relaxed);
                tracing::warn!(
                    capacity = WATCH_EVENT_CHANNEL_CAPACITY,
                    "config watcher filesystem event buffer full; coalescing by dropping event"
                );
            }
        },
        notify::Config::default(),
    )
    .map_err(ConfigError::watch)?;

    let mut config_dir_watched = watch_initial_path(&mut watcher, &root, &config_dir)?;

    Ok(tokio::spawn(async move {
        tracing::info!(config_path = %config_path.display(), "config watcher started");
        let mut watcher_guard = watcher;
        let watch_paths = WatchPaths {
            root: &root,
            config_dir: &config_dir,
            config_path: &config_path,
        };
        let mut last_config = match store.load().await {
            Ok(config) => Some(config),
            Err(error) => {
                tracing::warn!(%error, "initial config watcher load failed");
                pubsub.publish(ConfigWatchEvent::Error {
                    error: error.to_string(),
                });
                None
            }
        };

        while let Some(event) = events_rx.recv().await {
            let mut should_load = process_watch_event(
                event,
                &mut watcher_guard,
                &watch_paths,
                &mut config_dir_watched,
            ) || overflowed.swap(false, Ordering::Relaxed);
            if !should_load {
                continue;
            }

            tokio::time::sleep(CONFIG_WATCH_DEBOUNCE).await;
            while let Ok(event) = events_rx.try_recv() {
                should_load |= process_watch_event(
                    event,
                    &mut watcher_guard,
                    &watch_paths,
                    &mut config_dir_watched,
                );
            }
            should_load |= overflowed.swap(false, Ordering::Relaxed);
            if !should_load {
                continue;
            }

            match store.load().await {
                Ok(config) => {
                    let previous = last_config.replace(config.clone()).unwrap_or_default();
                    publish_change_if_needed(&pubsub, &previous, &config);
                }
                Err(error) => {
                    tracing::warn!(%error, "config watcher load failed");
                    pubsub.publish(ConfigWatchEvent::Error {
                        error: error.to_string(),
                    });
                }
            }
        }
    }))
}

struct WatchPaths<'a> {
    root: &'a Path,
    config_dir: &'a Path,
    config_path: &'a Path,
}

fn watch_initial_path(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    config_dir: &Path,
) -> Result<bool, ConfigError> {
    let config_dir_watched = config_dir.exists();
    let initial_path = if config_dir_watched { config_dir } else { root };
    watcher
        .watch(initial_path, RecursiveMode::NonRecursive)
        .map_err(ConfigError::watch)?;
    Ok(config_dir_watched)
}

fn process_watch_event(
    event: notify::Result<notify::Event>,
    watcher: &mut RecommendedWatcher,
    paths: &WatchPaths<'_>,
    config_dir_watched: &mut bool,
) -> bool {
    match event {
        Ok(event) => {
            tracing::debug!(paths = ?event.paths, "config watcher filesystem event received");
            if event.paths.iter().any(|path| path == paths.config_dir) {
                ensure_config_dir_watched(
                    watcher,
                    paths.root,
                    paths.config_dir,
                    config_dir_watched,
                );
            }

            event_targets_config(&event, paths.config_dir, paths.config_path)
        }
        Err(error) => {
            tracing::warn!(%error, "config watcher filesystem event error");
            false
        }
    }
}

fn ensure_config_dir_watched(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    config_dir: &Path,
    config_dir_watched: &mut bool,
) {
    if *config_dir_watched || !config_dir.exists() {
        return;
    }

    tracing::debug!(config_dir = %config_dir.display(), "watching config directory");
    match watcher.watch(config_dir, RecursiveMode::NonRecursive) {
        Ok(()) => {
            *config_dir_watched = true;
            if let Err(error) = watcher.unwatch(root) {
                tracing::debug!(%error, root = %root.display(), "failed to stop watching config root");
            }
        }
        Err(error) => {
            tracing::warn!(%error, config_dir = %config_dir.display(), "failed to watch config directory");
        }
    }
}

fn event_targets_config(event: &notify::Event, config_dir: &Path, config_path: &Path) -> bool {
    event
        .paths
        .iter()
        .any(|path| path == config_path || path == config_dir)
}

fn publish_change_if_needed(
    pubsub: &impl ConfigPubSub,
    previous: &Configuration,
    current: &Configuration,
) {
    let changed_keys = changed_keys(previous, current);
    if changed_keys.is_empty() && previous.etag == current.etag {
        tracing::debug!(etag = %current.etag, "config watcher ignored unchanged config");
        return;
    }

    tracing::info!(previous_etag = %previous.etag, etag = %current.etag, changed_keys = ?changed_keys, "config watcher detected config change");
    pubsub.publish(ConfigWatchEvent::Changed {
        previous_etag: previous.etag.clone(),
        etag: current.etag.clone(),
        changed_keys,
        config: changed_config(previous, current),
    });
}

fn changed_keys(previous: &Configuration, current: &Configuration) -> Vec<String> {
    let mut keys = BTreeSet::new();
    if previous.providers != current.providers {
        keys.insert("providers".to_owned());
    }
    if previous.mcp_servers != current.mcp_servers {
        keys.insert("mcp_servers".to_owned());
    }
    keys.into_iter().collect()
}

fn changed_config(previous: &Configuration, current: &Configuration) -> Configuration {
    let mut config = Configuration {
        etag: current.etag.clone(),
        ..Configuration::default()
    };

    if previous.providers != current.providers {
        config.providers.clone_from(&current.providers);
    }
    if previous.mcp_servers != current.mcp_servers {
        config.mcp_servers.clone_from(&current.mcp_servers);
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigPubSub, ConfigStore, FilesystemConfigPubSub};
    use waltr::grpc::waltr::config::v1::{McpServer, McpServerType, Provider, ProviderType};
    use notify::{Event, EventKind};
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn save_publishes_filesystem_watch_event() {
        let store = FilesystemConfigStore::new(crate::test_support::temp_root());
        let pubsub = FilesystemConfigPubSub::new();
        let _watcher =
            spawn_config_watcher(store.clone(), pubsub.clone()).expect("watcher should start");
        let current = store.load().await.expect("get should work");
        let mut events = pubsub.subscribe();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let saved = store
            .save(config_with_provider("anthropic"))
            .await
            .expect("save should work");
        let mut event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
            .await
            .expect("watch event should arrive")
            .expect("watch stream should remain open")
            .expect("watch event should be readable");

        while let ConfigWatchEvent::Changed { etag, .. } = &event {
            if etag == &saved.etag {
                break;
            }
            event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
                .await
                .expect("watch event should arrive")
                .expect("watch stream should remain open")
                .expect("watch event should be readable");
        }

        match event {
            ConfigWatchEvent::Changed {
                previous_etag,
                etag,
                changed_keys,
                config,
            } => {
                assert_eq!(previous_etag, current.etag);
                assert_eq!(etag, saved.etag);
                assert_eq!(changed_keys, vec!["providers"]);
                assert_eq!(config.providers, saved.providers);
            }
            ConfigWatchEvent::Error { error } => panic!("unexpected watch error: {error}"),
        }
    }

    #[tokio::test]
    async fn external_edit_publishes_filesystem_watch_event() {
        let store = FilesystemConfigStore::new(crate::test_support::temp_root());
        tokio::fs::create_dir_all(store.config_dir())
            .await
            .expect("config dir should be created");
        write_config_file(&store, config_with_provider("anthropic"))
            .await
            .expect("initial config should be written");
        store.load().await.expect("initial config should load");

        let pubsub = FilesystemConfigPubSub::new();
        let _watcher =
            spawn_config_watcher(store.clone(), pubsub.clone()).expect("watcher should start");
        let mut events = pubsub.subscribe();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        write_config_file(&store, config_with_provider("openai"))
            .await
            .expect("external edit should be written");
        let edited = store.load().await.expect("edited config should load");
        let mut event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
            .await
            .expect("watch event should arrive")
            .expect("watch stream should remain open")
            .expect("watch event should be readable");

        while let ConfigWatchEvent::Changed { etag, .. } = &event {
            if etag == &edited.etag {
                break;
            }
            event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
                .await
                .expect("watch event should arrive")
                .expect("watch stream should remain open")
                .expect("watch event should be readable");
        }

        match event {
            ConfigWatchEvent::Changed {
                previous_etag: _,
                etag,
                changed_keys,
                config,
            } => {
                assert_eq!(etag, edited.etag);
                assert_eq!(changed_keys, vec!["providers"]);
                assert_eq!(config.providers, edited.providers);
            }
            ConfigWatchEvent::Error { error } => panic!("unexpected watch error: {error}"),
        }
    }

    #[tokio::test]
    async fn event_buffer_pressure_still_observes_config_change() {
        let store = FilesystemConfigStore::new(crate::test_support::temp_root());
        let pubsub = FilesystemConfigPubSub::new();
        let _watcher =
            spawn_config_watcher(store.clone(), pubsub.clone()).expect("watcher should start");
        let mut events = pubsub.subscribe();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        for index in 0..(WATCH_EVENT_CHANNEL_CAPACITY * 2) {
            tokio::fs::write(
                store.root_path().join(format!("unrelated-{index}.tmp")),
                "noise",
            )
            .await
            .expect("unrelated root write should work");
        }
        let saved = store
            .save(config_with_provider("anthropic"))
            .await
            .expect("save should work");

        let mut event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
            .await
            .expect("watch event should arrive")
            .expect("watch stream should remain open")
            .expect("watch event should be readable");

        while let ConfigWatchEvent::Changed { etag, .. } = &event {
            if etag == &saved.etag {
                break;
            }
            event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
                .await
                .expect("watch event should arrive")
                .expect("watch stream should remain open")
                .expect("watch event should be readable");
        }

        match event {
            ConfigWatchEvent::Changed { etag, config, .. } => {
                assert_eq!(etag, saved.etag);
                assert_eq!(config.providers, saved.providers);
            }
            ConfigWatchEvent::Error { error } => panic!("unexpected watch error: {error}"),
        }
    }

    #[test]
    fn event_targets_config_accepts_config_file_and_directory() {
        let root = crate::test_support::temp_root();
        let store = FilesystemConfigStore::new(root);
        let config_dir = store.config_dir();
        let config_path = store.config_path();

        assert!(event_targets_config(
            &event_with_path(config_path.clone()),
            &config_dir,
            &config_path,
        ));
        assert!(event_targets_config(
            &event_with_path(config_dir.clone()),
            &config_dir,
            &config_path,
        ));
    }

    #[test]
    fn event_targets_config_rejects_unrelated_paths() {
        let root = crate::test_support::temp_root();
        let store = FilesystemConfigStore::new(root.clone());

        assert!(!event_targets_config(
            &event_with_path(root.join("unrelated.toml")),
            &store.config_dir(),
            &store.config_path(),
        ));
    }

    #[test]
    fn changed_config_includes_mcp_server_changes() {
        let previous = Configuration {
            mcp_servers: std::collections::HashMap::from([(
                "filesystem".to_owned(),
                mcp_server("filesystem", "run-filesystem"),
            )]),
            ..Configuration::default()
        };
        let current = Configuration {
            etag: "etag-2".to_owned(),
            mcp_servers: std::collections::HashMap::from([(
                "filesystem".to_owned(),
                mcp_server("filesystem", "run-filesystem-edited"),
            )]),
            ..Configuration::default()
        };

        let changed_keys = changed_keys(&previous, &current);
        let config = changed_config(&previous, &current);

        assert_eq!(changed_keys, vec!["mcp_servers"]);
        assert_eq!(config.etag, "etag-2");
        assert_eq!(config.mcp_servers, current.mcp_servers);
        assert!(config.providers.is_empty());
    }

    fn config_with_provider(name: &str) -> Configuration {
        Configuration {
            providers: std::collections::HashMap::from([(
                name.to_owned(),
                Provider {
                    r#type: ProviderType::Anthropic as i32,
                    name: name.to_owned(),
                    ..Provider::default()
                },
            )]),
            ..Configuration::default()
        }
    }

    fn mcp_server(name: &str, command: &str) -> McpServer {
        McpServer {
            name: name.to_owned(),
            r#type: McpServerType::Stdio as i32,
            command: command.to_owned(),
            ..McpServer::default()
        }
    }

    fn event_with_path(path: std::path::PathBuf) -> Event {
        Event::new(EventKind::Any).add_path(path)
    }

    async fn write_config_file(
        store: &FilesystemConfigStore,
        mut config: Configuration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        config.etag.clear();
        let toml = toml::to_string_pretty(&config)?;
        tokio::fs::write(store.config_path(), toml).await?;
        Ok(())
    }
}
