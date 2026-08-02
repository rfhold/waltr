//! Shared client state exposed to shells.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::transport::SessionTimelineWatchEvent;
use crate::{
    AuthStatus, AuthTarget, CatalogMatch, CatalogModelSummary, Configuration, GenericAuthAttempt,
    RollbackTarget, SessionSummary, SessionTemplateOption, StreamId, SystemStatus, TimelineEvent,
};

/// Runtime-local operation identifier used by shells to track command results.
pub type OperationKey = String;

/// Monotonic runtime-local generation for a Waltr access login attempt.
pub type AccessAttemptId = String;

/// Lifecycle-defining request parameters for one timeline watch.
pub type TimelineWatchRequest = (Option<u64>, Vec<String>, Option<u32>);

/// Stable identity for one timeline watch stream.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TimelineWatchKey {
    /// Session id whose timeline is watched.
    pub session_id: String,
    /// Optional exclusive cursor for historical catch-up.
    pub after_cursor: Option<u64>,
    /// Display-channel filter.
    pub channels: Vec<String>,
    /// Optional initial history batch size.
    pub initial_limit: Option<u32>,
}

impl TimelineWatchKey {
    /// Create a watch key from request parameters.
    #[must_use]
    pub const fn new(
        session_id: String,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
    ) -> Self {
        Self {
            session_id,
            after_cursor,
            channels,
            initial_limit,
        }
    }

    /// Return the request parameters represented by this key.
    #[must_use]
    pub fn request(&self) -> TimelineWatchRequest {
        (self.after_cursor, self.channels.clone(), self.initial_limit)
    }
}

/// Return the capped watch reconnect delay for a retry attempt.
#[must_use]
pub fn watch_reconnect_delay_ms(attempt: usize) -> u64 {
    const RETRY_DELAYS_MS: &[u64] = &[500, 1_000, 2_000, 5_000];

    RETRY_DELAYS_MS.get(attempt).copied().unwrap_or(5_000)
}

const MAX_AUTH_POLLS: u16 = 300;
const MAX_AUTH_POLL_FAILURES: u8 = 5;

/// Full headless state for Waltr shells.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClientState {
    /// Monotonic generation of the current Waltr access context.
    pub access_generation: u64,
    /// Waltr access authentication state.
    pub access: AccessClientState,
    /// Auth coordination state.
    pub auth: AuthClientState,
    /// Model catalog coordination state.
    pub catalog: CatalogClientState,
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
    next_access_attempt_generation: u64,
    next_status_watch_epoch: u64,
    next_watch_open_id: u64,
    next_operation_id: u64,
    next_auth_poll_generation: u64,
}

/// Model catalog coordination state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogClientState {
    /// Latest model search operation.
    pub search: CatalogModelSearchState,
    /// Latest match suggestion operation.
    pub suggestions: CatalogMatchSuggestionState,
}

/// Latest model catalog search state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogModelSearchState {
    /// Runtime-local request identity.
    pub request_id: String,
    /// Search query.
    pub query: String,
    /// Current operation status.
    pub status: OperationStatus,
    /// Last search error.
    pub error: Option<String>,
    /// Whether cached data is stale.
    pub stale: bool,
    /// Matching models.
    pub models: Vec<CatalogModelSummary>,
}

/// Latest catalog match suggestion state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogMatchSuggestionState {
    /// Runtime-local request identity.
    pub request_id: String,
    /// Current operation status.
    pub status: OperationStatus,
    /// Last suggestion error.
    pub error: Option<String>,
    /// Whether cached data is stale.
    pub stale: bool,
    /// Suggested matches.
    pub matches: Vec<CatalogMatch>,
}

/// Waltr access authentication state, independent from provider credentials.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessClientState {
    /// Current Waltr access authentication status.
    pub status: AccessStatus,
    /// Last safe login error, when present.
    pub error: Option<String>,
    /// Active platform login generation, when a login flow is running.
    pub active_attempt_id: Option<AccessAttemptId>,
    pub(crate) reload_config: bool,
    pub(crate) reload_sessions: bool,
}

/// Current Waltr access authentication status.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum AccessStatus {
    /// Access has not yet been established and a protected status check is allowed.
    #[default]
    UnknownChecking,
    /// Waltr rejected access and login is required.
    LoginRequired,
    /// A platform-specific login flow is active.
    LoginInProgress,
    /// A protected status response established access.
    Authenticated,
    /// A platform-specific login flow failed.
    LoginFailed,
}

/// Auth coordination state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AuthClientState {
    /// Last known statuses by target key.
    pub statuses: BTreeMap<String, AuthStatus>,
    /// Auth status list operation state.
    pub list: OperationState,
    /// Begin auth operation state.
    pub begin: OperationState,
    /// Complete auth operation state.
    pub complete: OperationState,
    /// Cancel auth operation state.
    pub cancel: OperationState,
    /// Forced refresh operation state.
    pub refresh: OperationState,
    /// Logout operation state.
    pub logout: OperationState,
    /// Active manual auth attempt, when one has been started.
    pub active_attempt: Option<AuthAttemptState>,
}

/// Active OAuth attempt details safe to show in clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthAttemptState {
    /// Target being authenticated.
    pub target: AuthTarget,
    /// Attempt id returned by the server.
    pub attempt_id: String,
    /// URL to open for browser-based auth.
    pub auth_url: String,
    /// Verification URL for device-style auth, when supplied.
    pub verification_url: String,
    /// User code for device-style auth, when supplied.
    pub user_code: String,
    /// Attempt expiration timestamp in milliseconds since epoch.
    pub expires_at_ms: i64,
    /// Poll interval for device-style auth.
    pub poll_interval_seconds: i32,
    /// User-facing instructions returned by the server.
    pub messages: Vec<String>,
    /// Server-selected completion mode.
    pub completion_mode: i32,
    /// Non-secret MCP registration method.
    pub registration_method: i32,
    /// Durable attempt lifecycle.
    pub lifecycle: i32,
    /// Sanitized callback failure reason.
    pub failure_reason: String,
    pub(crate) poll_generation: u64,
    pub(crate) poll_count: u16,
    pub(crate) poll_failures: u8,
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
    /// Configuration reset operation state.
    pub reset: OperationState,
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
    pub(crate) watch_epoch: u64,
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
    /// Session wipe operation state.
    pub wipe: OperationState,
    /// Session runtime metadata update operation state.
    pub runtime_config_update: OperationState,
    /// User message submit operation state.
    pub submit_user_message: OperationState,
    /// External request response operation state.
    pub respond_external_request: OperationState,
    /// Session interrupt operation state.
    pub interrupt: OperationState,
    /// Session rollback operation state.
    pub rollback: OperationState,
    /// Session continue operation state.
    pub continue_session: OperationState,
    /// Session watch operation state.
    pub watch: WatchState,
    /// Timeline state by session id.
    pub timelines: BTreeMap<String, SessionTimelineState>,
    /// Latest resource search state by session id.
    pub template_searches: BTreeMap<String, SessionTemplateSearchState>,
    pub(crate) last_sequence: u64,
}

/// Resource search coordination state for one session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionTemplateSearchState {
    /// Runtime-local caller request id.
    pub request_id: String,
    /// Session id searched.
    pub session_id: String,
    /// Query used for the latest request.
    pub query: String,
    /// Current search status.
    pub status: OperationStatus,
    /// Last search error, when present.
    pub error: Option<String>,
    /// Search options returned by the server.
    pub options: Vec<SessionTemplateOption>,
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
    pub(crate) watches: BTreeMap<TimelineWatchKey, WatchState>,
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
    /// Begin auth command result.
    AuthBegin {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target being authenticated.
        target: AuthTarget,
        /// Attempt details returned by the server.
        attempt: Option<AuthAttemptState>,
    },
    /// Complete auth command result.
    AuthComplete {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Secret-safe status returned by the server.
        auth_status: Option<AuthStatus>,
    },
    /// Cancel auth command result.
    AuthCancel {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
    },
    /// Forced auth refresh command result.
    AuthRefresh {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target being refreshed.
        target: AuthTarget,
        /// Secret-safe status returned by the server.
        auth_status: Option<AuthStatus>,
    },
    /// Logout command result.
    AuthLogout {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target logged out.
        target: AuthTarget,
        /// Secret-safe status returned by the server.
        auth_status: Option<AuthStatus>,
    },
    /// Configuration patch command result.
    ConfigPatch {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Patched configuration, when the operation completed successfully.
        config: Option<Configuration>,
    },
    /// Configuration reset command result.
    ConfigReset {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Reset configuration, when the operation completed successfully.
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
    /// Session wipe command result.
    SessionWipe {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
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
        /// Structured outbound authentication failure, when present.
        authentication_failure: Option<crate::OutboundAuthenticationFailure>,
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
        /// Structured outbound authentication failure, when present.
        authentication_failure: Option<crate::OutboundAuthenticationFailure>,
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
    /// Session rollback command result.
    SessionRollback {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id rolled back.
        session_id: String,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionSummary>,
    },
    /// Session continue command result.
    SessionContinue {
        /// Current operation status.
        status: OperationStatus,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id continued.
        session_id: String,
        /// Rollback target continued from.
        target: RollbackTarget,
        /// Structured outbound authentication failure, when present.
        authentication_failure: Option<crate::OutboundAuthenticationFailure>,
    },
}

impl ClientOperationState {
    fn fail_for_waltr_login(&mut self) -> bool {
        const MESSAGE: &str = "Waltr login required";
        match self {
            Self::AuthBegin {
                status,
                error,
                attempt,
                ..
            } => fail_loading_operation(status, error, MESSAGE, || *attempt = None),
            Self::AuthComplete {
                status,
                error,
                auth_status,
            }
            | Self::AuthRefresh {
                status,
                error,
                auth_status,
                ..
            }
            | Self::AuthLogout {
                status,
                error,
                auth_status,
                ..
            } => fail_loading_operation(status, error, MESSAGE, || *auth_status = None),
            Self::AuthCancel { status, error }
            | Self::SessionDelete { status, error, .. }
            | Self::SessionWipe { status, error }
            | Self::SessionSubmitUserMessage { status, error, .. }
            | Self::SessionRespondExternalRequest { status, error, .. }
            | Self::SessionInterrupt { status, error, .. }
            | Self::SessionContinue { status, error, .. } => {
                fail_loading_operation(status, error, MESSAGE, || {})
            }
            Self::ConfigPatch {
                status,
                error,
                config,
            }
            | Self::ConfigReset {
                status,
                error,
                config,
            } => fail_loading_operation(status, error, MESSAGE, || *config = None),
            Self::SessionCreate {
                status,
                error,
                session,
            }
            | Self::SessionRuntimeConfigUpdate {
                status,
                error,
                session,
            }
            | Self::SessionRollback {
                status,
                error,
                session,
                ..
            } => fail_loading_operation(status, error, MESSAGE, || *session = None),
        }
    }

    fn redact_identity(&mut self) {
        match self {
            Self::AuthBegin { target, .. }
            | Self::AuthRefresh { target, .. }
            | Self::AuthLogout { target, .. } => {
                *target = AuthTarget::default();
            }
            Self::SessionDelete { id, .. } => id.clear(),
            Self::SessionSubmitUserMessage { session_id, .. }
            | Self::SessionInterrupt { session_id, .. }
            | Self::SessionRollback { session_id, .. } => session_id.clear(),
            Self::SessionRespondExternalRequest {
                session_id,
                request_id,
                ..
            } => {
                session_id.clear();
                request_id.clear();
            }
            Self::SessionContinue {
                session_id, target, ..
            } => {
                session_id.clear();
                target.target = None;
            }
            Self::AuthComplete { .. }
            | Self::AuthCancel { .. }
            | Self::ConfigPatch { .. }
            | Self::ConfigReset { .. }
            | Self::SessionCreate { .. }
            | Self::SessionWipe { .. }
            | Self::SessionRuntimeConfigUpdate { .. } => {}
        }
    }
}

fn fail_loading_operation(
    status: &mut OperationStatus,
    error: &mut Option<String>,
    message: &str,
    clear_payload: impl FnOnce(),
) -> bool {
    if *status != OperationStatus::Loading {
        return false;
    }
    *status = OperationStatus::Failed;
    *error = Some(message.to_owned());
    clear_payload();
    true
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ClearPolicy {
    TransientExpiry,
    ExplicitLogout,
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
    pub(crate) open_id: Option<u64>,
}

impl ClientState {
    pub(crate) fn require_waltr_login(&mut self, error: Option<String>) {
        if matches!(
            self.access.status,
            AccessStatus::LoginRequired | AccessStatus::LoginInProgress | AccessStatus::LoginFailed
        ) {
            return;
        }
        self.bump_access_generation();
        self.access.status = AccessStatus::LoginRequired;
        self.access.error = error;
        self.access.active_attempt_id = None;
        self.clear_protected_state(ClearPolicy::TransientExpiry);
    }

    pub(crate) fn start_waltr_login(&mut self) -> AccessAttemptId {
        let attempt_id = format!("login-{}", self.next_access_attempt_generation);
        self.next_access_attempt_generation = self.next_access_attempt_generation.saturating_add(1);
        self.access.status = AccessStatus::LoginInProgress;
        self.access.error = None;
        self.access.active_attempt_id = Some(attempt_id.clone());
        attempt_id
    }

    pub(crate) fn complete_waltr_login(&mut self, attempt_id: &str) -> Option<u64> {
        if self.access.active_attempt_id.as_deref() != Some(attempt_id) {
            return None;
        }
        self.access.status = AccessStatus::UnknownChecking;
        self.access.error = None;
        self.access.active_attempt_id = None;
        self.bump_access_generation();
        Some(self.start_status_watch())
    }

    pub(crate) fn authenticate_waltr_access(&mut self, epoch: u64) -> bool {
        if self.status.watch_epoch != epoch || self.access.status != AccessStatus::UnknownChecking {
            return false;
        }
        self.access.status = AccessStatus::Authenticated;
        self.access.error = None;
        true
    }

    pub(crate) fn fail_waltr_login(&mut self, attempt_id: &str, error: String) {
        if self.access.active_attempt_id.as_deref() != Some(attempt_id) {
            return;
        }
        self.access.status = AccessStatus::LoginFailed;
        self.access.error = Some(error);
        self.access.active_attempt_id = None;
        self.clear_protected_state(ClearPolicy::TransientExpiry);
    }

    pub(crate) fn cancel_waltr_login(&mut self, attempt_id: &str) {
        if self.access.active_attempt_id.as_deref() != Some(attempt_id) {
            return;
        }
        self.access.status = AccessStatus::LoginRequired;
        self.access.error = None;
        self.access.active_attempt_id = None;
        self.clear_protected_state(ClearPolicy::TransientExpiry);
    }

    pub(crate) fn reprobe_waltr_access(&mut self) -> Option<u64> {
        if self.access.status != AccessStatus::LoginRequired {
            return None;
        }
        self.access.status = AccessStatus::UnknownChecking;
        self.access.error = None;
        self.access.active_attempt_id = None;
        self.bump_access_generation();
        Some(self.start_status_watch())
    }

    pub(crate) fn logout_waltr(&mut self) {
        self.bump_access_generation();
        self.access.status = AccessStatus::UnknownChecking;
        self.access.error = None;
        self.access.active_attempt_id = None;
        self.clear_protected_state(ClearPolicy::ExplicitLogout);
        self.start_status_watch();
    }

    const fn bump_access_generation(&mut self) {
        self.access_generation = self.access_generation.saturating_add(1);
    }

    #[must_use]
    pub(crate) const fn permits_protected_requests(&self) -> bool {
        matches!(
            self.access.status,
            AccessStatus::UnknownChecking | AccessStatus::Authenticated
        )
    }

    fn clear_protected_state(&mut self, policy: ClearPolicy) {
        self.terminalize_loading_operations(policy);
        let config_interests = self.config.watch.interests;
        let session_interests = self.sessions.watch.interests;
        let timeline_interests = self
            .sessions
            .timelines
            .values()
            .flat_map(|timeline| timeline.watches.iter())
            .map(|(key, watch)| (key.clone(), watch.interests))
            .collect::<Vec<_>>();
        if policy == ClearPolicy::TransientExpiry {
            self.access.reload_config |=
                self.config.current.is_some() || self.config.load.status != OperationStatus::Idle;
            self.access.reload_sessions |= !self.sessions.current.is_empty()
                || self.sessions.list.status != OperationStatus::Idle;
        } else {
            self.access.reload_config = false;
            self.access.reload_sessions = false;
        }

        self.config = ConfigClientState::default();
        self.catalog = CatalogClientState::default();
        self.auth = AuthClientState::default();
        self.config.watch.interests = match policy {
            ClearPolicy::TransientExpiry => config_interests,
            ClearPolicy::ExplicitLogout => 0,
        };
        if self.config.watch.interests > 0 {
            self.config.watch.status = OperationStatus::Waiting;
        }
        self.invalidate_status_watch();
        self.status.current = None;
        self.status.watch.status = OperationStatus::Waiting;
        self.sessions = SessionClientState::default();
        self.sessions.watch.interests = match policy {
            ClearPolicy::TransientExpiry => session_interests,
            ClearPolicy::ExplicitLogout => 0,
        };
        if self.sessions.watch.interests > 0 {
            self.sessions.watch.status = OperationStatus::Waiting;
        }
        for (key, interests) in timeline_interests
            .into_iter()
            .filter(|_| policy == ClearPolicy::TransientExpiry)
        {
            let timeline = self
                .sessions
                .timelines
                .entry(key.session_id.clone())
                .or_default();
            timeline.watch.interests = timeline.watch.interests.saturating_add(interests);
            timeline.watch.status = OperationStatus::Waiting;
            let watch = timeline.watches.entry(key).or_default();
            watch.interests = interests;
            watch.status = OperationStatus::Waiting;
        }
        self.streams = StreamClientState::default();
    }

    pub(crate) fn start_status_watch(&mut self) -> u64 {
        self.next_status_watch_epoch = self.next_status_watch_epoch.saturating_add(1);
        self.status.watch_epoch = self.next_status_watch_epoch;
        self.status.watch.status = OperationStatus::Loading;
        self.status.watch.error = None;
        self.status.watch_epoch
    }

    fn invalidate_status_watch(&mut self) {
        self.next_status_watch_epoch = self.next_status_watch_epoch.saturating_add(1);
        self.status = StatusClientState {
            watch_epoch: self.next_status_watch_epoch,
            ..StatusClientState::default()
        };
    }

    fn terminalize_loading_operations(&mut self, policy: ClearPolicy) {
        self.operations.retain(|_, operation| {
            if operation.fail_for_waltr_login() {
                if policy == ClearPolicy::ExplicitLogout {
                    operation.redact_identity();
                }
                true
            } else {
                false
            }
        });
    }

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
        self.permits_protected_requests()
            && self.status.watch.status == OperationStatus::Received
            && self.is_system_ready()
    }

    pub(crate) fn set_config_load_loading(&mut self) {
        self.config.load.status = OperationStatus::Loading;
        self.config.load.error = None;
    }

    pub(crate) fn set_catalog_model_search_loading(&mut self, request_id: String, query: String) {
        self.catalog.search = CatalogModelSearchState {
            request_id,
            query,
            status: OperationStatus::Loading,
            ..CatalogModelSearchState::default()
        };
    }

    pub(crate) fn set_catalog_model_search_ready(
        &mut self,
        request_id: &str,
        models: Vec<CatalogModelSummary>,
        stale: bool,
    ) {
        if self.catalog.search.request_id != request_id {
            return;
        }
        self.catalog.search.status = OperationStatus::Ready;
        self.catalog.search.error = None;
        self.catalog.search.stale = stale;
        self.catalog.search.models = models;
    }

    pub(crate) fn set_catalog_model_search_failed(&mut self, request_id: &str, message: String) {
        if self.catalog.search.request_id != request_id {
            return;
        }
        self.catalog.search.status = OperationStatus::Failed;
        self.catalog.search.error = Some(message);
        self.catalog.search.stale = false;
        self.catalog.search.models.clear();
    }

    pub(crate) fn set_catalog_match_suggestion_loading(&mut self, request_id: String) {
        self.catalog.suggestions = CatalogMatchSuggestionState {
            request_id,
            status: OperationStatus::Loading,
            ..CatalogMatchSuggestionState::default()
        };
    }

    pub(crate) fn set_catalog_match_suggestion_ready(
        &mut self,
        request_id: &str,
        matches: Vec<CatalogMatch>,
        stale: bool,
    ) {
        if self.catalog.suggestions.request_id != request_id {
            return;
        }
        self.catalog.suggestions.status = OperationStatus::Ready;
        self.catalog.suggestions.error = None;
        self.catalog.suggestions.stale = stale;
        self.catalog.suggestions.matches = matches;
    }

    pub(crate) fn set_catalog_match_suggestion_failed(
        &mut self,
        request_id: &str,
        message: String,
    ) {
        if self.catalog.suggestions.request_id != request_id {
            return;
        }
        self.catalog.suggestions.status = OperationStatus::Failed;
        self.catalog.suggestions.error = Some(message);
        self.catalog.suggestions.stale = false;
        self.catalog.suggestions.matches.clear();
    }

    pub(crate) fn set_auth_list_loading(&mut self) {
        self.auth.list.status = OperationStatus::Loading;
        self.auth.list.error = None;
    }

    pub(crate) fn set_auth_list_ready(&mut self, statuses: Vec<AuthStatus>) {
        self.auth.statuses = statuses
            .into_iter()
            .filter_map(|status| {
                let target = status.target.clone()?;
                Some((auth_target_key(&target), status))
            })
            .collect();
        self.auth.list.status = OperationStatus::Ready;
        self.auth.list.error = None;
    }

    pub(crate) fn set_auth_list_failed(&mut self, message: String) {
        self.auth.list.status = OperationStatus::Failed;
        self.auth.list.error = Some(message);
    }

    pub(crate) fn set_auth_begin_loading(&mut self, key: OperationKey, target: AuthTarget) {
        self.auth.active_attempt = None;
        self.auth.begin.status = OperationStatus::Loading;
        self.auth.begin.error = None;
        self.auth.begin.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthBegin {
                status: OperationStatus::Loading,
                error: None,
                target,
                attempt: None,
            },
        );
    }

    pub(crate) fn set_auth_begin_ready(
        &mut self,
        key: OperationKey,
        target: AuthTarget,
        attempt: GenericAuthAttempt,
    ) -> Option<(String, u64, u64)> {
        self.next_auth_poll_generation = self.next_auth_poll_generation.saturating_add(1);
        let poll_generation = self.next_auth_poll_generation;
        let completion_mode = attempt.completion_mode;
        let delay_ms = auth_poll_delay_ms(attempt.poll_interval_seconds);
        let attempt = AuthAttemptState {
            target: target.clone(),
            attempt_id: attempt.attempt_id,
            auth_url: safe_http_url(&attempt.auth_url).unwrap_or_default(),
            verification_url: safe_http_url(&attempt.verification_url).unwrap_or_default(),
            user_code: safe_text(&attempt.user_code, 256),
            expires_at_ms: attempt.expires_at_ms,
            poll_interval_seconds: attempt.poll_interval_seconds,
            messages: attempt
                .messages
                .into_iter()
                .map(|message| safe_text(&message, 1_024))
                .collect(),
            completion_mode,
            registration_method: attempt.registration_method,
            lifecycle: waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Pending as i32,
            failure_reason: String::new(),
            poll_generation,
            poll_count: 0,
            poll_failures: 0,
        };

        self.auth.begin.status = OperationStatus::Ready;
        self.auth.begin.error = None;
        self.auth.begin.key = Some(key.clone());
        self.auth.active_attempt = Some(attempt.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthBegin {
                status: OperationStatus::Ready,
                error: None,
                target,
                attempt: Some(attempt),
            },
        );
        (completion_mode == waltr::grpc::waltr::auth::v1::AuthCompletionMode::ServerCallback as i32)
            .then(|| {
                (
                    self.auth
                        .active_attempt
                        .as_ref()
                        .expect("attempt was installed")
                        .attempt_id
                        .clone(),
                    poll_generation,
                    delay_ms,
                )
            })
    }

    pub(crate) fn request_auth_poll(&mut self, attempt_id: &str) -> Option<(u64, u64)> {
        let attempt = self.auth.active_attempt.as_mut()?;
        if attempt.attempt_id != attempt_id || !auth_attempt_is_pollable(attempt) {
            return None;
        }
        attempt.poll_generation = attempt.poll_generation.saturating_add(1);
        attempt.poll_failures = 0;
        Some((
            attempt.poll_generation,
            auth_poll_delay_ms(attempt.poll_interval_seconds),
        ))
    }

    pub(crate) fn auth_poll_is_current(&self, attempt_id: &str, poll_generation: u64) -> bool {
        self.auth.active_attempt.as_ref().is_some_and(|attempt| {
            attempt.attempt_id == attempt_id
                && attempt.poll_generation == poll_generation
                && auth_attempt_is_pollable(attempt)
        })
    }

    pub(crate) fn set_auth_attempt_polled(
        &mut self,
        attempt_id: &str,
        poll_generation: u64,
        response: crate::GenericAuthAttemptStatus,
    ) -> Option<u64> {
        if !self.auth_poll_is_current(attempt_id, poll_generation) {
            return None;
        }
        let Some(status) = response.attempt else {
            return self.set_auth_poll_failed(attempt_id, poll_generation);
        };
        if status.attempt_id != attempt_id
            || !self
                .auth
                .active_attempt
                .as_ref()
                .is_some_and(|attempt| status.target.as_ref() == Some(&attempt.target))
        {
            return self.set_auth_poll_failed(attempt_id, poll_generation);
        }

        let lifecycle =
            waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::try_from(status.lifecycle)
                .unwrap_or(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Unspecified);
        let attempt = self.auth.active_attempt.as_mut().expect("poll was current");
        if status.completion_mode
            != waltr::grpc::waltr::auth::v1::AuthCompletionMode::ServerCallback as i32
        {
            attempt.lifecycle = waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Failed as i32;
            "Authorization callback status was invalid.".clone_into(&mut attempt.failure_reason);
            return None;
        }
        attempt.lifecycle = status.lifecycle;
        attempt.completion_mode = status.completion_mode;
        attempt.registration_method = status.registration_method;
        attempt.expires_at_ms = status.expires_at_ms;
        auth_lifecycle_failure(lifecycle).clone_into(&mut attempt.failure_reason);
        attempt.poll_count = attempt.poll_count.saturating_add(1);
        attempt.poll_failures = 0;

        if lifecycle == waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Succeeded {
            if let Some(auth_status) = response.status
                && auth_status.target.as_ref() == Some(&attempt.target)
            {
                let target = attempt.target.clone();
                self.auth
                    .statuses
                    .insert(auth_target_key(&target), auth_status);
            } else {
                attempt.lifecycle = waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Failed as i32;
                "Authorization completion status was unavailable."
                    .clone_into(&mut attempt.failure_reason);
            }
            return None;
        }
        if !matches!(
            lifecycle,
            waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Pending
                | waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Completing
        ) {
            return None;
        }
        if attempt.poll_count >= MAX_AUTH_POLLS {
            attempt.lifecycle = waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Failed as i32;
            "Authorization status polling limit reached.".clone_into(&mut attempt.failure_reason);
            return None;
        }
        Some(auth_poll_delay_ms(attempt.poll_interval_seconds))
    }

    pub(crate) fn set_auth_poll_failed(
        &mut self,
        attempt_id: &str,
        poll_generation: u64,
    ) -> Option<u64> {
        if !self.auth_poll_is_current(attempt_id, poll_generation) {
            return None;
        }
        let attempt = self.auth.active_attempt.as_mut().expect("poll was current");
        attempt.poll_count = attempt.poll_count.saturating_add(1);
        attempt.poll_failures = attempt.poll_failures.saturating_add(1);
        "Unable to check authorization status; retrying.".clone_into(&mut attempt.failure_reason);
        if attempt.poll_count >= MAX_AUTH_POLLS || attempt.poll_failures >= MAX_AUTH_POLL_FAILURES {
            attempt.lifecycle = waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Failed as i32;
            "Authorization status could not be confirmed.".clone_into(&mut attempt.failure_reason);
            return None;
        }
        Some(
            auth_poll_delay_ms(attempt.poll_interval_seconds)
                .saturating_mul(1_u64 << attempt.poll_failures.min(3))
                .min(10_000),
        )
    }

    pub(crate) fn set_auth_begin_failed(
        &mut self,
        key: OperationKey,
        target: AuthTarget,
        message: String,
    ) {
        self.auth.begin.status = OperationStatus::Failed;
        self.auth.begin.error = Some(message.clone());
        self.auth.begin.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthBegin {
                status: OperationStatus::Failed,
                error: Some(message),
                target,
                attempt: None,
            },
        );
    }

    pub(crate) fn set_auth_complete_loading(&mut self, key: OperationKey) {
        self.auth.complete.status = OperationStatus::Loading;
        self.auth.complete.error = None;
        self.auth.complete.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthComplete {
                status: OperationStatus::Loading,
                error: None,
                auth_status: None,
            },
        );
    }

    pub(crate) fn set_auth_complete_ready(&mut self, key: OperationKey, status: AuthStatus) {
        if let Some(target) = status.target.as_ref() {
            self.auth
                .statuses
                .insert(auth_target_key(target), status.clone());
        }
        self.auth.complete.status = OperationStatus::Ready;
        self.auth.complete.error = None;
        self.auth.complete.key = Some(key.clone());
        self.auth.active_attempt = None;
        self.operations.insert(
            key,
            ClientOperationState::AuthComplete {
                status: OperationStatus::Ready,
                error: None,
                auth_status: Some(status),
            },
        );
    }

    pub(crate) fn set_auth_complete_failed(&mut self, key: OperationKey, message: String) {
        self.auth.complete.status = OperationStatus::Failed;
        self.auth.complete.error = Some(message.clone());
        self.auth.complete.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthComplete {
                status: OperationStatus::Failed,
                error: Some(message),
                auth_status: None,
            },
        );
    }

    pub(crate) fn set_auth_cancel_loading(&mut self, key: OperationKey) {
        self.auth.cancel.status = OperationStatus::Loading;
        self.auth.cancel.error = None;
        self.auth.cancel.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthCancel {
                status: OperationStatus::Loading,
                error: None,
            },
        );
    }

    pub(crate) fn set_auth_cancel_ready(&mut self, key: OperationKey) {
        self.auth.cancel.status = OperationStatus::Ready;
        self.auth.cancel.error = None;
        self.auth.cancel.key = Some(key.clone());
        self.auth.active_attempt = None;
        self.operations.insert(
            key,
            ClientOperationState::AuthCancel {
                status: OperationStatus::Ready,
                error: None,
            },
        );
    }

    pub(crate) fn set_auth_cancel_failed(&mut self, key: OperationKey, message: String) {
        self.auth.cancel.status = OperationStatus::Failed;
        self.auth.cancel.error = Some(message.clone());
        self.auth.cancel.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthCancel {
                status: OperationStatus::Failed,
                error: Some(message),
            },
        );
    }

    pub(crate) fn set_auth_refresh_loading(&mut self, key: OperationKey, target: AuthTarget) {
        self.auth.refresh.status = OperationStatus::Loading;
        self.auth.refresh.error = None;
        self.auth.refresh.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthRefresh {
                status: OperationStatus::Loading,
                error: None,
                target,
                auth_status: None,
            },
        );
    }

    pub(crate) fn set_auth_refresh_ready(&mut self, key: OperationKey, status: AuthStatus) {
        let target = status.target.clone().unwrap_or_default();
        self.auth
            .statuses
            .insert(auth_target_key(&target), status.clone());
        if self.auth.refresh.key.as_ref() == Some(&key) {
            self.auth.refresh.status = OperationStatus::Ready;
            self.auth.refresh.error = None;
        }
        self.operations.insert(
            key,
            ClientOperationState::AuthRefresh {
                status: OperationStatus::Ready,
                error: None,
                target,
                auth_status: Some(status),
            },
        );
    }

    pub(crate) fn set_auth_refresh_failed(&mut self, key: OperationKey, message: String) {
        let target = match self.operations.get(&key) {
            Some(ClientOperationState::AuthRefresh { target, .. }) => target.clone(),
            _ => AuthTarget::default(),
        };
        if self.auth.refresh.key.as_ref() == Some(&key) {
            self.auth.refresh.status = OperationStatus::Failed;
            self.auth.refresh.error = Some(message.clone());
        }
        self.operations.insert(
            key,
            ClientOperationState::AuthRefresh {
                status: OperationStatus::Failed,
                error: Some(message),
                target,
                auth_status: None,
            },
        );
    }

    pub(crate) fn set_auth_logout_loading(&mut self, key: OperationKey, target: AuthTarget) {
        self.auth.logout.status = OperationStatus::Loading;
        self.auth.logout.error = None;
        self.auth.logout.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthLogout {
                status: OperationStatus::Loading,
                error: None,
                target,
                auth_status: None,
            },
        );
    }

    pub(crate) fn set_auth_logout_ready(&mut self, key: OperationKey, status: AuthStatus) {
        let target = status.target.clone().unwrap_or_default();
        self.auth
            .statuses
            .insert(auth_target_key(&target), status.clone());
        self.auth.logout.status = OperationStatus::Ready;
        self.auth.logout.error = None;
        self.auth.logout.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthLogout {
                status: OperationStatus::Ready,
                error: None,
                target,
                auth_status: Some(status),
            },
        );
    }

    pub(crate) fn set_auth_logout_failed(&mut self, key: OperationKey, message: String) {
        let target = match self.operations.get(&key) {
            Some(ClientOperationState::AuthLogout { target, .. }) => target.clone(),
            _ => AuthTarget::default(),
        };
        self.auth.logout.status = OperationStatus::Failed;
        self.auth.logout.error = Some(message.clone());
        self.auth.logout.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::AuthLogout {
                status: OperationStatus::Failed,
                error: Some(message),
                target,
                auth_status: None,
            },
        );
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

    pub(crate) fn set_config_reset_loading(&mut self, key: OperationKey) {
        self.config.reset.status = OperationStatus::Loading;
        self.config.reset.error = None;
        self.config.reset.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::ConfigReset {
                status: OperationStatus::Loading,
                error: None,
                config: None,
            },
        );
    }

    pub(crate) fn set_config_reset_ready(&mut self, key: OperationKey, config: Configuration) {
        self.config.current = Some(config.clone());
        self.config.reset.status = OperationStatus::Ready;
        self.config.reset.error = None;
        self.config.reset.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::ConfigReset {
                status: OperationStatus::Ready,
                error: None,
                config: Some(config),
            },
        );
    }

    pub(crate) fn set_config_reset_failed(&mut self, key: OperationKey, message: String) {
        self.config.reset.status = OperationStatus::Failed;
        self.config.reset.error = Some(message.clone());
        self.config.reset.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::ConfigReset {
                status: OperationStatus::Failed,
                error: Some(message),
                config: None,
            },
        );
    }

    pub(crate) fn set_config_watch_loading(&mut self) -> Option<u64> {
        let Some(open_id) = self.next_watch_open_id() else {
            fail_watch_open_id_exhausted(&mut self.config.watch);
            return None;
        };
        self.config.watch.status = OperationStatus::Loading;
        self.config.watch.error = None;
        self.config.watch.open_id = Some(open_id);
        Some(open_id)
    }

    pub(crate) fn set_config_watch_opened(&mut self, open_id: u64) {
        if self.config.watch.open_id != Some(open_id) {
            return;
        }
        self.config.watch.status = OperationStatus::Opened;
        self.config.watch.error = None;
        self.config.watch.open_id = None;
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
            self.config.watch.open_id = None;
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
        self.config.watch.open_id = None;

        let delay = watch_reconnect_delay_ms(self.config.watch.retry_attempt);
        self.config.watch.retry_attempt = self.config.watch.retry_attempt.saturating_add(1);
        delay
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

    pub(crate) fn set_session_wipe_loading(&mut self, key: OperationKey) {
        self.sessions.wipe.status = OperationStatus::Loading;
        self.sessions.wipe.error = None;
        self.sessions.wipe.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionWipe {
                status: OperationStatus::Loading,
                error: None,
            },
        );
    }

    pub(crate) fn set_session_wipe_ready(&mut self, key: OperationKey) {
        self.clear_session_derived_state();
        self.sessions.wipe.status = OperationStatus::Ready;
        self.sessions.wipe.error = None;
        self.sessions.wipe.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionWipe {
                status: OperationStatus::Ready,
                error: None,
            },
        );
    }

    pub(crate) fn set_session_wipe_failed(&mut self, key: OperationKey, message: String) {
        self.sessions.wipe.status = OperationStatus::Failed;
        self.sessions.wipe.error = Some(message.clone());
        self.sessions.wipe.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionWipe {
                status: OperationStatus::Failed,
                error: Some(message),
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

    pub(crate) fn set_session_template_search_loading(
        &mut self,
        request_id: String,
        session_id: String,
        query: String,
    ) {
        self.sessions.template_searches.insert(
            session_id.clone(),
            SessionTemplateSearchState {
                request_id,
                session_id,
                query,
                status: OperationStatus::Loading,
                error: None,
                options: Vec::new(),
            },
        );
    }

    pub(crate) fn set_session_template_search_ready(
        &mut self,
        request_id: &str,
        session_id: &str,
        options: Vec<SessionTemplateOption>,
    ) {
        let Some(search) = self.sessions.template_searches.get_mut(session_id) else {
            return;
        };

        if search.request_id != request_id {
            return;
        }

        search.status = OperationStatus::Ready;
        search.error = None;
        search.options = options;
    }

    pub(crate) fn set_session_template_search_failed(
        &mut self,
        request_id: &str,
        session_id: &str,
        message: String,
    ) {
        let Some(search) = self.sessions.template_searches.get_mut(session_id) else {
            return;
        };

        if search.request_id != request_id {
            return;
        }

        search.status = OperationStatus::Failed;
        search.error = Some(message);
        search.options.clear();
    }

    pub(crate) fn configure_session_timeline_watch(&mut self, key: TimelineWatchKey) {
        let timeline = self.session_timeline_mut(&key.session_id);
        timeline.watch.interests = timeline.watch.interests.saturating_add(1);
        let watch = timeline.watches.entry(key).or_default();
        watch.interests = watch.interests.saturating_add(1);
    }

    pub(crate) fn remove_session_timeline_watch_interest(
        &mut self,
        session_id: &str,
    ) -> Vec<TimelineWatchKey> {
        let timeline = self.session_timeline_mut(session_id);
        let keys = timeline.watches.keys().cloned().collect::<Vec<_>>();
        timeline.watches.clear();
        timeline.watch.interests = 0;
        timeline.watch.status = OperationStatus::Idle;
        timeline.watch.error = None;
        timeline.watch.retry_attempt = 0;
        keys
    }

    pub(crate) fn set_session_timeline_watch_loading(
        &mut self,
        key: &TimelineWatchKey,
    ) -> Option<u64> {
        let Some(open_id) = self.next_watch_open_id() else {
            let timeline = self.session_timeline_mut(&key.session_id);
            if let Some(watch) = timeline.watches.get_mut(key) {
                fail_watch_open_id_exhausted(watch);
            }
            fail_watch_open_id_exhausted(&mut timeline.watch);
            return None;
        };
        let timeline = self.session_timeline_mut(&key.session_id);
        if let Some(watch) = timeline.watches.get_mut(key) {
            watch.status = OperationStatus::Loading;
            watch.error = None;
            watch.open_id = Some(open_id);
        }
        timeline.watch.status = OperationStatus::Loading;
        timeline.watch.error = None;
        Some(open_id)
    }

    pub(crate) fn set_session_timeline_watch_opened(
        &mut self,
        key: &TimelineWatchKey,
        open_id: u64,
    ) {
        let timeline = self.session_timeline_mut(&key.session_id);
        if let Some(watch) = timeline.watches.get_mut(key) {
            if watch.open_id != Some(open_id) {
                return;
            }
            watch.status = OperationStatus::Opened;
            watch.error = None;
            watch.open_id = None;
        }
        timeline.watch.status = OperationStatus::Opened;
        timeline.watch.error = None;
    }

    pub(crate) fn set_session_timeline_watch_waiting(&mut self, key: &TimelineWatchKey) {
        let timeline = self.session_timeline_mut(&key.session_id);
        if let Some(watch) = timeline.watches.get_mut(key) {
            watch.status = OperationStatus::Waiting;
        }
        timeline.watch.status = OperationStatus::Waiting;
    }

    pub(crate) fn set_session_timeline_watch_changed(
        &mut self,
        key: &TimelineWatchKey,
        sequence: u64,
        event: SessionTimelineWatchEvent,
    ) {
        let session_id = match &event {
            SessionTimelineWatchEvent::Event(event) => event.session_id.clone(),
            SessionTimelineWatchEvent::Truncated(truncation) => truncation.session_id.clone(),
        };
        let timeline = self.session_timeline_mut(&session_id);
        timeline.last_sequence = sequence;
        match event {
            SessionTimelineWatchEvent::Event(event) => {
                timeline.events.insert(event.cursor, event);
            }
            SessionTimelineWatchEvent::Truncated(truncation) => {
                let after_cursor = truncation.after_cursor.unwrap_or(0);
                timeline.events.retain(|cursor, _| *cursor <= after_cursor);
            }
        }
        if let Some(watch) = timeline.watches.get_mut(key) {
            watch.status = OperationStatus::Received;
            watch.error = None;
            watch.retry_attempt = 0;
        }
        timeline.watch.status = OperationStatus::Received;
        timeline.watch.error = None;
        timeline.watch.retry_attempt = 0;
    }

    pub(crate) fn set_session_timeline_watch_failed(
        &mut self,
        key: &TimelineWatchKey,
        message: String,
    ) -> u64 {
        let timeline = self.session_timeline_mut(&key.session_id);
        let watch = timeline.watches.entry(key.clone()).or_default();
        watch.status = OperationStatus::Failed;
        watch.error = Some(message.clone());
        watch.open_id = None;

        let delay = watch_reconnect_delay_ms(watch.retry_attempt);
        watch.retry_attempt = watch.retry_attempt.saturating_add(1);

        timeline.watch.status = OperationStatus::Failed;
        timeline.watch.error = Some(message);
        delay
    }

    pub(crate) fn session_timeline_watch_request(
        &self,
        key: &TimelineWatchKey,
    ) -> Option<TimelineWatchRequest> {
        self.sessions
            .timelines
            .get(&key.session_id)
            .and_then(|timeline| timeline.watches.get(key).map(|_| key.request()))
    }

    pub(crate) fn timeline_watch_keys(&self) -> Vec<TimelineWatchKey> {
        self.sessions
            .timelines
            .values()
            .flat_map(|timeline| {
                timeline
                    .watches
                    .iter()
                    .filter(|(_, watch)| watch.interests > 0)
                    .map(|(key, _)| key.clone())
            })
            .collect()
    }

    pub(crate) fn session_timeline_watch_is_active(&self, key: &TimelineWatchKey) -> bool {
        self.sessions
            .timelines
            .get(&key.session_id)
            .and_then(|timeline| timeline.watches.get(key))
            .is_some_and(|watch| watch.interests > 0)
    }

    fn next_watch_open_id(&mut self) -> Option<u64> {
        let next = self.next_watch_open_id.checked_add(1)?;
        self.next_watch_open_id = next;
        Some(next)
    }

    #[cfg(test)]
    pub(crate) const fn exhaust_watch_open_ids_for_test(&mut self) {
        self.next_watch_open_id = u64::MAX;
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
                authentication_failure: None,
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
                authentication_failure: None,
            },
        );
    }

    pub(crate) fn set_session_submit_user_message_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        error: &crate::ClientError,
    ) {
        let authentication_failure = error.authentication_failure().cloned();
        let message = error.to_string();
        self.sessions.submit_user_message.status = OperationStatus::Failed;
        self.sessions.submit_user_message.error = Some(message.clone());
        self.sessions.submit_user_message.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionSubmitUserMessage {
                status: OperationStatus::Failed,
                error: Some(message),
                session_id,
                authentication_failure,
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
                authentication_failure: None,
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
                authentication_failure: None,
            },
        );
    }

    pub(crate) fn set_session_respond_external_request_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        request_id: String,
        error: &crate::ClientError,
    ) {
        let authentication_failure = error.authentication_failure().cloned();
        let message = error.to_string();
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
                authentication_failure,
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

    pub(crate) fn set_session_rollback_loading(&mut self, key: OperationKey, session_id: String) {
        self.sessions.rollback.status = OperationStatus::Loading;
        self.sessions.rollback.error = None;
        self.sessions.rollback.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRollback {
                status: OperationStatus::Loading,
                error: None,
                session_id,
                session: None,
            },
        );
    }

    pub(crate) fn set_session_rollback_ready(
        &mut self,
        key: OperationKey,
        session_id: String,
        session: SessionSummary,
    ) {
        self.sessions.rollback.status = OperationStatus::Ready;
        self.sessions.rollback.error = None;
        self.sessions.rollback.key = Some(key.clone());
        self.upsert_session(session.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRollback {
                status: OperationStatus::Ready,
                error: None,
                session_id,
                session: Some(session),
            },
        );
    }

    pub(crate) fn set_session_rollback_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        message: String,
    ) {
        self.sessions.rollback.status = OperationStatus::Failed;
        self.sessions.rollback.error = Some(message.clone());
        self.sessions.rollback.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionRollback {
                status: OperationStatus::Failed,
                error: Some(message),
                session_id,
                session: None,
            },
        );
    }

    pub(crate) fn set_session_continue_loading(
        &mut self,
        key: OperationKey,
        session_id: String,
        target: RollbackTarget,
    ) {
        self.sessions.continue_session.status = OperationStatus::Loading;
        self.sessions.continue_session.error = None;
        self.sessions.continue_session.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionContinue {
                status: OperationStatus::Loading,
                error: None,
                session_id,
                target,
                authentication_failure: None,
            },
        );
    }

    pub(crate) fn set_session_continue_ready(
        &mut self,
        key: OperationKey,
        session_id: String,
        target: RollbackTarget,
    ) {
        self.sessions.continue_session.status = OperationStatus::Ready;
        self.sessions.continue_session.error = None;
        self.sessions.continue_session.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionContinue {
                status: OperationStatus::Ready,
                error: None,
                session_id,
                target,
                authentication_failure: None,
            },
        );
    }

    pub(crate) fn set_session_continue_failed(
        &mut self,
        key: OperationKey,
        session_id: String,
        target: RollbackTarget,
        error: &crate::ClientError,
    ) {
        let authentication_failure = error.authentication_failure().cloned();
        let message = error.to_string();
        self.sessions.continue_session.status = OperationStatus::Failed;
        self.sessions.continue_session.error = Some(message.clone());
        self.sessions.continue_session.key = Some(key.clone());
        self.operations.insert(
            key,
            ClientOperationState::SessionContinue {
                status: OperationStatus::Failed,
                error: Some(message),
                session_id,
                target,
                authentication_failure,
            },
        );
    }

    pub(crate) fn set_session_watch_loading(&mut self) -> Option<u64> {
        let Some(open_id) = self.next_watch_open_id() else {
            fail_watch_open_id_exhausted(&mut self.sessions.watch);
            return None;
        };
        self.sessions.watch.status = OperationStatus::Loading;
        self.sessions.watch.error = None;
        self.sessions.watch.open_id = Some(open_id);
        Some(open_id)
    }

    pub(crate) fn set_session_watch_opened(&mut self, open_id: u64) {
        if self.sessions.watch.open_id != Some(open_id) {
            return;
        }
        self.sessions.watch.status = OperationStatus::Opened;
        self.sessions.watch.error = None;
        self.sessions.watch.open_id = None;
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
            self.sessions.watch.open_id = None;
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

    pub(crate) fn set_session_watch_cleared(&mut self, sequence: u64) {
        self.clear_session_derived_state();
        self.sessions.last_sequence = sequence;
        self.sessions.watch.status = OperationStatus::Received;
        self.sessions.watch.error = None;
        self.sessions.watch.retry_attempt = 0;
    }

    pub(crate) fn set_session_watch_failed(&mut self, message: String) -> u64 {
        self.sessions.watch.status = OperationStatus::Failed;
        self.sessions.watch.error = Some(message);
        self.sessions.watch.open_id = None;

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
        self.sessions.template_searches.remove(id);
    }

    fn clear_session_derived_state(&mut self) {
        self.sessions.current.clear();
        self.sessions.timelines.clear();
        self.sessions.template_searches.clear();
        if self.sessions.list.status == OperationStatus::Loading {
            self.sessions.list.status = OperationStatus::Ready;
            self.sessions.list.error = None;
        }
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

fn fail_watch_open_id_exhausted(watch: &mut WatchState) {
    watch.status = OperationStatus::Failed;
    watch.error = Some("watch open identity exhausted".to_owned());
    watch.open_id = None;
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

pub fn auth_target_key(target: &AuthTarget) -> String {
    match target.target.as_ref() {
        Some(waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(key)) => {
            format!("provider:{key}")
        }
        Some(waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(key)) => {
            format!("mcp:{key}")
        }
        None => "unknown:".to_owned(),
    }
}

fn auth_attempt_is_pollable(attempt: &AuthAttemptState) -> bool {
    attempt.completion_mode == waltr::grpc::waltr::auth::v1::AuthCompletionMode::ServerCallback as i32
        && matches!(
            waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::try_from(attempt.lifecycle),
            Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Pending
                | waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Completing)
        )
}

fn auth_poll_delay_ms(seconds: i32) -> u64 {
    u64::try_from(seconds).unwrap_or(1).clamp(1, 10) * 1_000
}

const fn auth_lifecycle_failure(
    lifecycle: waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle,
) -> &'static str {
    match lifecycle {
        waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Failed => "Authorization failed.",
        waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Canceled => "Authorization was canceled.",
        waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Expired => "Authorization expired.",
        _ => "",
    }
}

pub fn safe_http_url(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let mut parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.has_host()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    parsed.set_fragment(None);
    Some(parsed.into())
}

pub fn safe_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
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
