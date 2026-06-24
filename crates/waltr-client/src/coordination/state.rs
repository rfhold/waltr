//! Shared client state exposed to shells.

use std::collections::BTreeMap;

use crate::{Configuration, SessionSummary, StreamId, SystemStatus, TimelineEvent};

/// Runtime-local operation identifier used by shells to track command results.
pub type OperationKey = String;

type TimelineWatchRequest = (Option<u64>, Vec<String>, Option<u32>);

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
    /// Session coordination state.
    pub sessions: SessionClientState,
    /// Stream coordination state.
    pub streams: StreamClientState,
    /// Command operation results by runtime-local key.
    pub operations: BTreeMap<OperationKey, ClientOperationState>,
    next_operation_id: u64,
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

/// Session coordination state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionClientState {
    /// Last known sessions by id.
    pub current: BTreeMap<String, SessionSummary>,
    /// Session create operation state.
    pub create: OperationState,
    /// Session list operation state.
    pub list: OperationState,
    /// Session delete operation state.
    pub delete: OperationState,
    /// Session runtime metadata update operation state.
    pub runtime_config_update: OperationState,
    /// User message submit operation state.
    pub submit_user_message: OperationState,
    /// External request response operation state.
    pub respond_external_request: OperationState,
    /// Session interrupt operation state.
    pub interrupt: OperationState,
    /// Session watch operation state.
    pub watch: WatchState,
    /// Timeline state by session id.
    pub timelines: BTreeMap<String, SessionTimelineState>,
    pub(crate) last_sequence: u64,
}

/// Timeline coordination state for one session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionTimelineState {
    /// Last known persisted timeline events by cursor.
    pub events: BTreeMap<u64, TimelineEvent>,
    /// Timeline page load operation state.
    pub load: OperationState,
    /// Timeline watch operation state.
    pub watch: WatchState,
    pub(crate) last_sequence: u64,
    pub(crate) watch_after_cursor: Option<u64>,
    pub(crate) watch_channels: Vec<String>,
    pub(crate) watch_initial_limit: Option<u32>,
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
    /// Runtime-local key for the latest operation of this kind.
    pub key: Option<OperationKey>,
}

/// Result state for a command operation that UI shells may observe by key.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ClientOperationState {
    /// Configuration patch command result.
    ConfigPatch {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Patched configuration, when the operation completed successfully.
        config: Option<Configuration>,
    },
    /// Session creation command result.
    SessionCreate {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Created session, when the operation completed successfully.
        session: Option<SessionSummary>,
    },
    /// Session deletion command result.
    SessionDelete {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Deleted session id.
        id: String,
    },
    /// Session runtime metadata update command result.
    SessionRuntimeConfigUpdate {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionSummary>,
    },
    /// User message submit command result.
    SessionSubmitUserMessage {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id submitted to.
        session_id: String,
    },
    /// External request response command result.
    SessionRespondExternalRequest {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id responded to.
        session_id: String,
        /// External request id.
        request_id: String,
    },
    /// Session interrupt command result.
    SessionInterrupt {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id interrupted.
        session_id: String,
    },
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
    pub(crate) fn next_operation_key(&mut self) -> OperationKey {
        let key = format!("op-{}", self.next_operation_id);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        key
    }

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

    pub(crate) fn set_config_patch_loading(&mut self, key: OperationKey) {
        self.config.patch.status = OperationStatus::Loading;
        self.config.patch.error = None;
        self.config.patch.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::ConfigPatch {
                status: OperationStatus::Loading,
                error: None,
                config: None,
            },
        );
    }

    pub(crate) fn set_config_patch_ready(&mut self, key: OperationKey, config: Configuration) {
        self.config.current = Some(config.clone());
        self.config.patch.status = OperationStatus::Ready;
        self.config.patch.error = None;
        self.config.patch.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::ConfigPatch {
                status: OperationStatus::Ready,
                error: None,
                config: Some(config),
            },
        );
    }

    pub(crate) fn set_config_patch_failed(&mut self, key: OperationKey, message: String) {
        self.config.patch.status = OperationStatus::Failed;
        self.config.patch.error = Some(message.clone());
        self.config.patch.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::ConfigPatch {
                status: OperationStatus::Failed,
                error: Some(message),
                config: None,
            },
        );
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

    pub(crate) fn set_session_create_loading(&mut self, key: OperationKey) {
        self.sessions.create.status = OperationStatus::Loading;
        self.sessions.create.error = None;
        self.sessions.create.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionCreate {
                status: OperationStatus::Loading,
                error: None,
                session: None,
            },
        );
    }

    pub(crate) fn set_session_create_ready(&mut self, key: OperationKey, session: SessionSummary) {
        self.sessions.create.status = OperationStatus::Ready;
        self.sessions.create.error = None;
        self.sessions.create.key = Some(key.clone());
        self.upsert_session(session.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionCreate {
                status: OperationStatus::Ready,
                error: None,
                session: Some(session),
            },
        );
    }

    pub(crate) fn set_session_create_failed(&mut self, key: OperationKey, message: String) {
        self.sessions.create.status = OperationStatus::Failed;
        self.sessions.create.error = Some(message.clone());
        self.sessions.create.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionCreate {
                status: OperationStatus::Failed,
                error: Some(message),
                session: None,
            },
        );
    }

    pub(crate) fn set_session_delete_loading(&mut self, key: OperationKey, id: String) {
        self.sessions.delete.status = OperationStatus::Loading;
        self.sessions.delete.error = None;
        self.sessions.delete.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionDelete {
                status: OperationStatus::Loading,
                error: None,
                id,
            },
        );
    }

    pub(crate) fn set_session_delete_ready(&mut self, key: OperationKey, id: String) {
        self.sessions.delete.status = OperationStatus::Ready;
        self.sessions.delete.error = None;
        self.sessions.delete.key = Some(key.clone());
        self.remove_session(&id);
        self.operations.insert(
            key,
            ClientOperationState::SessionDelete {
                status: OperationStatus::Ready,
                error: None,
                id,
            },
        );
    }

    pub(crate) fn set_session_delete_failed(
        &mut self,
        key: OperationKey,
        id: String,
        message: String,
    ) {
        self.sessions.delete.status = OperationStatus::Failed;
        self.sessions.delete.error = Some(message.clone());
        self.sessions.delete.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionDelete {
                status: OperationStatus::Failed,
                error: Some(message),
                id,
            },
        );
    }

    pub(crate) fn set_session_runtime_config_update_loading(&mut self, key: OperationKey) {
        self.sessions.runtime_config_update.status = OperationStatus::Loading;
        self.sessions.runtime_config_update.error = None;
        self.sessions.runtime_config_update.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRuntimeConfigUpdate {
                status: OperationStatus::Loading,
                error: None,
                session: None,
            },
        );
    }

    pub(crate) fn set_session_runtime_config_update_ready(
        &mut self,
        key: OperationKey,
        session: SessionSummary,
    ) {
        self.sessions.runtime_config_update.status = OperationStatus::Ready;
        self.sessions.runtime_config_update.error = None;
        self.sessions.runtime_config_update.key = Some(key.clone());
        self.upsert_session(session.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRuntimeConfigUpdate {
                status: OperationStatus::Ready,
                error: None,
                session: Some(session),
            },
        );
    }

    pub(crate) fn set_session_runtime_config_update_failed(
        &mut self,
        key: OperationKey,
        message: String,
    ) {
        self.sessions.runtime_config_update.status = OperationStatus::Failed;
        self.sessions.runtime_config_update.error = Some(message.clone());
        self.sessions.runtime_config_update.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRuntimeConfigUpdate {
                status: OperationStatus::Failed,
                error: Some(message),
                session: None,
            },
        );
    }

    pub(crate) fn set_session_list_loading(&mut self) {
        self.sessions.list.status = OperationStatus::Loading;
        self.sessions.list.error = None;
    }

    pub(crate) fn set_session_list_ready(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions.current = sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        self.sessions.list.status = OperationStatus::Ready;
        self.sessions.list.error = None;
    }

    pub(crate) fn set_session_list_failed(&mut self, message: String) {
        self.sessions.list.status = OperationStatus::Failed;
        self.sessions.list.error = Some(message);
    }

    pub(crate) fn set_session_timeline_load_loading(&mut self, session_id: &str) {
        let timeline = self.session_timeline_mut(session_id);
        timeline.load.status = OperationStatus::Loading;
        timeline.load.error = None;
    }

    pub(crate) fn set_session_timeline_load_ready(
        &mut self,
        session_id: &str,
        events: Vec<TimelineEvent>,
    ) {
        let timeline = self.session_timeline_mut(session_id);
        timeline
            .events
            .extend(events.into_iter().map(|event| (event.cursor, event)));
        timeline.load.status = OperationStatus::Ready;
        timeline.load.error = None;
    }

    pub(crate) fn set_session_timeline_load_failed(&mut self, session_id: &str, message: String) {
        let timeline = self.session_timeline_mut(session_id);
        timeline.load.status = OperationStatus::Failed;
        timeline.load.error = Some(message);
    }

    pub(crate) fn configure_session_timeline_watch(
        &mut self,
        session_id: &str,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
    ) {
        let timeline = self.session_timeline_mut(session_id);
        timeline.watch.interests = timeline.watch.interests.saturating_add(1);
        timeline.watch_after_cursor = after_cursor;
        timeline.watch_channels = channels;
        timeline.watch_initial_limit = initial_limit;
    }

    pub(crate) fn remove_session_timeline_watch_interest(&mut self, session_id: &str) -> usize {
        let timeline = self.session_timeline_mut(session_id);
        timeline.watch.interests = timeline.watch.interests.saturating_sub(1);

        if timeline.watch.interests == 0 {
            timeline.watch.status = OperationStatus::Idle;
            timeline.watch.error = None;
            timeline.watch.retry_attempt = 0;
        }

        timeline.watch.interests
    }

    pub(crate) fn set_session_timeline_watch_loading(&mut self, session_id: &str) {
        let timeline = self.session_timeline_mut(session_id);
        timeline.watch.status = OperationStatus::Loading;
        timeline.watch.error = None;
    }

    pub(crate) fn set_session_timeline_watch_opened(&mut self, session_id: &str) {
        let timeline = self.session_timeline_mut(session_id);
        timeline.watch.status = OperationStatus::Opened;
        timeline.watch.error = None;
    }

    pub(crate) fn set_session_timeline_watch_waiting(&mut self, session_id: &str) {
        self.session_timeline_mut(session_id).watch.status = OperationStatus::Waiting;
    }

    pub(crate) fn set_session_timeline_watch_changed(
        &mut self,
        sequence: u64,
        event: TimelineEvent,
    ) {
        let session_id = event.session_id.clone();
        let timeline = self.session_timeline_mut(&session_id);
        timeline.last_sequence = sequence;
        timeline.events.insert(event.cursor, event);
        timeline.watch.status = OperationStatus::Received;
        timeline.watch.error = None;
        timeline.watch.retry_attempt = 0;
    }

    pub(crate) fn set_session_timeline_watch_failed(
        &mut self,
        session_id: &str,
        message: String,
    ) -> u64 {
        let timeline = self.session_timeline_mut(session_id);
        timeline.watch.status = OperationStatus::Failed;
        timeline.watch.error = Some(message);

        let delay = watch_reconnect_delay_ms(timeline.watch.retry_attempt);
        timeline.watch.retry_attempt = timeline.watch.retry_attempt.saturating_add(1);
        delay
    }

    pub(crate) fn session_timeline_watch_request(
        &self,
        session_id: &str,
    ) -> Option<TimelineWatchRequest> {
        self.sessions.timelines.get(session_id).map(|timeline| {
            (
                timeline.watch_after_cursor,
                timeline.watch_channels.clone(),
                timeline.watch_initial_limit,
            )
        })
    }

    pub(crate) fn timeline_watch_session_ids(&self) -> Vec<String> {
        self.sessions
            .timelines
            .iter()
            .filter(|(_, timeline)| timeline.watch.interests > 0)
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    pub(crate) fn set_session_submit_user_message_loading(
        &mut self,
        key: OperationKey,
        session_id: String,
    ) {
        self.sessions.submit_user_message.status = OperationStatus::Loading;
        self.sessions.submit_user_message.error = None;
        self.sessions.submit_user_message.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionSubmitUserMessage {
                status: OperationStatus::Loading,
                error: None,
                session_id,
            },
        );
    }

    pub(crate) fn set_session_submit_user_message_ready(
        &mut self,
        key: OperationKey,
        session_id: String,
    ) {
        self.sessions.submit_user_message.status = OperationStatus::Ready;
        self.sessions.submit_user_message.error = None;
        self.sessions.submit_user_message.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionSubmitUserMessage {
                status: OperationStatus::Ready,
                error: None,
                session_id,
            },
        );
    }

    pub(crate) fn set_session_submit_user_message_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        message: String,
    ) {
        self.sessions.submit_user_message.status = OperationStatus::Failed;
        self.sessions.submit_user_message.error = Some(message.clone());
        self.sessions.submit_user_message.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionSubmitUserMessage {
                status: OperationStatus::Failed,
                error: Some(message),
                session_id,
            },
        );
    }

    pub(crate) fn set_session_respond_external_request_loading(
        &mut self,
        key: OperationKey,
        session_id: String,
        request_id: String,
    ) {
        self.sessions.respond_external_request.status = OperationStatus::Loading;
        self.sessions.respond_external_request.error = None;
        self.sessions.respond_external_request.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRespondExternalRequest {
                status: OperationStatus::Loading,
                error: None,
                session_id,
                request_id,
            },
        );
    }

    pub(crate) fn set_session_respond_external_request_ready(
        &mut self,
        key: OperationKey,
        session_id: String,
        request_id: String,
    ) {
        self.sessions.respond_external_request.status = OperationStatus::Ready;
        self.sessions.respond_external_request.error = None;
        self.sessions.respond_external_request.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRespondExternalRequest {
                status: OperationStatus::Ready,
                error: None,
                session_id,
                request_id,
            },
        );
    }

    pub(crate) fn set_session_respond_external_request_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        request_id: String,
        message: String,
    ) {
        self.sessions.respond_external_request.status = OperationStatus::Failed;
        self.sessions.respond_external_request.error = Some(message.clone());
        self.sessions.respond_external_request.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRespondExternalRequest {
                status: OperationStatus::Failed,
                error: Some(message),
                session_id,
                request_id,
            },
        );
    }

    pub(crate) fn set_session_interrupt_loading(&mut self, key: OperationKey, session_id: String) {
        self.sessions.interrupt.status = OperationStatus::Loading;
        self.sessions.interrupt.error = None;
        self.sessions.interrupt.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionInterrupt {
                status: OperationStatus::Loading,
                error: None,
                session_id,
            },
        );
    }

    pub(crate) fn set_session_interrupt_ready(&mut self, key: OperationKey, session_id: String) {
        self.sessions.interrupt.status = OperationStatus::Ready;
        self.sessions.interrupt.error = None;
        self.sessions.interrupt.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionInterrupt {
                status: OperationStatus::Ready,
                error: None,
                session_id,
            },
        );
    }

    pub(crate) fn set_session_interrupt_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        message: String,
    ) {
        self.sessions.interrupt.status = OperationStatus::Failed;
        self.sessions.interrupt.error = Some(message.clone());
        self.sessions.interrupt.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionInterrupt {
                status: OperationStatus::Failed,
                error: Some(message),
                session_id,
            },
        );
    }

    pub(crate) fn set_session_watch_loading(&mut self) {
        self.sessions.watch.status = OperationStatus::Loading;
        self.sessions.watch.error = None;
    }

    pub(crate) fn set_session_watch_opened(&mut self) {
        self.sessions.watch.status = OperationStatus::Opened;
        self.sessions.watch.error = None;
    }

    pub(crate) const fn add_session_watch_interest(&mut self) -> usize {
        self.sessions.watch.interests = self.sessions.watch.interests.saturating_add(1);
        self.sessions.watch.interests
    }

    pub(crate) fn remove_session_watch_interest(&mut self) -> usize {
        self.sessions.watch.interests = self.sessions.watch.interests.saturating_sub(1);

        if self.sessions.watch.interests == 0 {
            self.sessions.watch.status = OperationStatus::Idle;
            self.sessions.watch.error = None;
            self.sessions.watch.retry_attempt = 0;
        }

        self.sessions.watch.interests
    }

    pub(crate) const fn set_session_watch_waiting(&mut self) {
        self.sessions.watch.status = OperationStatus::Waiting;
    }

    pub(crate) fn set_session_watch_snapshot(
        &mut self,
        sequence: u64,
        sessions: Vec<SessionSummary>,
    ) {
        self.sessions.last_sequence = sequence;
        self.sessions.current = sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        self.sessions.watch.status = OperationStatus::Received;
        self.sessions.watch.error = None;
        self.sessions.watch.retry_attempt = 0;
    }

    pub(crate) fn set_session_watch_changed(&mut self, sequence: u64, session: SessionSummary) {
        self.sessions.last_sequence = sequence;
        self.upsert_session(session);
        self.sessions.watch.status = OperationStatus::Received;
        self.sessions.watch.error = None;
        self.sessions.watch.retry_attempt = 0;
    }

    pub(crate) fn set_session_watch_deleted(&mut self, sequence: u64, id: &str) {
        self.sessions.last_sequence = sequence;
        self.remove_session(id);
        self.sessions.watch.status = OperationStatus::Received;
        self.sessions.watch.error = None;
        self.sessions.watch.retry_attempt = 0;
    }

    pub(crate) fn set_session_watch_failed(&mut self, message: String) -> u64 {
        self.sessions.watch.status = OperationStatus::Failed;
        self.sessions.watch.error = Some(message);

        let delay = watch_reconnect_delay_ms(self.sessions.watch.retry_attempt);
        self.sessions.watch.retry_attempt = self.sessions.watch.retry_attempt.saturating_add(1);
        delay
    }

    fn upsert_session(&mut self, session: SessionSummary) {
        self.sessions.current.insert(session.id.clone(), session);
    }

    fn remove_session(&mut self, id: &str) {
        self.sessions.current.remove(id);
        self.sessions.timelines.remove(id);
    }

    fn session_timeline_mut(&mut self, session_id: &str) -> &mut SessionTimelineState {
        self.sessions
            .timelines
            .entry(session_id.to_owned())
            .or_default()
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
