//! Shared client state exposed to shells.

use std::collections::BTreeMap;

use crate::{Configuration, StreamId, SystemStatus};

/// Return the capped watch reconnect delay for a retry attempt.
#[must_use]
pub fn watch_reconnect_delay_ms(attempt: usize) -> u64 {
    const RETRY_DELAYS_MS: &[u64] = &[500, 1_000, 2_000, 5_000];

    RETRY_DELAYS_MS.get(attempt).copied().unwrap_or(5_000)
}

/// Full headless state for Waltr shells.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClientState {
    /// Configuration coordination state.
    pub config: ConfigClientState,
    /// System status coordination state.
    pub status: StatusClientState,
    /// Stream coordination state.
    pub streams: StreamClientState,
}

/// Configuration coordination state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigClientState {
    /// Last known configuration snapshot.
    pub current: Option<Configuration>,
    /// Configuration load operation state.
    pub load: OperationState,
    /// Configuration patch operation state.
    pub patch: OperationState,
    /// Configuration watch operation state.
    pub watch: WatchState,
}

/// System status coordination state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusClientState {
    /// Last known system status snapshot.
    pub current: Option<SystemStatus>,
    /// Status watch operation state.
    pub watch: WatchState,
}

/// Stream coordination state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamClientState {
    /// Stream states by runtime-local stream identifier.
    pub items: BTreeMap<StreamId, StreamState>,
}

/// Shared state for one client operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OperationState {
    /// Current operation status.
    pub status: OperationStatus,
    /// Last operation error, when present.
    pub error: Option<String>,
}

/// Shared state for one watch-style operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchState {
    /// Current watch operation status.
    pub status: OperationStatus,
    /// Last watch error, when present.
    pub error: Option<String>,
    pub(crate) interests: usize,
    pub(crate) retry_attempt: usize,
}

impl ClientState {
    #[must_use]
    pub(crate) fn is_system_ready(&self) -> bool {
        self.status.current.is_some_and(|status| status.ready)
    }

    #[must_use]
    pub(crate) fn permits_watch_maintenance(&self) -> bool {
        self.status.watch.status == OperationStatus::Received && self.is_system_ready()
    }

    pub(crate) fn set_config_load_loading(&mut self) {
        self.config.load.status = OperationStatus::Loading;
        self.config.load.error = None;
    }

    pub(crate) fn set_config_load_ready(&mut self, config: Configuration) {
        self.config.current = Some(config);
        self.config.load.status = OperationStatus::Ready;
        self.config.load.error = None;
    }

    pub(crate) fn set_config_load_failed(&mut self, message: String) {
        self.config.load.status = OperationStatus::Failed;
        self.config.load.error = Some(message);
    }

    pub(crate) fn set_config_patch_loading(&mut self) {
        self.config.patch.status = OperationStatus::Loading;
        self.config.patch.error = None;
    }

    pub(crate) fn set_config_patch_ready(&mut self, config: Configuration) {
        self.config.current = Some(config);
        self.config.patch.status = OperationStatus::Ready;
        self.config.patch.error = None;
    }

    pub(crate) fn set_config_patch_failed(&mut self, message: String) {
        self.config.patch.status = OperationStatus::Failed;
        self.config.patch.error = Some(message);
    }

    pub(crate) fn set_config_watch_loading(&mut self) {
        self.config.watch.status = OperationStatus::Loading;
        self.config.watch.error = None;
    }

    pub(crate) fn set_config_watch_opened(&mut self) {
        self.config.watch.status = OperationStatus::Opened;
        self.config.watch.error = None;
    }

    pub(crate) const fn add_config_watch_interest(&mut self) -> usize {
        self.config.watch.interests = self.config.watch.interests.saturating_add(1);
        self.config.watch.interests
    }

    pub(crate) fn remove_config_watch_interest(&mut self) -> usize {
        self.config.watch.interests = self.config.watch.interests.saturating_sub(1);

        if self.config.watch.interests == 0 {
            self.config.watch.status = OperationStatus::Idle;
            self.config.watch.error = None;
            self.config.watch.retry_attempt = 0;
        }

        self.config.watch.interests
    }

    pub(crate) const fn set_config_watch_waiting(&mut self) {
        self.config.watch.status = OperationStatus::Waiting;
    }

    pub(crate) fn set_config_watch_changed(
        &mut self,
        config: Option<Configuration>,
        error: String,
    ) {
        if let Some(config) = config {
            self.config.current = Some(config);
        }

        if error.is_empty() {
            self.config.watch.status = OperationStatus::Received;
            self.config.watch.error = None;
            self.config.watch.retry_attempt = 0;
        } else {
            self.config.watch.status = OperationStatus::Failed;
            self.config.watch.error = Some(error);
        }
    }

    pub(crate) fn set_config_watch_failed(&mut self, message: String) -> u64 {
        self.config.watch.status = OperationStatus::Failed;
        self.config.watch.error = Some(message);

        let delay = watch_reconnect_delay_ms(self.config.watch.retry_attempt);
        self.config.watch.retry_attempt = self.config.watch.retry_attempt.saturating_add(1);
        delay
    }

    pub(crate) fn set_status_watch_loading(&mut self) {
        self.status.watch.status = OperationStatus::Loading;
        self.status.watch.error = None;
    }

    pub(crate) fn set_status_watch_started(&mut self) {
        self.status.watch.status = OperationStatus::Opened;
        self.status.watch.error = None;
    }

    pub(crate) fn set_status_watch_changed(&mut self, status: SystemStatus) {
        self.status.current = Some(status);
        self.status.watch.status = OperationStatus::Received;
        self.status.watch.error = None;
        self.status.watch.retry_attempt = 0;
    }

    pub(crate) fn set_status_watch_failed(&mut self, message: String) -> u64 {
        self.status.current = None;
        self.status.watch.status = OperationStatus::Failed;
        self.status.watch.error = Some(message);

        let delay = watch_reconnect_delay_ms(self.status.watch.retry_attempt);
        self.status.watch.retry_attempt = self.status.watch.retry_attempt.saturating_add(1);
        delay
    }

    pub(crate) fn stream_mut(&mut self, stream_id: StreamId) -> &mut StreamState {
        self.streams.items.entry(stream_id).or_default()
    }
}

/// Status for one shared client operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OperationStatus {
    /// No operation has been requested yet.
    #[default]
    Idle,
    /// An operation is in flight.
    Loading,
    /// The latest operation completed successfully.
    Ready,
    /// A watch-style stream has opened but has not produced a message.
    Opened,
    /// A watch-style stream has produced at least one message and remains healthy.
    Received,
    /// The operation is waiting for central watch maintenance to permit progress.
    Waiting,
    /// The latest operation failed.
    Failed,
}

/// Shared state for one streamed response.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamState {
    text: String,
    status: StreamStatus,
    error: Option<String>,
}

impl StreamState {
    /// Return accumulated stream text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return stream status.
    #[must_use]
    pub const fn status(&self) -> StreamStatus {
        self.status
    }

    /// Return stream error, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn append(&mut self, text: &str) {
        self.text.push_str(text);
        self.status = StreamStatus::Streaming;
        self.error = None;
    }

    pub(crate) const fn finish(&mut self) {
        self.status = StreamStatus::Finished;
    }

    pub(crate) fn fail(&mut self, message: String) {
        self.status = StreamStatus::Failed;
        self.error = Some(message);
    }
}

/// Status for a tracked stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StreamStatus {
    /// The stream is known but has not emitted content yet.
    #[default]
    Pending,
    /// The stream is actively receiving chunks.
    Streaming,
    /// The stream finished successfully.
    Finished,
    /// The stream failed.
    Failed,
}
