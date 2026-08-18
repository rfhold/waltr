//! Owned filesystem watcher lifecycle primitives.

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::{error::AgentDefinitionLoadError, state::AgentDefinitionState};

const WATCH_EVENT_CHANNEL_CAPACITY: usize = 64;
const WATCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Health reported by the owned resource watcher task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionWatcherHealth {
    /// The watcher task is processing events.
    Running,
    /// The watcher failed and exited.
    Failed(String),
    /// The watcher was explicitly cancelled.
    Stopped,
}

/// Owned cancellation, health, and join handle for definition maintenance.
#[derive(Debug)]
pub struct AgentDefinitionWatcher {
    pub(super) cancellation: CancellationToken,
    pub(super) health: tokio::sync::watch::Receiver<DefinitionWatcherHealth>,
    pub(super) join: Option<JoinHandle<()>>,
}

impl AgentDefinitionWatcher {
    /// Subscribe to watcher health changes.
    #[must_use]
    pub fn health(&self) -> tokio::sync::watch::Receiver<DefinitionWatcherHealth> {
        self.health.clone()
    }

    /// Request watcher cancellation.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Cancel and join the watcher task.
    ///
    /// # Errors
    ///
    /// Returns an error if the watcher task panicked or was cancelled externally.
    ///
    /// # Panics
    ///
    /// Panics if the internally owned join handle was previously removed.
    pub async fn shutdown(mut self) -> Result<(), tokio::task::JoinError> {
        self.cancel();
        self.join
            .take()
            .expect("watcher join handle is present")
            .await
    }

    /// Join the watcher without requesting cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error if the watcher task panicked or was cancelled externally.
    ///
    /// # Panics
    ///
    /// Panics if the internally owned join handle was previously removed.
    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        self.join
            .take()
            .expect("watcher join handle is present")
            .await
    }
}

impl Drop for AgentDefinitionWatcher {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) fn event_targets_resources(event: &notify::Event, resources_root: &Path) -> bool {
    let waltr_dir = resources_root
        .parent()
        .expect("resources root has a .waltr parent");
    event
        .paths
        .iter()
        .any(|path| path.starts_with(resources_root) || path == resources_root || path == waltr_dir)
}

impl AgentDefinitionState {
    /// Start an owned watcher for direct resource-tree edits.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating-system watcher cannot be created or registered.
    ///
    /// # Panics
    ///
    /// Panics if the state contains an internally malformed resources-root path.
    #[allow(
        clippy::too_many_lines,
        reason = "watch setup and its owned event loop are clearer as one lifecycle unit"
    )]
    pub fn spawn_watcher(&self) -> Result<AgentDefinitionWatcher, AgentDefinitionLoadError> {
        let watch_root = self
            .inner
            .resources_root
            .parent()
            .and_then(Path::parent)
            .expect("resources root always has .waltr and server-root parents")
            .to_owned();
        let (events_tx, mut events_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflowed_tx = overflowed.clone();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                if events_tx.try_send(event).is_err() {
                    overflowed_tx.store(true, Ordering::Relaxed);
                }
            },
            notify::Config::default(),
        )
        .map_err(AgentDefinitionLoadError::Watch)?;
        watcher
            .watch(&watch_root, RecursiveMode::Recursive)
            .map_err(AgentDefinitionLoadError::Watch)?;

        let state = self.clone();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (health_tx, health_rx) = tokio::sync::watch::channel(DefinitionWatcherHealth::Running);
        let join = tokio::spawn(async move {
            let _watcher_guard = watcher;
            loop {
                let event = tokio::select! {
                    () = task_cancellation.cancelled() => {
                        drop(health_tx.send(DefinitionWatcherHealth::Stopped));
                        return;
                    }
                    event = events_rx.recv() => event,
                };
                let Some(event) = event else {
                    let message = "agent-definition watcher event channel closed".to_owned();
                    drop(health_tx.send(DefinitionWatcherHealth::Failed(message)));
                    return;
                };
                let mut should_reload = overflowed.swap(false, Ordering::Relaxed);
                match event {
                    Ok(event) if event_targets_resources(&event, &state.inner.resources_root) => {
                        should_reload = true;
                    }
                    Ok(_) if !should_reload => continue,
                    Ok(_) => {}
                    Err(error) => {
                        let message =
                            format!("agent-definition filesystem watcher failed: {error}");
                        drop(health_tx.send(DefinitionWatcherHealth::Failed(message)));
                        return;
                    }
                }
                tokio::time::sleep(WATCH_DEBOUNCE).await;
                while let Ok(event) = events_rx.try_recv() {
                    match event {
                        Ok(event) => {
                            should_reload |=
                                event_targets_resources(&event, &state.inner.resources_root);
                        }
                        Err(error) => {
                            let message =
                                format!("agent-definition filesystem watcher failed: {error}");
                            drop(health_tx.send(DefinitionWatcherHealth::Failed(message)));
                            return;
                        }
                    }
                }
                should_reload |= overflowed.swap(false, Ordering::Relaxed);
                if should_reload {
                    state.reload_from_disk().await;
                }
            }
        });
        Ok(AgentDefinitionWatcher {
            cancellation,
            health: health_rx,
            join: Some(join),
        })
    }
}
