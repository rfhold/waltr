//! Shared reducer runtime used by shell adapters.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::rc::Rc;

use futures_channel::oneshot;
use futures_util::{
    FutureExt, StreamExt,
    future::{Either, LocalBoxFuture, pending, select, select_all},
    pin_mut,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::executor::{
    ConfigWatchUpdateStream, SessionTimelineWatchUpdateStream, SessionWatchUpdateStream,
};
use crate::transport::{
    RoutineDefinitionWatchStream, RoutineInvocationWatchStream, SessionTimelineWatchUpdate,
};
use crate::{
    AccessAttemptId, AccessStatus, AuthAttemptState, AuthStatus, AuthTarget, CatalogEffect,
    CatalogMatchView, CatalogModelSummaryView, Client, ClientEffect, ClientError, ClientEvent,
    ClientOperationState, ClientUpdate, ConfigEffect, ConfigEvent, ConfigView, GrpcEffectExecutor,
    InferenceDefaultsView, InferenceSettingsRequest, McpAuthorizationContext, McpServerView,
    OperationKey, OperationStatus, OutboundAuthenticationFailure, ProviderView, ResourceOrigin,
    RollbackTarget, RoutineEffect, RoutineEvent, SessionEffect, SessionEvent, SessionTemplateKind,
    SessionTemplateOption, SessionTimelineView, SessionView, StatusChangeStream, StatusEffect,
    StatusEvent, TimelineDirection, TimelineQuery, TimelineWatchKey, rollback_target,
};

const READY_IDLE_DELAY_MS: u64 = 1_000;

type ClientSleep = Rc<dyn Fn(u64) -> LocalBoxFuture<'static, ()>>;

/// Command accepted by the shared client runtime.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientCommand {
    /// Record the start of a platform-specific Waltr login flow.
    StartWaltrLogin,
    /// Record installed Waltr credentials and restart protected status checking.
    CompleteWaltrLogin {
        /// Attempt generation returned after starting login.
        #[serde(rename = "attemptId")]
        attempt_id: AccessAttemptId,
    },
    /// Record a failed platform-specific Waltr login flow.
    FailWaltrLogin {
        /// Attempt generation returned after starting login.
        #[serde(rename = "attemptId")]
        attempt_id: AccessAttemptId,
        /// Safe failure text to present to the user.
        error: String,
    },
    /// Record cancellation of a platform-specific Waltr login flow.
    CancelWaltrLogin {
        /// Attempt generation returned after starting login.
        #[serde(rename = "attemptId")]
        attempt_id: AccessAttemptId,
    },
    /// Reprobe anonymously after rejected native credentials were already cleared.
    ReprobeWaltrAccess,
    /// Clear protected state after the platform removes Waltr credentials.
    LogoutWaltr,
    /// Record proactive native token expiry or terminal refresh failure.
    WaltrAccessExpired {
        /// Safe expiry or refresh failure text.
        error: String,
    },
    /// Load secret-safe auth statuses.
    RequestAuthStatuses,
    /// Begin manual OAuth for a target.
    BeginAuth {
        /// Target to authenticate.
        target: ClientAuthTarget,
    },
    /// Complete manual OAuth with a pasted redirect URL or code.
    CompleteAuth {
        /// Attempt id returned by begin.
        #[serde(rename = "attemptId")]
        attempt_id: String,
        /// Pasted redirect URL or code.
        input: String,
    },
    /// Start or restart polling for a server-owned OAuth callback attempt.
    PollAuthAttempt {
        /// Attempt id returned by begin.
        #[serde(rename = "attemptId")]
        attempt_id: String,
    },
    /// Cancel a manual OAuth attempt.
    CancelAuth {
        /// Attempt id returned by begin.
        #[serde(rename = "attemptId")]
        attempt_id: String,
    },
    /// Force-refresh stored OAuth credentials for a target.
    RefreshAuth {
        /// Target whose OAuth credential should be refreshed.
        target: ClientAuthTarget,
    },
    /// Delete stored credentials for a target.
    LogoutAuth {
        /// Target to log out.
        target: ClientAuthTarget,
    },
    /// Load the current configuration.
    RequestConfig,
    /// Reset the latest configuration to server defaults.
    ResetConfig,
    /// Search the model catalog.
    SearchCatalogModels {
        /// Runtime-local request identity.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Search query.
        query: String,
        /// Optional provider filter.
        #[serde(rename = "providerId")]
        provider_id: Option<String>,
        /// Maximum results.
        limit: u32,
    },
    /// Suggest model catalog matches.
    SuggestCatalogMatches {
        /// Runtime-local request identity.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Configured provider key.
        #[serde(rename = "providerKey")]
        provider_key: String,
        /// Configured model key.
        #[serde(rename = "modelKey")]
        model_key: String,
        /// Maximum suggestions.
        limit: u32,
    },
    /// Add a provider to the latest configuration.
    AddProvider {
        /// Provider to add.
        provider: ProviderView,
    },
    /// Update an existing provider in the latest configuration.
    UpdateProvider {
        /// Current provider name.
        #[serde(rename = "originalName")]
        original_name: String,
        /// Provider value captured when editing began.
        original: Box<ProviderView>,
        /// Persisted resource slot captured when editing began.
        baseline: ResourceEditBaselineView,
        /// Replacement provider definition.
        provider: ProviderView,
    },
    /// Delete a provider from the latest configuration.
    DeleteProvider {
        /// Provider name to delete.
        name: String,
    },
    /// Add an MCP server to the latest configuration.
    AddMcpServer {
        /// MCP server to add.
        server: McpServerView,
    },
    /// Update an existing MCP server in the latest configuration.
    UpdateMcpServer {
        /// Current MCP server key.
        #[serde(rename = "originalName")]
        original_name: String,
        /// MCP server value captured when editing began.
        original: Box<McpServerView>,
        /// Persisted resource slot captured when editing began.
        baseline: ResourceEditBaselineView,
        /// Replacement MCP server definition.
        server: McpServerView,
    },
    /// Delete an MCP server from the latest configuration.
    DeleteMcpServer {
        /// MCP server key to delete.
        name: String,
    },
    /// Replace inference defaults for newly created sessions.
    SetInferenceDefaults {
        /// Inference defaults to store, or null to clear defaults.
        defaults: Option<InferenceDefaultsView>,
    },
    /// Register interest in complete global manual Routine definitions.
    RegisterRoutineDefinitionWatchInterest,
    /// Release interest in complete global manual Routine definitions.
    ReleaseRoutineDefinitionWatchInterest,
    /// Register interest in durable Routine invocation history.
    RegisterRoutineInvocationWatchInterest,
    /// Release interest in durable Routine invocation history.
    ReleaseRoutineInvocationWatchInterest,
    /// Register interest in Routine invocations sourced from one session.
    RegisterSessionRoutineInvocationWatchInterest {
        /// Source session whose authoritative invocation history should be watched.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Release interest in Routine invocations sourced from one session.
    ReleaseSessionRoutineInvocationWatchInterest {
        /// Source session whose watch interest should be released.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Start one manual Routine. This command is never automatically retried.
    StartManualRoutine {
        /// Caller-supplied operation identity.
        #[serde(rename = "operationKey")]
        operation_key: OperationKey,
        /// Correlation-only request identity.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Routine name.
        #[serde(rename = "routineName")]
        routine_name: String,
        /// Trigger name.
        #[serde(rename = "triggerName")]
        trigger_name: String,
        /// Source session identity.
        #[serde(rename = "sourceSessionId")]
        source_session_id: String,
        /// Typed inputs keyed by declared name.
        inputs: BTreeMap<String, crate::ClientRoutineValue>,
    },
    /// Load the next older invocation-history page.
    LoadOlderRoutineInvocations {
        /// Correlation-only request identity.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Requested page size.
        #[serde(rename = "pageSize")]
        page_size: u32,
    },
    /// Load full task-level detail for one durable invocation.
    LoadRoutineInvocationDetail {
        /// Correlation-only request identity.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Durable invocation identity.
        #[serde(rename = "invocationId")]
        invocation_id: String,
    },
    /// Register current interest in configuration watch updates.
    RegisterConfigWatchInterest,
    /// Release current interest in configuration watch updates.
    ReleaseConfigWatchInterest,
    /// Create a new server-side session.
    CreateSession {
        /// Exact selectable agent name.
        #[serde(rename = "agentName")]
        agent_name: String,
    },
    /// Retry loading selectable session agents.
    RequestSessionAgents,
    /// Delete a server-side session by id.
    DeleteSession {
        /// Session id to delete.
        id: String,
    },
    /// Delete every server-side session after local confirmation.
    WipeSessions,
    /// Replace inference settings for a server-side session.
    UpdateSessionInferenceSettings {
        /// Session id to update.
        id: String,
        /// Provider key to use on subsequent prompts.
        #[serde(rename = "providerKey")]
        provider_key: String,
        /// Model id to use on subsequent prompts.
        #[serde(rename = "modelId")]
        model_id: String,
        /// Optional per-session reasoning effort.
        #[serde(rename = "reasoningEffort")]
        reasoning_effort: Option<String>,
    },
    /// Switch a session to an exact selectable agent.
    UpdateSessionAgent {
        /// Session id to update.
        id: String,
        /// Exact selectable agent name.
        #[serde(rename = "agentName")]
        agent_name: String,
    },
    /// Register current interest in session watch updates.
    RegisterSessionWatchInterest,
    /// Release current interest in session watch updates.
    ReleaseSessionWatchInterest,
    /// Load a page of timeline events for one session.
    RequestSessionTimeline {
        /// Session id whose timeline should load.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Direction from the exclusive cursor boundary.
        direction: ClientTimelineDirection,
        /// Exclusive cursor boundary.
        cursor: Option<u64>,
        /// Maximum events to return.
        limit: u32,
        /// Display-channel filter.
        channels: Vec<String>,
    },
    /// Search templates visible to one session.
    SearchSessionTemplates {
        /// Runtime-local caller request id.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Session id whose visible templates should be searched.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Search query.
        query: String,
        /// Maximum templates to return.
        limit: u32,
        /// Template kinds to include.
        kinds: Vec<ClientSessionTemplateKind>,
        /// Template search target.
        target: ClientSessionTemplateSearchTarget,
        /// Cursor byte offset relative to query for context-aware completions.
        cursor: Option<u32>,
    },
    /// Register interest in timeline updates for one session.
    RegisterSessionTimelineWatchInterest {
        /// Session id whose timeline should be watched.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Optional cursor boundary for replay.
        #[serde(rename = "afterCursor")]
        after_cursor: Option<u64>,
        /// Display-channel filter.
        channels: Vec<String>,
        /// Optional history batch size.
        #[serde(rename = "initialLimit")]
        initial_limit: Option<u32>,
    },
    /// Release interest in timeline updates for one session.
    ReleaseSessionTimelineWatchInterest {
        /// Session id whose timeline watch interest should be released.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Submit a user message to a session.
    SubmitUserMessage {
        /// Session id to submit to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// User message text.
        text: String,
    },
    /// Respond to an external request.
    RespondExternalRequest {
        /// Session id to respond to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// External request id.
        #[serde(rename = "requestId")]
        request_id: String,
        /// JSON response payload.
        #[serde(rename = "responseJson")]
        response_json: String,
    },
    /// Interrupt an active session run.
    InterruptSession {
        /// Session id to interrupt.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Roll back a session to the end of a turn.
    RollbackSessionToTurn {
        /// Session id to roll back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Turn to preserve.
        turn: u64,
    },
    /// Roll back a session to a timeline cursor.
    RollbackSessionToCursor {
        /// Session id to roll back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Cursor to preserve.
        cursor: u64,
    },
    /// Roll back a session to immediately before a timeline cursor.
    RollbackSessionBeforeCursor {
        /// Session id to roll back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Cursor to remove.
        cursor: u64,
    },
    /// Roll back to a turn and continue from that checkpoint.
    ContinueSessionFromTurn {
        /// Session id to continue.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Turn to continue from.
        turn: u64,
    },
    /// Roll back to a timeline cursor and continue from that checkpoint.
    ContinueSessionFromCursor {
        /// Session id to continue.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Cursor to continue from.
        cursor: u64,
    },
    /// Process one status watch update.
    WatchStatus,
    /// Keep watching status until the reducer observes readiness.
    WaitForSystemReady,
}

/// Stable UI-facing auth target.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientAuthTarget {
    /// Target kind, currently `provider` or `mcpServer`.
    pub kind: String,
    /// Config key for the target.
    pub key: String,
}

/// Direction for loading a timeline page from an exclusive cursor boundary.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientTimelineDirection {
    /// Load events after the cursor.
    Forward,
    /// Load events before the cursor.
    Backward,
}

/// Template kind requested by UI clients.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ClientSessionTemplateKind {
    /// File templates visible to the session.
    File,
    /// Tool templates visible to the session.
    Tool,
    /// MCP resource templates visible to the session.
    Resource,
    /// Prompt templates visible to the session.
    Prompt,
}

/// Template search target requested by UI clients.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum ClientSessionTemplateSearchTarget {
    /// Search file and tool reference templates.
    #[default]
    Resources,
    /// Search prompt templates such as skills or MCP prompts.
    Prompts,
}

/// Stable snapshot of shared client state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ClientSnapshot {
    /// Waltr access authentication snapshot.
    pub access: AccessSnapshot,
    /// Auth coordination snapshot.
    pub auth: AuthSnapshot,
    /// Model catalog coordination snapshot.
    pub catalog: CatalogSnapshot,
    /// Command operation result snapshots.
    pub operations: OperationCollectionSnapshot,
    /// Configuration coordination snapshot.
    pub config: ConfigSnapshot,
    /// System status coordination snapshot.
    pub status: StatusSnapshot,
    /// Session coordination snapshot.
    pub sessions: SessionSnapshot,
    /// Session-scoped manual Routine coordination snapshot.
    pub routines: RoutineSnapshot,
}

/// Stable global Routine snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineSnapshot {
    /// Global definition watch and complete ordered projection.
    pub definitions: RoutineDefinitionSnapshot,
    /// Durable bounded invocation history.
    pub history: RoutineHistorySnapshot,
    /// Authoritative bounded invocation histories by source session id.
    pub session_histories: BTreeMap<String, RoutineHistorySnapshot>,
    /// Bounded keyed detail loads.
    pub details: BTreeMap<String, RoutineDetailSnapshot>,
    /// Bounded local manual-start operations.
    pub starts: BTreeMap<OperationKey, RoutineStartOperationView>,
}

/// Stable global manual Routine definition snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineDefinitionSnapshot {
    /// Definition watch health.
    pub watch: OperationSnapshot,
    /// Stable digest of the current complete projection.
    pub digest: String,
    /// Definitions in canonical server order.
    pub definitions: Vec<crate::ClientManualRoutineDefinition>,
}

/// Stable bounded durable invocation history snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineHistorySnapshot {
    /// Invocation watch health.
    pub watch: OperationSnapshot,
    /// Newest-first retained invocation summaries.
    pub recent: Vec<crate::ClientRoutineInvocationSummary>,
    /// Last successfully reduced durable event sequence.
    pub through_sequence: Option<u64>,
    /// Exclusive admission cursor for another older page.
    pub older_page_token: Option<u64>,
    /// Current older-page operation.
    pub pagination: OperationSnapshot,
}

/// Stable state for one keyed detail load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineDetailSnapshot {
    /// Current load status.
    pub status: &'static str,
    /// Full detail when available.
    pub detail: Option<crate::ClientRoutineInvocationDetail>,
    /// Last load error.
    pub error: Option<String>,
}

/// Stable bounded local manual-start operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RoutineStartOperationView {
    /// Shell-local operation key.
    pub operation_key: OperationKey,
    /// Correlation-only request identity.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Submitted Routine name.
    #[serde(rename = "routineName")]
    pub routine_name: String,
    /// Submitted trigger name.
    #[serde(rename = "triggerName")]
    pub trigger_name: String,
    /// Source session supplied at start only.
    #[serde(rename = "sourceSessionId")]
    pub source_session_id: String,
    /// Submitted inputs retained for validation correction.
    pub inputs: BTreeMap<String, crate::ClientRoutineValue>,
    /// Stable local operation status.
    pub status: &'static str,
    /// Associated durable server identity after admission.
    pub invocation_id: Option<String>,
    /// Structured pre-admission validation failure.
    pub validation_failure: Option<crate::ClientRoutineValidationFailure>,
    /// Transport failure before admission identity was returned.
    pub error: Option<String>,
}

/// Stable model catalog coordination snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSnapshot {
    /// Latest model search.
    pub search: CatalogModelSearchSnapshot,
    /// Latest match suggestions.
    pub suggestions: CatalogMatchSuggestionSnapshot,
}

/// Stable latest model catalog search snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelSearchSnapshot {
    /// Runtime-local request identity.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Search query.
    pub query: String,
    /// Stable operation status name.
    pub status: &'static str,
    /// Last search error.
    pub error: Option<String>,
    /// Whether cached data is stale.
    pub stale: bool,
    /// Matching models.
    pub models: Vec<CatalogModelSummaryView>,
}

/// Stable latest catalog match suggestion snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogMatchSuggestionSnapshot {
    /// Runtime-local request identity.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Stable operation status name.
    pub status: &'static str,
    /// Last suggestion error.
    pub error: Option<String>,
    /// Whether cached data is stale.
    pub stale: bool,
    /// Suggested matches.
    pub matches: Vec<CatalogMatchView>,
}

/// Stable Waltr access authentication snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AccessSnapshot {
    /// Current Waltr access status.
    pub status: AccessStatus,
    /// Last safe login error, when present.
    pub error: Option<String>,
    /// Active platform login generation, when one is running.
    #[serde(rename = "activeAttemptId")]
    pub active_attempt_id: Option<AccessAttemptId>,
}

/// Stable auth coordination snapshot.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthSnapshot {
    /// Last known secret-safe auth statuses by stable target key.
    pub statuses: BTreeMap<String, AuthStatusSnapshot>,
    /// Active manual auth attempt, when one has started.
    #[serde(rename = "activeAttempt")]
    pub active_attempt: Option<AuthAttemptSnapshot>,
    /// Auth status list operation state.
    pub list: OperationSnapshot,
    /// Begin auth operation state.
    pub begin: OperationSnapshot,
    /// Complete auth operation state.
    pub complete: OperationSnapshot,
    /// Cancel auth operation state.
    pub cancel: OperationSnapshot,
    /// Forced refresh operation state.
    pub refresh: OperationSnapshot,
    /// Logout operation state.
    pub logout: OperationSnapshot,
}

/// Secret-safe auth status exposed to UI shells.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatusSnapshot {
    /// Target whose status is represented.
    pub target: ClientAuthTarget,
    /// Stable status kind name.
    pub kind: String,
    /// Supported authorization method names.
    #[serde(rename = "supportedMethods")]
    pub supported_methods: Vec<String>,
    /// Credential expiry in Unix milliseconds, or zero when unknown.
    #[serde(rename = "expiresAtMs")]
    pub expires_at_ms: i64,
    /// Non-secret provider or MCP metadata.
    pub metadata: BTreeMap<String, String>,
    /// Sanitized status detail.
    pub message: String,
    /// Non-secret MCP registration method name.
    #[serde(rename = "registrationMethod")]
    pub registration_method: String,
    /// Whether forced refresh is currently available.
    #[serde(rename = "canRefresh")]
    pub can_refresh: bool,
}

/// Structured outbound authentication failure exposed to UI shells.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationFailureSnapshot {
    /// Failure schema version.
    pub version: u32,
    /// Stable authentication domain name.
    pub domain: String,
    /// Stable authentication failure reason name.
    pub reason: String,
    /// Provider or MCP target that failed.
    pub target: ClientAuthTarget,
    /// Sanitized upstream MCP context, when supplied.
    #[serde(rename = "mcpContext")]
    pub mcp_context: Option<McpAuthorizationContextSnapshot>,
}

/// Sanitized upstream MCP authentication context.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct McpAuthorizationContextSnapshot {
    /// Protected MCP endpoint, when safe.
    pub endpoint: Option<String>,
    /// HTTP status associated with the challenge.
    pub http_status: u32,
    /// Protected-resource metadata URL, when safe.
    pub resource_metadata_url: Option<String>,
    /// Requested OAuth scope.
    pub scope: Option<String>,
    /// OAuth bearer error code.
    pub error: Option<String>,
    /// OAuth bearer error description.
    pub error_description: Option<String>,
    /// OAuth bearer error documentation URL, when safe.
    pub error_uri: Option<String>,
    /// Bearer challenge realm.
    pub realm: Option<String>,
}

/// Active manual auth attempt details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuthAttemptSnapshot {
    /// Target being authenticated.
    pub target: ClientAuthTarget,
    /// Attempt id returned by the server.
    #[serde(rename = "attemptId")]
    pub attempt_id: String,
    /// URL to open for browser-based auth.
    #[serde(rename = "authUrl")]
    pub auth_url: String,
    /// Verification URL for device-style auth, when supplied.
    #[serde(rename = "verificationUrl")]
    pub verification_url: String,
    /// User code for device-style auth, when supplied.
    #[serde(rename = "userCode")]
    pub user_code: String,
    /// Attempt expiration timestamp in milliseconds since epoch.
    #[serde(rename = "expiresAtMs")]
    pub expires_at_ms: i64,
    /// Poll interval for device-style auth.
    #[serde(rename = "pollIntervalSeconds")]
    pub poll_interval_seconds: i32,
    /// User-facing instructions returned by the server.
    pub messages: Vec<String>,
    /// Server-selected completion mode.
    #[serde(rename = "completionMode")]
    pub completion_mode: String,
    /// Non-secret registration selection.
    #[serde(rename = "registrationMethod")]
    pub registration_method: String,
    /// Durable callback lifecycle.
    pub lifecycle: String,
    /// Sanitized callback failure reason.
    #[serde(rename = "failureReason")]
    pub failure_reason: String,
}

/// Stable snapshot of command operation results.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OperationCollectionSnapshot {
    /// Operation results by runtime-local operation key.
    pub current: std::collections::BTreeMap<OperationKey, ClientOperationView>,
}

/// Stable snapshot of one command operation result.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientOperationView {
    /// Begin auth command result.
    AuthBegin {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target being authenticated.
        target: ClientAuthTarget,
        /// Attempt details returned by the server.
        attempt: Option<AuthAttemptSnapshot>,
    },
    /// Complete auth command result.
    AuthComplete {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
    },
    /// Cancel auth command result.
    AuthCancel {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
    },
    /// Forced auth refresh command result.
    AuthRefresh {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target being refreshed.
        target: ClientAuthTarget,
    },
    /// Logout command result.
    AuthLogout {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Target logged out.
        target: ClientAuthTarget,
    },
    /// Configuration patch command result.
    ConfigPatch {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Patched configuration, when the operation completed successfully.
        config: Option<ConfigView>,
    },
    /// Configuration reset command result.
    ConfigReset {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Reset configuration, when the operation completed successfully.
        config: Option<ConfigView>,
    },
    /// Session creation command result.
    SessionCreate {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Created session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// Session deletion command result.
    SessionDelete {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Deleted session id.
        id: String,
    },
    /// Session wipe command result.
    SessionWipe {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
    },
    /// Session inference settings update command result.
    SessionInferenceSettingsUpdate {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// Session agent update command result.
    SessionAgentUpdate {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// User message submit command result.
    SessionSubmitUserMessage {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id submitted to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Structured outbound authentication failure, when present.
        #[serde(rename = "authenticationFailure")]
        authentication_failure: Option<AuthenticationFailureSnapshot>,
    },
    /// External request response command result.
    SessionRespondExternalRequest {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id responded to.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// External request id.
        #[serde(rename = "requestId")]
        request_id: String,
        /// Structured outbound authentication failure, when present.
        #[serde(rename = "authenticationFailure")]
        authentication_failure: Option<AuthenticationFailureSnapshot>,
    },
    /// Session interrupt command result.
    SessionInterrupt {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id interrupted.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Session rollback command result.
    SessionRollback {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id rolled back.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Updated session, when the operation completed successfully.
        session: Option<SessionView>,
    },
    /// Session continue command result.
    SessionContinue {
        /// Stable operation status name.
        status: &'static str,
        /// Last operation error, when present.
        error: Option<String>,
        /// Session id continued.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Rollback target continued from.
        target: RollbackTargetView,
        /// Structured outbound authentication failure, when present.
        #[serde(rename = "authenticationFailure")]
        authentication_failure: Option<AuthenticationFailureSnapshot>,
    },
}

/// Stable shell-facing rollback target view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RollbackTargetView {
    /// Turn target, when continuing or rolling back by turn.
    pub turn: Option<u64>,
    /// Timeline cursor target, when continuing or rolling back by cursor.
    pub cursor: Option<u64>,
}

impl From<&RollbackTarget> for RollbackTargetView {
    fn from(target: &RollbackTarget) -> Self {
        match target.target {
            Some(rollback_target::Target::Turn(turn)) => Self {
                turn: Some(turn),
                cursor: None,
            },
            Some(
                rollback_target::Target::Cursor(cursor)
                | rollback_target::Target::BeforeCursor(cursor),
            ) => Self {
                turn: None,
                cursor: Some(cursor),
            },
            None => Self {
                turn: None,
                cursor: None,
            },
        }
    }
}

/// Stable snapshot of configuration coordination state.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    /// Last known configuration snapshot.
    pub current: Option<ConfigView>,
    /// Resource-slot write baselines keyed by canonical path.
    pub resource_baselines: BTreeMap<String, ResourceEditBaselineView>,
    /// Configuration load operation snapshot.
    pub load: OperationSnapshot,
    /// Configuration patch operation snapshot.
    pub patch: OperationSnapshot,
    /// Configuration reset operation snapshot.
    pub reset: OperationSnapshot,
    /// Configuration watch operation snapshot.
    pub watch: OperationSnapshot,
}

/// Exact resource-slot baseline captured by an edit form.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ResourceEditBaselineView {
    /// Canonical resource path.
    pub path: String,
    /// Persisted-slot entity tag.
    pub persisted_etag: String,
    /// Catalog-observed entity tag for an absent persisted slot.
    pub absent_etag: String,
    /// Exact active YAML source.
    pub active_source: String,
}

/// Stable snapshot of status coordination state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    /// Whether the last known system status is ready.
    pub ready: bool,
    /// Status watch operation snapshot.
    pub watch: OperationSnapshot,
}

/// Stable snapshot of session coordination state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    /// Selectable session agent catalog.
    pub agents: SessionAgentCatalogSnapshot,
    /// Last known sessions.
    pub current: Vec<SessionView>,
    /// Session create operation snapshot.
    pub create: OperationSnapshot,
    /// Session list operation snapshot.
    pub list: OperationSnapshot,
    /// Session delete operation snapshot.
    pub delete: OperationSnapshot,
    /// Session wipe operation snapshot.
    pub wipe: OperationSnapshot,
    /// Session inference settings update operation snapshot.
    pub inference_settings_update: OperationSnapshot,
    /// Session agent update operation snapshot.
    pub agent_update: OperationSnapshot,
    /// User message submit operation snapshot.
    pub submit_user_message: OperationSnapshot,
    /// External request response operation snapshot.
    pub respond_external_request: OperationSnapshot,
    /// Session interrupt operation snapshot.
    pub interrupt: OperationSnapshot,
    /// Session rollback operation snapshot.
    pub rollback: OperationSnapshot,
    /// Session continue operation snapshot.
    pub continue_session: OperationSnapshot,
    /// Session watch operation snapshot.
    pub watch: OperationSnapshot,
    /// Session timelines by session id.
    pub timelines: BTreeMap<String, SessionTimelineView>,
    /// Latest resource search state by session id.
    pub template_searches: BTreeMap<String, SessionTemplateSearchSnapshot>,
}

/// Stable selectable session agent catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionAgentCatalogSnapshot {
    /// Deterministically ordered selectable agents.
    pub entries: Vec<crate::SelectableAgentView>,
    /// Server-selected default agent name.
    pub default_agent_name: String,
    /// Catalog load operation snapshot.
    pub load: OperationSnapshot,
}

/// Stable snapshot of resource search state for one session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionTemplateSearchSnapshot {
    /// Runtime-local caller request id.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Session id searched.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Search query.
    pub query: String,
    /// Stable operation status name.
    pub status: &'static str,
    /// Last search error, when present.
    pub error: Option<String>,
    /// Search options returned by the server.
    pub options: Vec<SessionTemplateOptionView>,
}

/// Stable snapshot of one session resource option.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionTemplateOptionView {
    /// Template kind name.
    pub kind: &'static str,
    /// Human-readable option label.
    pub label: String,
    /// Human-readable option detail.
    pub detail: String,
    /// Template text to insert when selected.
    pub template: String,
}

/// Stable snapshot of one operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OperationSnapshot {
    /// Stable operation status name.
    pub status: &'static str,
    /// Last operation error, when present.
    pub error: Option<String>,
    /// Runtime-local key for the latest operation of this kind.
    pub key: Option<OperationKey>,
}

/// Shared reducer plus effect executor loop used by shells.
pub struct ClientRuntime {
    reducer: Client,
    executor: GrpcEffectExecutor,
    status_watch: Option<(u64, StatusChangeStream)>,
    config_watch: Option<ConfigWatchUpdateStream>,
    session_watch: Option<SessionWatchUpdateStream>,
    routine_definition_watch: Option<RoutineDefinitionWatchStream>,
    routine_invocation_watch: Option<RoutineInvocationWatchStream>,
    session_routine_invocation_watches: BTreeMap<String, RoutineInvocationWatchStream>,
    timeline_watches: BTreeMap<TimelineWatchKey, SessionTimelineWatchUpdateStream>,
    watch_poll_offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WatchCandidate {
    Status,
    Config,
    Session,
    RoutineDefinitions,
    RoutineInvocations,
    SessionRoutineInvocations(String),
    Timeline(TimelineWatchKey),
}

#[allow(clippy::large_enum_variant)]
enum WatchResult {
    Status(u64, Option<Result<crate::SystemStatus, ClientError>>),
    Config(Option<Result<crate::ConfigWatchUpdate, ClientError>>),
    Session(Option<Result<crate::SessionWatchUpdate, ClientError>>),
    RoutineDefinitions(Option<Result<crate::ClientRoutineDefinitionSnapshot, ClientError>>),
    RoutineInvocations(Option<Result<crate::ClientRoutineInvocationWatchUpdate, ClientError>>),
    SessionRoutineInvocations(
        String,
        Option<Result<crate::ClientRoutineInvocationWatchUpdate, ClientError>>,
    ),
    Timeline(
        TimelineWatchKey,
        Option<Result<SessionTimelineWatchUpdate, ClientError>>,
    ),
}

/// Command sent to a shared client state stream loop.
#[derive(Debug)]
pub struct ClientCommandRequest {
    command: ClientCommand,
    result: Option<oneshot::Sender<Result<ClientSnapshot, ClientError>>>,
}

/// Result of advancing the shared client state stream loop once.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientStateStreamEvent {
    /// The stream produced a snapshot that should be published to the shell.
    Snapshot(Box<ClientSnapshot>),
    /// The command source has closed.
    CommandsClosed,
}

impl ClientCommandRequest {
    /// Create a command request without a result channel.
    #[must_use]
    pub const fn new(command: ClientCommand) -> Self {
        Self {
            command,
            result: None,
        }
    }

    /// Create a command request that replies with the command result.
    #[must_use]
    pub const fn with_result(
        command: ClientCommand,
        result: oneshot::Sender<Result<ClientSnapshot, ClientError>>,
    ) -> Self {
        Self {
            command,
            result: Some(result),
        }
    }
}

impl fmt::Debug for ClientRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientRuntime")
            .field("reducer", &self.reducer)
            .field("executor", &self.executor)
            .field("status_watch_active", &self.status_watch.is_some())
            .field("config_watch_active", &self.config_watch.is_some())
            .field("session_watch_active", &self.session_watch.is_some())
            .field(
                "routine_definition_watch_active",
                &self.routine_definition_watch.is_some(),
            )
            .field(
                "routine_invocation_watch_active",
                &self.routine_invocation_watch.is_some(),
            )
            .field("timeline_watch_count", &self.timeline_watches.len())
            .field("watch_poll_offset", &self.watch_poll_offset)
            .finish()
    }
}

impl ClientRuntime {
    /// Create a runtime from a runtime-owned default transport.
    #[must_use]
    pub fn with_transport(transport: crate::DefaultGrpcTransport) -> Self {
        Self::with_executor(GrpcEffectExecutor::with_transport(transport))
    }

    /// Create a runtime and restore one process-local pending outbound auth attempt.
    #[must_use]
    pub fn with_transport_and_pending_auth_attempt(
        transport: crate::DefaultGrpcTransport,
        attempt: AuthAttemptSnapshot,
    ) -> Self {
        let mut runtime = Self::with_transport(transport);
        if let Some(attempt) = pending_auth_attempt_state(attempt) {
            runtime.reducer.restore_pending_auth_attempt(attempt);
        }
        runtime
    }

    fn with_executor(executor: GrpcEffectExecutor) -> Self {
        Self {
            reducer: Client::new(),
            executor,
            status_watch: None,
            config_watch: None,
            session_watch: None,
            routine_definition_watch: None,
            routine_invocation_watch: None,
            session_routine_invocation_watches: BTreeMap::new(),
            timeline_watches: BTreeMap::new(),
            watch_poll_offset: 0,
        }
    }

    /// Create a runtime for an endpoint base URL.
    #[must_use]
    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self::with_executor(GrpcEffectExecutor::for_endpoint(endpoint))
    }

    /// Return the current reducer snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        ClientSnapshot::from_client(&self.reducer)
    }

    /// Convert this runtime into a startup/config state stream.
    #[must_use]
    pub fn into_state_stream<Sleep, SleepFuture>(self, sleep: Sleep) -> ClientStateStream
    where
        Sleep: Fn(u64) -> SleepFuture + 'static,
        SleepFuture: Future<Output = ()> + 'static,
    {
        ClientStateStream::new(self, sleep)
    }

    #[allow(clippy::future_not_send, reason = "watch streams are locally polled")]
    async fn next_watch_event(&mut self) -> ClientEvent {
        if let Some(result) = self.try_ready_watch_result() {
            return self.watch_result_event(result);
        }

        let result = self.next_pending_watch_result().await;
        self.watch_result_event(result)
    }

    fn active_watch_candidates(&self) -> Vec<WatchCandidate> {
        let mut candidates = Vec::new();

        if !self.reducer.state().permits_protected_requests() {
            return candidates;
        }

        if self.status_watch.is_some() {
            candidates.push(WatchCandidate::Status);
        }

        if self.reducer.state().config.watch.interests > 0 && self.config_watch.is_some() {
            candidates.push(WatchCandidate::Config);
        }

        if self.reducer.state().sessions.watch.interests > 0 && self.session_watch.is_some() {
            candidates.push(WatchCandidate::Session);
        }

        if self.reducer.state().routines.definition_watch.interests > 0
            && self.routine_definition_watch.is_some()
        {
            candidates.push(WatchCandidate::RoutineDefinitions);
        }
        if self.reducer.state().routines.invocation_watch.interests > 0
            && self.routine_invocation_watch.is_some()
        {
            candidates.push(WatchCandidate::RoutineInvocations);
        }
        candidates.extend(
            self.reducer
                .state()
                .routines
                .session_histories
                .iter()
                .filter(|(session_id, history)| {
                    history.watch.interests > 0
                        && self
                            .session_routine_invocation_watches
                            .contains_key(*session_id)
                })
                .map(|(session_id, _)| {
                    WatchCandidate::SessionRoutineInvocations(session_id.clone())
                }),
        );

        candidates.extend(
            self.reducer
                .state()
                .timeline_watch_keys()
                .into_iter()
                .filter(|key| self.timeline_watches.contains_key(key))
                .map(WatchCandidate::Timeline),
        );

        if !candidates.is_empty() {
            let offset = self.watch_poll_offset % candidates.len();
            candidates.rotate_left(offset);
        }

        candidates
    }

    fn try_ready_watch_result(&mut self) -> Option<WatchResult> {
        for candidate in self.active_watch_candidates() {
            let result = match candidate {
                WatchCandidate::Status => self.status_watch.as_mut().and_then(|(epoch, stream)| {
                    stream
                        .next()
                        .now_or_never()
                        .map(|result| WatchResult::Status(*epoch, result))
                }),
                WatchCandidate::Config => self
                    .config_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::Config),
                WatchCandidate::Session => self
                    .session_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::Session),
                WatchCandidate::RoutineDefinitions => self
                    .routine_definition_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::RoutineDefinitions),
                WatchCandidate::RoutineInvocations => self
                    .routine_invocation_watch
                    .as_mut()
                    .and_then(|stream| stream.next().now_or_never())
                    .map(WatchResult::RoutineInvocations),
                WatchCandidate::SessionRoutineInvocations(session_id) => self
                    .session_routine_invocation_watches
                    .get_mut(&session_id)
                    .and_then(|stream| stream.next().now_or_never())
                    .map(|result| WatchResult::SessionRoutineInvocations(session_id, result)),
                WatchCandidate::Timeline(key) => self
                    .timeline_watches
                    .get_mut(&key)
                    .and_then(|stream| stream.next().now_or_never())
                    .map(|result| WatchResult::Timeline(key, result)),
            };

            if result.is_some() {
                self.watch_poll_offset = self.watch_poll_offset.saturating_add(1);
                return result;
            }
        }

        None
    }

    #[allow(clippy::future_not_send, reason = "watch streams are locally polled")]
    async fn next_pending_watch_result(&mut self) -> WatchResult {
        let mut futures: Vec<LocalBoxFuture<'_, WatchResult>> = Vec::new();

        if let Some((epoch, stream)) = &mut self.status_watch {
            let epoch = *epoch;
            futures.push(Box::pin(async move {
                WatchResult::Status(epoch, stream.next().await)
            }));
        }

        if self.reducer.state().config.watch.interests > 0
            && let Some(stream) = &mut self.config_watch
        {
            futures.push(Box::pin(
                async move { WatchResult::Config(stream.next().await) },
            ));
        }

        if self.reducer.state().sessions.watch.interests > 0
            && let Some(stream) = &mut self.session_watch
        {
            futures.push(Box::pin(async move {
                WatchResult::Session(stream.next().await)
            }));
        }

        if self.reducer.state().routines.definition_watch.interests > 0
            && let Some(stream) = &mut self.routine_definition_watch
        {
            futures.push(Box::pin(async move {
                WatchResult::RoutineDefinitions(stream.next().await)
            }));
        }
        if self.reducer.state().routines.invocation_watch.interests > 0
            && let Some(stream) = &mut self.routine_invocation_watch
        {
            futures.push(Box::pin(async move {
                WatchResult::RoutineInvocations(stream.next().await)
            }));
        }

        let active_session_ids = self
            .reducer
            .state()
            .routines
            .session_histories
            .iter()
            .filter(|(_, history)| history.watch.interests > 0)
            .map(|(session_id, _)| session_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for (session_id, stream) in self
            .session_routine_invocation_watches
            .iter_mut()
            .filter(|(session_id, _)| active_session_ids.contains(*session_id))
        {
            let session_id = session_id.clone();
            futures.push(Box::pin(async move {
                WatchResult::SessionRoutineInvocations(session_id, stream.next().await)
            }));
        }

        let active_timeline_keys = self
            .reducer
            .state()
            .timeline_watch_keys()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        for (key, stream) in self
            .timeline_watches
            .iter_mut()
            .filter(|(key, _)| active_timeline_keys.contains(*key))
        {
            let key = key.clone();
            futures.push(Box::pin(async move {
                WatchResult::Timeline(key, stream.next().await)
            }));
        }

        if futures.is_empty() {
            return WatchResult::Status(0, Some(Ok(crate::SystemStatus::default())));
        }

        let (result, index, _) = select_all(futures).await;
        self.watch_poll_offset = self.watch_poll_offset.saturating_add(index + 1);
        result
    }

    fn watch_result_event(&mut self, result: WatchResult) -> ClientEvent {
        match result {
            WatchResult::Status(epoch, Some(Ok(status))) => {
                ClientEvent::Status(StatusEvent::WatchChangedForEpoch { epoch, status })
            }
            WatchResult::Status(epoch, Some(Err(error))) => {
                self.status_watch = None;
                ClientEvent::Status(StatusEvent::WatchFailedForEpoch { epoch, error })
            }
            WatchResult::Status(epoch, None) => {
                self.status_watch = None;
                ClientEvent::Status(StatusEvent::WatchEndedForEpoch { epoch })
            }
            WatchResult::Config(Some(Ok(update))) => {
                ClientEvent::Config(ConfigEvent::WatchChanged(update))
            }
            WatchResult::Config(Some(Err(error))) => {
                self.config_watch = None;
                ClientEvent::Config(ConfigEvent::WatchFailed(error))
            }
            WatchResult::Config(None) => {
                self.config_watch = None;
                ClientEvent::Config(ConfigEvent::WatchEnded)
            }
            WatchResult::Session(Some(Ok(update))) => {
                ClientEvent::Session(SessionEvent::WatchChanged(update))
            }
            WatchResult::Session(Some(Err(error))) => {
                self.session_watch = None;
                ClientEvent::Session(SessionEvent::WatchFailed(error))
            }
            WatchResult::Session(None) => {
                self.session_watch = None;
                ClientEvent::Session(SessionEvent::WatchEnded)
            }
            WatchResult::RoutineDefinitions(Some(Ok(update))) => {
                ClientEvent::Routine(RoutineEvent::DefinitionWatchChanged(update))
            }
            WatchResult::RoutineDefinitions(Some(Err(error))) => {
                self.routine_definition_watch = None;
                ClientEvent::Routine(RoutineEvent::DefinitionWatchFailed(error))
            }
            WatchResult::RoutineDefinitions(None) => {
                self.routine_definition_watch = None;
                ClientEvent::Routine(RoutineEvent::DefinitionWatchEnded)
            }
            WatchResult::RoutineInvocations(Some(Ok(update))) => {
                ClientEvent::Routine(RoutineEvent::InvocationWatchChanged(update))
            }
            WatchResult::RoutineInvocations(Some(Err(error))) => {
                self.routine_invocation_watch = None;
                ClientEvent::Routine(RoutineEvent::InvocationWatchFailed(error))
            }
            WatchResult::RoutineInvocations(None) => {
                self.routine_invocation_watch = None;
                ClientEvent::Routine(RoutineEvent::InvocationWatchEnded)
            }
            WatchResult::SessionRoutineInvocations(session_id, Some(Ok(update))) => {
                ClientEvent::Routine(RoutineEvent::SessionInvocationWatchChanged {
                    session_id,
                    update,
                })
            }
            WatchResult::SessionRoutineInvocations(session_id, Some(Err(error))) => {
                self.session_routine_invocation_watches.remove(&session_id);
                ClientEvent::Routine(RoutineEvent::SessionInvocationWatchFailed {
                    session_id,
                    error,
                })
            }
            WatchResult::SessionRoutineInvocations(session_id, None) => {
                self.session_routine_invocation_watches.remove(&session_id);
                ClientEvent::Routine(RoutineEvent::SessionInvocationWatchEnded { session_id })
            }
            WatchResult::Timeline(key, Some(Ok(update))) => {
                ClientEvent::Session(SessionEvent::TimelineWatchChanged { key, update })
            }
            WatchResult::Timeline(key, Some(Err(error))) => {
                self.timeline_watches.remove(&key);
                ClientEvent::Session(SessionEvent::TimelineWatchFailed { key, error })
            }
            WatchResult::Timeline(key, None) => {
                self.timeline_watches.remove(&key);
                ClientEvent::Session(SessionEvent::TimelineWatchEnded { key })
            }
        }
    }

    fn suspend_protected_watches(&mut self) {
        if self.reducer.state().permits_protected_requests() {
            return;
        }
        self.status_watch = None;
        self.config_watch = None;
        self.session_watch = None;
        self.routine_definition_watch = None;
        self.routine_invocation_watch = None;
        self.session_routine_invocation_watches.clear();
        self.timeline_watches.clear();
    }
}

/// Shared client state stream consumed by shell runtimes.
pub struct ClientStateStream {
    runtime: ClientRuntime,
    phase: ClientStateStreamPhase,
    effects: Vec<QueuedEffect>,
    in_flight: Option<InFlightEffect>,
    sleep: ClientSleep,
}

#[derive(Clone, Debug)]
struct QueuedEffect {
    access_generation: u64,
    effect: ClientEffect,
}

struct InFlightEffect {
    access_generation: u64,
    effect: ClientEffect,
    future: LocalBoxFuture<'static, ExecutedEffect>,
}

enum ExecutedEffect {
    Events(Vec<ClientEvent>),
    StatusWatch {
        epoch: u64,
        result: Result<StatusChangeStream, ClientError>,
    },
    ConfigWatch {
        open_id: u64,
        result: Result<ConfigWatchUpdateStream, ClientError>,
    },
    SessionWatch {
        open_id: u64,
        result: Result<SessionWatchUpdateStream, ClientError>,
    },
    RoutineDefinitionWatch {
        open_id: u64,
        result: Result<RoutineDefinitionWatchStream, ClientError>,
    },
    RoutineInvocationWatch {
        open_id: u64,
        session_id: Option<String>,
        result: Result<RoutineInvocationWatchStream, ClientError>,
    },
    TimelineWatch {
        key: TimelineWatchKey,
        open_id: u64,
        result: Result<SessionTimelineWatchUpdateStream, ClientError>,
    },
}

impl fmt::Debug for ClientStateStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientStateStream")
            .field("runtime", &self.runtime)
            .field("phase", &self.phase)
            .field("effects", &self.effects)
            .finish_non_exhaustive()
    }
}

impl ClientStateStream {
    fn new<Sleep, SleepFuture>(mut runtime: ClientRuntime, sleep: Sleep) -> Self
    where
        Sleep: Fn(u64) -> SleepFuture + 'static,
        SleepFuture: Future<Output = ()> + 'static,
    {
        let poll = runtime
            .reducer
            .state()
            .auth
            .active_attempt
            .as_ref()
            .map(|attempt| attempt.attempt_id.clone())
            .map(|attempt_id| runtime.reducer.poll_auth_attempt(attempt_id));
        let mut stream = Self {
            runtime,
            phase: ClientStateStreamPhase::WaitForSystemReady,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(move |delay_ms| Box::pin(sleep(delay_ms))),
        };
        if let Some(update) = poll {
            stream.extend_effects(&update);
        }
        stream
    }

    /// Return the next state snapshot from shared startup/config orchestration.
    #[allow(
        clippy::future_not_send,
        reason = "client state is locally polled by UI shells"
    )]
    pub async fn next(&mut self) -> ClientSnapshot {
        loop {
            if self.in_flight.is_none()
                && let Some(queued) = self.effects.pop()
            {
                self.in_flight = Some(self.start_effect(queued));
            }
            if let Some(in_flight) = self.in_flight.as_mut() {
                let executed = in_flight.future.as_mut().await;
                let access_generation = in_flight.access_generation;
                let effect = in_flight.effect.clone();
                self.in_flight = None;
                self.complete_effect(access_generation, &effect, executed);
                return self.snapshot();
            }

            if matches!(
                self.runtime.reducer.state().access.status,
                AccessStatus::LoginRequired
                    | AccessStatus::LoginInProgress
                    | AccessStatus::LoginFailed
            ) {
                pending::<()>().await;
            }

            match self.phase {
                ClientStateStreamPhase::WaitForSystemReady
                    if self
                        .runtime
                        .reducer
                        .state()
                        .status
                        .current
                        .is_some_and(|status| status.ready) =>
                {
                    self.phase = ClientStateStreamPhase::Ready;
                }
                ClientStateStreamPhase::WaitForSystemReady => {
                    if self.runtime.status_watch.is_some() {
                        self.apply_next_watch_event().await;
                    } else {
                        let update = self.runtime.reducer.maintain_watches();
                        self.extend_effects(&update);
                    }
                    return self.snapshot();
                }
                ClientStateStreamPhase::Ready => {
                    if self.runtime.reducer.state().status.watch.status == OperationStatus::Received
                        && self.runtime.reducer.state().config.watch.interests == 0
                        && self.runtime.reducer.state().sessions.watch.interests == 0
                        && self
                            .runtime
                            .reducer
                            .state()
                            .routines
                            .definition_watch
                            .interests
                            == 0
                        && self
                            .runtime
                            .reducer
                            .state()
                            .routines
                            .invocation_watch
                            .interests
                            == 0
                        && self
                            .runtime
                            .reducer
                            .state()
                            .routines
                            .session_histories
                            .is_empty()
                        && self
                            .runtime
                            .reducer
                            .state()
                            .timeline_watch_keys()
                            .is_empty()
                    {
                        (self.sleep)(READY_IDLE_DELAY_MS).await;
                        let update = self.runtime.reducer.maintain_watches();
                        self.extend_effects(&update);
                        return self.snapshot();
                    }

                    if self.runtime.config_watch.is_some()
                        || self.runtime.session_watch.is_some()
                        || self.runtime.routine_definition_watch.is_some()
                        || self.runtime.routine_invocation_watch.is_some()
                        || !self.runtime.session_routine_invocation_watches.is_empty()
                        || !self.runtime.timeline_watches.is_empty()
                        || self.runtime.status_watch.is_some()
                    {
                        self.apply_next_watch_event().await;
                        return self.snapshot();
                    }

                    let update = self.runtime.reducer.maintain_watches();
                    self.extend_effects(&update);
                    return self.snapshot();
                }
            }
        }
    }

    /// Advance until either a client snapshot changes or a command arrives.
    #[allow(
        clippy::future_not_send,
        reason = "client state is locally polled by UI shells"
    )]
    pub async fn next_or_dispatch<CommandFuture>(
        &mut self,
        command: CommandFuture,
    ) -> ClientStateStreamEvent
    where
        CommandFuture: Future<Output = Option<ClientCommandRequest>>,
    {
        let request = {
            let next = self.next();
            pin_mut!(next);
            pin_mut!(command);

            match select(command, next).await {
                Either::Left((Some(request), _pending_next)) => request,
                Either::Left((None, _)) => return ClientStateStreamEvent::CommandsClosed,
                Either::Right((snapshot, _)) => {
                    return ClientStateStreamEvent::Snapshot(Box::new(snapshot));
                }
            }
        };

        ClientStateStreamEvent::Snapshot(Box::new(self.dispatch_request(request)))
    }

    /// Return the current stream snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClientSnapshot {
        self.runtime.snapshot()
    }

    fn dispatch_request(&mut self, request: ClientCommandRequest) -> ClientSnapshot {
        let result = self
            .begin_command(request.command)
            .map(|()| self.snapshot());
        let snapshot = result.clone().unwrap_or_else(|_| self.snapshot());

        if let Some(reply) = request.result {
            drop(reply.send(result));
        }

        snapshot
    }

    #[allow(clippy::too_many_lines)]
    fn begin_command(&mut self, command: ClientCommand) -> Result<(), ClientError> {
        if !self.runtime.reducer.state().permits_protected_requests()
            && !command_allowed_without_access(&command)
        {
            return Err(ClientError::waltr_login_required("Waltr login required"));
        }

        let access_generation = self.runtime.reducer.state().access_generation;
        let update = match command {
            ClientCommand::StartWaltrLogin => self.runtime.reducer.start_waltr_login(),
            ClientCommand::CompleteWaltrLogin { attempt_id } => {
                self.runtime.reducer.complete_waltr_login(attempt_id)
            }
            ClientCommand::FailWaltrLogin { attempt_id, error } => {
                self.runtime.reducer.fail_waltr_login(attempt_id, error)
            }
            ClientCommand::CancelWaltrLogin { attempt_id } => {
                self.runtime.reducer.cancel_waltr_login(attempt_id)
            }
            ClientCommand::ReprobeWaltrAccess => self.runtime.reducer.reprobe_waltr_access(),
            ClientCommand::LogoutWaltr => self.runtime.reducer.logout_waltr(),
            ClientCommand::WaltrAccessExpired { error } => {
                self.runtime.reducer.expire_waltr_access(error)
            }
            ClientCommand::RequestAuthStatuses
            | ClientCommand::BeginAuth { .. }
            | ClientCommand::CompleteAuth { .. }
            | ClientCommand::PollAuthAttempt { .. }
            | ClientCommand::CancelAuth { .. }
            | ClientCommand::RefreshAuth { .. }
            | ClientCommand::LogoutAuth { .. } => self.begin_auth_command(command),
            ClientCommand::RequestConfig
            | ClientCommand::ResetConfig
            | ClientCommand::AddProvider { .. }
            | ClientCommand::UpdateProvider { .. }
            | ClientCommand::DeleteProvider { .. }
            | ClientCommand::AddMcpServer { .. }
            | ClientCommand::UpdateMcpServer { .. }
            | ClientCommand::DeleteMcpServer { .. }
            | ClientCommand::SetInferenceDefaults { .. }
            | ClientCommand::RegisterConfigWatchInterest
            | ClientCommand::ReleaseConfigWatchInterest => self.begin_config_command(command)?,
            ClientCommand::SearchCatalogModels { .. }
            | ClientCommand::SuggestCatalogMatches { .. } => self.begin_catalog_command(command),
            ClientCommand::RegisterRoutineDefinitionWatchInterest
            | ClientCommand::ReleaseRoutineDefinitionWatchInterest
            | ClientCommand::RegisterRoutineInvocationWatchInterest
            | ClientCommand::ReleaseRoutineInvocationWatchInterest
            | ClientCommand::RegisterSessionRoutineInvocationWatchInterest { .. }
            | ClientCommand::ReleaseSessionRoutineInvocationWatchInterest { .. }
            | ClientCommand::StartManualRoutine { .. }
            | ClientCommand::LoadOlderRoutineInvocations { .. }
            | ClientCommand::LoadRoutineInvocationDetail { .. } => {
                self.begin_routine_command(command)
            }
            ClientCommand::CreateSession { .. }
            | ClientCommand::RequestSessionAgents
            | ClientCommand::DeleteSession { .. }
            | ClientCommand::WipeSessions
            | ClientCommand::UpdateSessionInferenceSettings { .. }
            | ClientCommand::UpdateSessionAgent { .. }
            | ClientCommand::SearchSessionTemplates { .. }
            | ClientCommand::RegisterSessionWatchInterest
            | ClientCommand::ReleaseSessionWatchInterest
            | ClientCommand::SubmitUserMessage { .. }
            | ClientCommand::RespondExternalRequest { .. }
            | ClientCommand::InterruptSession { .. }
            | ClientCommand::RollbackSessionToTurn { .. }
            | ClientCommand::RollbackSessionToCursor { .. }
            | ClientCommand::RollbackSessionBeforeCursor { .. }
            | ClientCommand::ContinueSessionFromTurn { .. }
            | ClientCommand::ContinueSessionFromCursor { .. } => {
                self.begin_session_command(command)
            }
            ClientCommand::RequestSessionTimeline { .. }
            | ClientCommand::RegisterSessionTimelineWatchInterest { .. }
            | ClientCommand::ReleaseSessionTimelineWatchInterest { .. } => {
                self.begin_timeline_command(command)
            }
            ClientCommand::WatchStatus => self.runtime.reducer.request_status_watch(),
            ClientCommand::WaitForSystemReady => self.runtime.reducer.maintain_watches(),
        };

        if self.runtime.reducer.state().access_generation != access_generation {
            self.effects.clear();
            self.in_flight = None;
            self.runtime.status_watch = None;
            self.runtime.config_watch = None;
            self.runtime.session_watch = None;
            self.runtime.routine_definition_watch = None;
            self.runtime.routine_invocation_watch = None;
            self.runtime.session_routine_invocation_watches.clear();
            self.runtime.timeline_watches.clear();
        }
        self.extend_effects(&update);
        self.suspend_if_access_lost();
        Ok(())
    }

    fn begin_auth_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::RequestAuthStatuses => self.runtime.reducer.request_auth_statuses(),
            ClientCommand::BeginAuth { target } => {
                self.cancel_auth_poll_effects(None);
                self.runtime.reducer.begin_auth(target.into_auth_target())
            }
            ClientCommand::CompleteAuth { attempt_id, input } => {
                self.cancel_auth_poll_effects(Some(&attempt_id));
                self.runtime.reducer.complete_auth(attempt_id, input)
            }
            ClientCommand::PollAuthAttempt { attempt_id } => {
                self.cancel_auth_poll_effects(Some(&attempt_id));
                self.runtime.reducer.poll_auth_attempt(attempt_id)
            }
            ClientCommand::CancelAuth { attempt_id } => {
                self.cancel_auth_poll_effects(Some(&attempt_id));
                self.runtime.reducer.cancel_auth(attempt_id)
            }
            ClientCommand::RefreshAuth { target } => {
                self.runtime.reducer.refresh_auth(target.into_auth_target())
            }
            ClientCommand::LogoutAuth { target } => {
                let target = target.into_auth_target();
                if self
                    .runtime
                    .reducer
                    .state()
                    .auth
                    .active_attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.target == target)
                {
                    self.cancel_auth_poll_effects(None);
                }
                self.runtime.reducer.logout_auth(target)
            }
            _ => unreachable!("non-auth command routed to auth handler"),
        }
    }

    fn begin_config_command(
        &mut self,
        command: ClientCommand,
    ) -> Result<ClientUpdate, ClientError> {
        Ok(match command {
            ClientCommand::RequestConfig => self.runtime.reducer.request_config(),
            ClientCommand::ResetConfig => self.runtime.reducer.reset_config()?,
            ClientCommand::AddProvider { provider } => {
                self.runtime.reducer.add_provider(provider.into())?
            }
            ClientCommand::UpdateProvider {
                original_name,
                original,
                baseline,
                provider,
            } => self.runtime.reducer.update_provider(
                &original_name,
                (*original).into(),
                provider.into(),
                &baseline,
            )?,
            ClientCommand::DeleteProvider { name } => {
                self.runtime.reducer.delete_provider(&name)?
            }
            ClientCommand::AddMcpServer { server } => {
                self.runtime.reducer.add_mcp_server(server.into())?
            }
            ClientCommand::UpdateMcpServer {
                original_name,
                original,
                baseline,
                server,
            } => self.runtime.reducer.update_mcp_server(
                &original_name,
                (*original).into(),
                server.into(),
                &baseline,
            )?,
            ClientCommand::DeleteMcpServer { name } => {
                self.runtime.reducer.delete_mcp_server(&name)?
            }
            ClientCommand::SetInferenceDefaults { defaults } => self
                .runtime
                .reducer
                .set_inference_defaults(defaults.map(Into::into))?,
            ClientCommand::RegisterConfigWatchInterest => {
                self.runtime.reducer.register_config_watch_interest()
            }
            ClientCommand::ReleaseConfigWatchInterest => {
                let update = self.runtime.reducer.release_config_watch_interest();
                if self.runtime.reducer.state().config.watch.interests == 0 {
                    self.runtime.config_watch = None;
                    self.cancel_effects(|effect| {
                        matches!(effect, ClientEffect::Config(ConfigEffect::Watch { .. }))
                    });
                }
                update
            }
            _ => unreachable!("non-config command routed to config handler"),
        })
    }

    fn begin_catalog_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::SearchCatalogModels {
                request_id,
                query,
                provider_id,
                limit,
            } => {
                self.cancel_effects(|effect| {
                    matches!(
                        effect,
                        ClientEffect::Catalog(CatalogEffect::SearchModels { .. })
                    )
                });
                self.runtime
                    .reducer
                    .search_catalog_models(request_id, query, provider_id, limit)
            }
            ClientCommand::SuggestCatalogMatches {
                request_id,
                provider_key,
                model_key,
                limit,
            } => {
                self.cancel_effects(|effect| {
                    matches!(
                        effect,
                        ClientEffect::Catalog(CatalogEffect::SuggestMatches { .. })
                    )
                });
                self.runtime.reducer.suggest_catalog_matches(
                    request_id,
                    provider_key,
                    model_key,
                    limit,
                )
            }
            _ => unreachable!("non-catalog command routed to catalog handler"),
        }
    }

    fn begin_routine_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::RegisterRoutineDefinitionWatchInterest => self
                .runtime
                .reducer
                .register_routine_definition_watch_interest(),
            ClientCommand::ReleaseRoutineDefinitionWatchInterest => {
                let update = self
                    .runtime
                    .reducer
                    .release_routine_definition_watch_interest();
                if self
                    .runtime
                    .reducer
                    .state()
                    .routines
                    .definition_watch
                    .interests
                    == 0
                {
                    self.runtime.routine_definition_watch = None;
                    self.cancel_effects(|effect| {
                        matches!(
                            effect,
                            ClientEffect::Routine(RoutineEffect::WatchDefinitions { .. })
                        )
                    });
                }
                update
            }
            ClientCommand::RegisterRoutineInvocationWatchInterest => self
                .runtime
                .reducer
                .register_routine_invocation_watch_interest(),
            ClientCommand::ReleaseRoutineInvocationWatchInterest => {
                let update = self
                    .runtime
                    .reducer
                    .release_routine_invocation_watch_interest();
                if self
                    .runtime
                    .reducer
                    .state()
                    .routines
                    .invocation_watch
                    .interests
                    == 0
                {
                    self.runtime.routine_invocation_watch = None;
                    self.cancel_effects(|effect| {
                        matches!(
                            effect,
                            ClientEffect::Routine(RoutineEffect::WatchInvocations {
                                session_id: None,
                                ..
                            })
                        )
                    });
                }
                update
            }
            ClientCommand::RegisterSessionRoutineInvocationWatchInterest { session_id } => self
                .runtime
                .reducer
                .register_session_routine_invocation_watch_interest(session_id),
            ClientCommand::ReleaseSessionRoutineInvocationWatchInterest { session_id } => {
                let update = self
                    .runtime
                    .reducer
                    .release_session_routine_invocation_watch_interest(&session_id);
                if !self
                    .runtime
                    .reducer
                    .state()
                    .routines
                    .session_histories
                    .contains_key(&session_id)
                {
                    self.runtime
                        .session_routine_invocation_watches
                        .remove(&session_id);
                    self.cancel_effects(|effect| {
                        matches!(
                            effect,
                            ClientEffect::Routine(RoutineEffect::WatchInvocations {
                                session_id: Some(watched),
                                ..
                            }) if watched == &session_id
                        )
                    });
                }
                update
            }
            ClientCommand::StartManualRoutine {
                operation_key,
                request_id,
                routine_name,
                trigger_name,
                source_session_id,
                inputs,
            } => self.runtime.reducer.start_manual_routine(
                operation_key,
                request_id,
                routine_name,
                trigger_name,
                source_session_id,
                inputs,
            ),
            ClientCommand::LoadOlderRoutineInvocations {
                request_id,
                page_size,
            } => self
                .runtime
                .reducer
                .load_older_routine_invocations(request_id, page_size),
            ClientCommand::LoadRoutineInvocationDetail {
                request_id,
                invocation_id,
            } => self
                .runtime
                .reducer
                .load_routine_invocation_detail(request_id, invocation_id),
            _ => unreachable!("non-Routine command routed to Routine handler"),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn begin_session_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::CreateSession { agent_name } => {
                self.runtime.reducer.create_session(agent_name)
            }
            ClientCommand::RequestSessionAgents => self.runtime.reducer.request_session_agents(),
            ClientCommand::DeleteSession { id } => self.runtime.reducer.delete_session(id),
            ClientCommand::WipeSessions => self.runtime.reducer.wipe_sessions(),
            ClientCommand::UpdateSessionInferenceSettings {
                id,
                provider_key,
                model_id,
                reasoning_effort,
            } => self.runtime.reducer.update_session_inference_settings(
                id,
                InferenceSettingsRequest {
                    provider_key,
                    model_id,
                    reasoning_effort,
                },
            ),
            ClientCommand::UpdateSessionAgent { id, agent_name } => {
                self.runtime.reducer.update_session_agent(id, agent_name)
            }
            ClientCommand::SearchSessionTemplates {
                request_id,
                session_id,
                query,
                limit,
                kinds,
                target,
                cursor,
            } => {
                self.cancel_effects(|effect| {
                    matches!(
                        effect,
                        ClientEffect::Session(SessionEffect::SearchTemplates {
                            session_id: pending_session_id,
                            ..
                        }) if pending_session_id == &session_id
                    )
                });
                self.runtime.reducer.search_session_templates(
                    request_id, session_id, query, limit, kinds, target, cursor,
                )
            }
            ClientCommand::RegisterSessionWatchInterest => {
                self.runtime.reducer.register_session_watch_interest()
            }
            ClientCommand::ReleaseSessionWatchInterest => {
                let update = self.runtime.reducer.release_session_watch_interest();
                if self.runtime.reducer.state().sessions.watch.interests == 0 {
                    self.runtime.session_watch = None;
                    self.cancel_effects(|effect| {
                        matches!(
                            effect,
                            ClientEffect::Session(
                                SessionEffect::Watch { .. } | SessionEffect::ListAgents { .. }
                            )
                        )
                    });
                }
                update
            }
            ClientCommand::SubmitUserMessage { session_id, text } => {
                self.runtime.reducer.submit_user_message(session_id, text)
            }
            ClientCommand::RespondExternalRequest {
                session_id,
                request_id,
                response_json,
            } => {
                self.runtime
                    .reducer
                    .respond_external_request(session_id, request_id, response_json)
            }
            ClientCommand::InterruptSession { session_id } => {
                self.runtime.reducer.interrupt_session(session_id)
            }
            ClientCommand::RollbackSessionToTurn { session_id, turn } => {
                self.runtime.reducer.rollback_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Turn(turn)),
                    },
                )
            }
            ClientCommand::RollbackSessionToCursor { session_id, cursor } => {
                self.runtime.reducer.rollback_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Cursor(cursor)),
                    },
                )
            }
            ClientCommand::RollbackSessionBeforeCursor { session_id, cursor } => {
                self.runtime.reducer.rollback_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::BeforeCursor(cursor)),
                    },
                )
            }
            ClientCommand::ContinueSessionFromTurn { session_id, turn } => {
                self.runtime.reducer.continue_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Turn(turn)),
                    },
                )
            }
            ClientCommand::ContinueSessionFromCursor { session_id, cursor } => {
                self.runtime.reducer.continue_session(
                    session_id,
                    RollbackTarget {
                        target: Some(rollback_target::Target::Cursor(cursor)),
                    },
                )
            }
            _ => unreachable!("non-session command routed to session handler"),
        }
    }

    fn begin_timeline_command(&mut self, command: ClientCommand) -> ClientUpdate {
        match command {
            ClientCommand::RequestSessionTimeline {
                session_id,
                direction,
                cursor,
                limit,
                channels,
            } => {
                let timeline = self
                    .runtime
                    .reducer
                    .state()
                    .sessions
                    .timelines
                    .get(&session_id);
                if direction == ClientTimelineDirection::Backward
                    && timeline.is_some_and(|timeline| {
                        timeline.projection_mode == crate::SessionTimelineProjectionMode::Effective
                            && timeline.projection_revision.is_some()
                            && !timeline.has_more
                    })
                {
                    return ClientUpdate::default();
                }
                let cursor = if direction == ClientTimelineDirection::Backward {
                    match timeline.filter(|timeline| {
                        timeline.projection_mode == crate::SessionTimelineProjectionMode::Effective
                            && timeline.projection_revision.is_some()
                    }) {
                        Some(timeline) => timeline.raw_page_boundary,
                        None => cursor,
                    }
                } else {
                    cursor
                };
                self.runtime
                    .reducer
                    .request_session_timeline_with_projection(
                        session_id.clone(),
                        Some(TimelineQuery {
                            direction: timeline_direction(direction) as i32,
                            cursor,
                            limit,
                            channels,
                        }),
                        crate::SessionTimelineProjectionMode::Effective,
                    )
            }
            ClientCommand::RegisterSessionTimelineWatchInterest {
                session_id,
                after_cursor,
                channels,
                initial_limit,
            } => self
                .runtime
                .reducer
                .register_session_timeline_watch_interest_with_projection(
                    session_id,
                    after_cursor,
                    channels,
                    initial_limit,
                    crate::SessionTimelineProjectionMode::Effective,
                ),
            ClientCommand::ReleaseSessionTimelineWatchInterest { session_id } => {
                let update = self
                    .runtime
                    .reducer
                    .release_session_timeline_watch_interest(session_id.clone());
                let active = self
                    .runtime
                    .reducer
                    .state()
                    .sessions
                    .timelines
                    .get(&session_id)
                    .is_some_and(|timeline| timeline.watch.interests > 0);
                if !active {
                    self.runtime
                        .timeline_watches
                        .retain(|key, _| key.session_id != session_id);
                    self.cancel_effects(|effect| {
                        matches!(
                            effect,
                            ClientEffect::Session(SessionEffect::WatchTimeline { key, .. })
                                if key.session_id == session_id
                        )
                    });
                }
                update
            }
            _ => unreachable!("non-timeline command routed to timeline handler"),
        }
    }

    fn start_effect(&self, queued: QueuedEffect) -> InFlightEffect {
        let executor = self.runtime.executor.clone();
        let sleep = Rc::clone(&self.sleep);
        let effect = queued.effect.clone();
        let future = async move {
            match queued.effect {
                ClientEffect::Status(StatusEffect::Watch { epoch }) => {
                    ExecutedEffect::StatusWatch {
                        epoch,
                        result: executor.watch_status().await,
                    }
                }
                ClientEffect::Config(ConfigEffect::Watch { open_id }) => {
                    ExecutedEffect::ConfigWatch {
                        open_id,
                        result: executor.watch_config_updates().await,
                    }
                }
                ClientEffect::Session(SessionEffect::Watch { open_id }) => {
                    ExecutedEffect::SessionWatch {
                        open_id,
                        result: executor.watch_session_updates().await,
                    }
                }
                ClientEffect::Session(SessionEffect::WatchTimeline { key, open_id }) => {
                    let result = executor
                        .watch_session_timeline_updates(
                            key.session_id.clone(),
                            key.after_cursor,
                            key.channels.clone(),
                            key.initial_limit,
                            key.projection_mode,
                        )
                        .await;
                    ExecutedEffect::TimelineWatch {
                        key,
                        open_id,
                        result,
                    }
                }
                ClientEffect::Routine(RoutineEffect::WatchDefinitions {
                    open_id,
                    request_id,
                }) => ExecutedEffect::RoutineDefinitionWatch {
                    open_id,
                    result: executor.watch_routine_definition_updates(request_id).await,
                },
                ClientEffect::Routine(RoutineEffect::WatchInvocations {
                    open_id,
                    request_id,
                    after_sequence,
                    metadata_filters,
                    session_id,
                }) => ExecutedEffect::RoutineInvocationWatch {
                    open_id,
                    session_id,
                    result: executor
                        .watch_routine_invocation_updates(
                            request_id,
                            after_sequence,
                            metadata_filters,
                        )
                        .await,
                },
                effect => ExecutedEffect::Events(
                    executor
                        .execute_effect(effect, move |delay_ms| sleep(delay_ms))
                        .await,
                ),
            }
        }
        .boxed_local();
        InFlightEffect {
            access_generation: queued.access_generation,
            effect,
            future,
        }
    }

    fn complete_effect(
        &mut self,
        access_generation: u64,
        effect: &ClientEffect,
        executed: ExecutedEffect,
    ) {
        if self.runtime.reducer.state().access_generation != access_generation
            || !self.effect_completion_is_current(effect, &executed)
        {
            return;
        }
        let events = match executed {
            ExecutedEffect::Events(events) => events,
            ExecutedEffect::StatusWatch { epoch, result } => match result {
                Ok(stream) => {
                    self.runtime.status_watch = Some((epoch, stream));
                    vec![ClientEvent::Status(StatusEvent::WatchStartedForEpoch {
                        epoch,
                    })]
                }
                Err(error) => vec![ClientEvent::Status(StatusEvent::WatchFailedForEpoch {
                    epoch,
                    error,
                })],
            },
            ExecutedEffect::ConfigWatch { open_id, result } => match result {
                Ok(stream) => {
                    self.runtime.config_watch = Some(stream);
                    vec![ClientEvent::Config(ConfigEvent::WatchStartedForOpen {
                        open_id,
                    })]
                }
                Err(error) => vec![ClientEvent::Config(ConfigEvent::WatchOpenFailed {
                    open_id,
                    error,
                })],
            },
            ExecutedEffect::SessionWatch { open_id, result } => match result {
                Ok(stream) => {
                    self.runtime.session_watch = Some(stream);
                    vec![ClientEvent::Session(SessionEvent::WatchStartedForOpen {
                        open_id,
                    })]
                }
                Err(error) => vec![ClientEvent::Session(SessionEvent::WatchOpenFailed {
                    open_id,
                    error,
                })],
            },
            ExecutedEffect::RoutineDefinitionWatch { open_id, result } => match result {
                Ok(stream) => {
                    self.runtime.routine_definition_watch = Some(stream);
                    vec![ClientEvent::Routine(
                        RoutineEvent::DefinitionWatchStartedForOpen { open_id },
                    )]
                }
                Err(error) => vec![ClientEvent::Routine(
                    RoutineEvent::DefinitionWatchOpenFailed { open_id, error },
                )],
            },
            ExecutedEffect::RoutineInvocationWatch {
                open_id,
                session_id,
                result,
            } => match (session_id, result) {
                (Some(session_id), Ok(stream)) => {
                    self.runtime
                        .session_routine_invocation_watches
                        .insert(session_id.clone(), stream);
                    vec![ClientEvent::Routine(
                        RoutineEvent::SessionInvocationWatchStartedForOpen {
                            session_id,
                            open_id,
                        },
                    )]
                }
                (Some(session_id), Err(error)) => vec![ClientEvent::Routine(
                    RoutineEvent::SessionInvocationWatchOpenFailed {
                        session_id,
                        open_id,
                        error,
                    },
                )],
                (None, Ok(stream)) => {
                    self.runtime.routine_invocation_watch = Some(stream);
                    vec![ClientEvent::Routine(
                        RoutineEvent::InvocationWatchStartedForOpen { open_id },
                    )]
                }
                (None, Err(error)) => vec![ClientEvent::Routine(
                    RoutineEvent::InvocationWatchOpenFailed { open_id, error },
                )],
            },
            ExecutedEffect::TimelineWatch {
                key,
                open_id,
                result,
            } => match result {
                Ok(stream) => {
                    self.runtime.timeline_watches.insert(key.clone(), stream);
                    vec![ClientEvent::Session(
                        SessionEvent::TimelineWatchStartedForOpen { key, open_id },
                    )]
                }
                Err(error) => vec![ClientEvent::Session(
                    SessionEvent::TimelineWatchOpenFailed {
                        key,
                        open_id,
                        error,
                    },
                )],
            },
        };

        for event in events {
            self.clear_session_runtime_for_event(&event);
            let update = self
                .runtime
                .reducer
                .apply_for_access_generation(access_generation, event);
            self.extend_effects(&update);
            self.suspend_if_access_lost();
        }
    }

    #[allow(clippy::future_not_send, reason = "watch streams are locally polled")]
    async fn apply_next_watch_event(&mut self) {
        let event = self.runtime.next_watch_event().await;
        self.clear_session_runtime_for_event(&event);
        let update = self.runtime.reducer.apply(event);
        self.extend_effects(&update);
        let maintenance = self.runtime.reducer.maintain_watches();
        self.extend_effects(&maintenance);
        self.suspend_if_access_lost();
    }

    fn clear_session_runtime_for_event(&mut self, event: &ClientEvent) {
        let should_clear = match event {
            ClientEvent::Session(SessionEvent::Wiped { operation_key }) => self
                .runtime
                .reducer
                .state()
                .sessions
                .wipe
                .key
                .as_ref()
                .is_some_and(|current| current == operation_key),
            ClientEvent::Session(SessionEvent::WatchChanged(update)) => {
                matches!(update.event, crate::SessionWatchEvent::Cleared)
            }
            _ => false,
        };
        if !should_clear {
            return;
        }

        self.runtime.timeline_watches.clear();
        self.cancel_effects(|effect| {
            matches!(
                effect,
                ClientEffect::Session(
                    SessionEffect::List
                        | SessionEffect::GetTimeline { .. }
                        | SessionEffect::SearchTemplates { .. }
                        | SessionEffect::WatchTimeline { .. }
                )
            )
        });
    }

    fn extend_effects(&mut self, update: &ClientUpdate) {
        for effect in update.effects() {
            if let ClientEffect::Routine(RoutineEffect::WatchInvocations { session_id, .. }) =
                effect
            {
                if let Some(session_id) = session_id {
                    self.runtime
                        .session_routine_invocation_watches
                        .remove(session_id);
                } else {
                    self.runtime.routine_invocation_watch = None;
                }
            }
        }
        let access_generation = self.runtime.reducer.state().access_generation;
        self.effects.extend(
            update
                .effects()
                .iter()
                .cloned()
                .rev()
                .map(|effect| QueuedEffect {
                    access_generation,
                    effect,
                }),
        );
    }

    fn cancel_effects(&mut self, matches: impl Fn(&ClientEffect) -> bool) {
        self.effects.retain(|queued| !matches(&queued.effect));
        if self
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| matches(&in_flight.effect))
        {
            self.in_flight = None;
        }
    }

    fn cancel_auth_poll_effects(&mut self, attempt_id: Option<&str>) {
        self.cancel_effects(|effect| {
            matches!(
                effect,
                ClientEffect::Auth(crate::AuthEffect::Poll { attempt_id: pending, .. })
                    if attempt_id.is_none_or(|attempt_id| pending == attempt_id)
            )
        });
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keep effect and completion identity checks in one exhaustive match"
    )]
    fn effect_completion_is_current(
        &self,
        effect: &ClientEffect,
        executed: &ExecutedEffect,
    ) -> bool {
        match (effect, executed) {
            (
                ClientEffect::Routine(RoutineEffect::WatchDefinitions {
                    open_id: effect_open_id,
                    ..
                }),
                ExecutedEffect::RoutineDefinitionWatch {
                    open_id: executed_open_id,
                    ..
                },
            ) => {
                effect_open_id == executed_open_id
                    && self
                        .runtime
                        .reducer
                        .state()
                        .routines
                        .definition_watch
                        .interests
                        > 0
                    && self
                        .runtime
                        .reducer
                        .state()
                        .routines
                        .definition_watch
                        .open_id
                        == Some(*effect_open_id)
            }
            (
                ClientEffect::Routine(RoutineEffect::WatchInvocations {
                    open_id: effect_open_id,
                    session_id: effect_session_id,
                    ..
                }),
                ExecutedEffect::RoutineInvocationWatch {
                    open_id: executed_open_id,
                    session_id: executed_session_id,
                    ..
                },
            ) => {
                if effect_open_id != executed_open_id || effect_session_id != executed_session_id {
                    return false;
                }
                let state = self.runtime.reducer.state();
                if let Some(session_id) = effect_session_id {
                    state
                        .routines
                        .session_histories
                        .get(session_id)
                        .is_some_and(|history| {
                            history.watch.interests > 0
                                && history.watch.open_id == Some(*effect_open_id)
                        })
                } else {
                    state.routines.invocation_watch.interests > 0
                        && state.routines.invocation_watch.open_id == Some(*effect_open_id)
                }
            }
            (
                ClientEffect::Config(ConfigEffect::Watch {
                    open_id: effect_open_id,
                }),
                ExecutedEffect::ConfigWatch {
                    open_id: executed_open_id,
                    ..
                },
            ) => {
                effect_open_id == executed_open_id
                    && self.runtime.reducer.state().config.watch.interests > 0
                    && self.runtime.reducer.state().config.watch.open_id == Some(*effect_open_id)
            }
            (
                ClientEffect::Session(SessionEffect::Watch {
                    open_id: effect_open_id,
                }),
                ExecutedEffect::SessionWatch {
                    open_id: executed_open_id,
                    ..
                },
            ) => {
                effect_open_id == executed_open_id
                    && self.runtime.reducer.state().sessions.watch.interests > 0
                    && self.runtime.reducer.state().sessions.watch.open_id == Some(*effect_open_id)
            }
            (
                ClientEffect::Session(SessionEffect::WatchTimeline {
                    key: effect_key,
                    open_id: effect_open_id,
                }),
                ExecutedEffect::TimelineWatch {
                    key: executed_key,
                    open_id: executed_open_id,
                    ..
                },
            ) => {
                effect_key == executed_key
                    && effect_open_id == executed_open_id
                    && self
                        .runtime
                        .reducer
                        .state()
                        .session_timeline_watch_is_active(effect_key)
                    && self
                        .runtime
                        .reducer
                        .state()
                        .sessions
                        .timelines
                        .get(&effect_key.session_id)
                        .and_then(|timeline| timeline.watches.get(effect_key))
                        .is_some_and(|watch| watch.open_id == Some(*effect_open_id))
            }
            (
                ClientEffect::Status(StatusEffect::Watch {
                    epoch: effect_epoch,
                }),
                ExecutedEffect::StatusWatch {
                    epoch: executed_epoch,
                    ..
                },
            ) => {
                effect_epoch == executed_epoch
                    && self.runtime.reducer.state().status.watch_epoch == *effect_epoch
                    && self.runtime.reducer.state().status.watch.status == OperationStatus::Loading
            }
            (
                ClientEffect::Auth(crate::AuthEffect::Poll {
                    attempt_id,
                    poll_generation,
                    ..
                }),
                ExecutedEffect::Events(_),
            ) => self
                .runtime
                .reducer
                .state()
                .auth_poll_is_current(attempt_id, *poll_generation),
            (_, ExecutedEffect::Events(_)) => true,
            _ => false,
        }
    }

    fn suspend_if_access_lost(&mut self) {
        self.runtime.suspend_protected_watches();
        if !self.runtime.reducer.state().permits_protected_requests() {
            self.effects.clear();
            self.in_flight = None;
        }
    }
}

const fn command_allowed_without_access(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::StartWaltrLogin
            | ClientCommand::CompleteWaltrLogin { .. }
            | ClientCommand::FailWaltrLogin { .. }
            | ClientCommand::CancelWaltrLogin { .. }
            | ClientCommand::ReprobeWaltrAccess
            | ClientCommand::LogoutWaltr
            | ClientCommand::WaltrAccessExpired { .. }
            | ClientCommand::RegisterConfigWatchInterest
            | ClientCommand::ReleaseConfigWatchInterest
            | ClientCommand::RegisterSessionWatchInterest
            | ClientCommand::ReleaseSessionWatchInterest
            | ClientCommand::RegisterRoutineDefinitionWatchInterest
            | ClientCommand::ReleaseRoutineDefinitionWatchInterest
            | ClientCommand::RegisterRoutineInvocationWatchInterest
            | ClientCommand::ReleaseRoutineInvocationWatchInterest
            | ClientCommand::RegisterSessionRoutineInvocationWatchInterest { .. }
            | ClientCommand::ReleaseSessionRoutineInvocationWatchInterest { .. }
            | ClientCommand::RegisterSessionTimelineWatchInterest { .. }
            | ClientCommand::ReleaseSessionTimelineWatchInterest { .. }
    )
}

const fn timeline_direction(direction: ClientTimelineDirection) -> TimelineDirection {
    match direction {
        ClientTimelineDirection::Forward => TimelineDirection::Forward,
        ClientTimelineDirection::Backward => TimelineDirection::Backward,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientStateStreamPhase {
    WaitForSystemReady,
    Ready,
}

impl ClientSnapshot {
    #[allow(clippy::too_many_lines)]
    fn from_client(client: &Client) -> Self {
        let state = client.state();

        Self {
            access: AccessSnapshot {
                status: state.access.status,
                error: state.access.error.clone(),
                active_attempt_id: state.access.active_attempt_id.clone(),
            },
            auth: AuthSnapshot {
                statuses: state
                    .auth
                    .statuses
                    .iter()
                    .filter_map(|(key, status)| {
                        AuthStatusSnapshot::try_from(status.clone())
                            .ok()
                            .map(|status| (key.clone(), status))
                    })
                    .collect(),
                active_attempt: state
                    .auth
                    .active_attempt
                    .clone()
                    .map(AuthAttemptSnapshot::from),
                list: OperationSnapshot::from_state(&state.auth.list),
                begin: OperationSnapshot::from_state(&state.auth.begin),
                complete: OperationSnapshot::from_state(&state.auth.complete),
                cancel: OperationSnapshot::from_state(&state.auth.cancel),
                refresh: OperationSnapshot::from_state(&state.auth.refresh),
                logout: OperationSnapshot::from_state(&state.auth.logout),
            },
            catalog: CatalogSnapshot {
                search: CatalogModelSearchSnapshot {
                    request_id: state.catalog.search.request_id.clone(),
                    query: state.catalog.search.query.clone(),
                    status: operation_status_name(state.catalog.search.status),
                    error: state.catalog.search.error.clone(),
                    stale: state.catalog.search.stale,
                    models: state
                        .catalog
                        .search
                        .models
                        .iter()
                        .cloned()
                        .map(Into::into)
                        .collect(),
                },
                suggestions: CatalogMatchSuggestionSnapshot {
                    request_id: state.catalog.suggestions.request_id.clone(),
                    status: operation_status_name(state.catalog.suggestions.status),
                    error: state.catalog.suggestions.error.clone(),
                    stale: state.catalog.suggestions.stale,
                    matches: state
                        .catalog
                        .suggestions
                        .matches
                        .iter()
                        .cloned()
                        .map(Into::into)
                        .collect(),
                },
            },
            operations: OperationCollectionSnapshot {
                current: state
                    .operations
                    .iter()
                    .map(|(key, operation)| {
                        (key.clone(), ClientOperationView::from(operation.clone()))
                    })
                    .collect(),
            },
            config: ConfigSnapshot {
                current: state.config.current.clone().map(ConfigView::from),
                resource_baselines: state
                    .config
                    .catalog
                    .as_ref()
                    .map(|catalog| {
                        let absent_etag = catalog
                            .resources
                            .iter()
                            .find(|resource| {
                                ResourceOrigin::try_from(resource.origin)
                                    == Ok(ResourceOrigin::Bundled)
                            })
                            .map(|resource| resource.persisted_etag.clone())
                            .unwrap_or_default();
                        catalog
                            .resources
                            .iter()
                            .map(|resource| {
                                (
                                    resource.path.clone(),
                                    ResourceEditBaselineView {
                                        path: resource.path.clone(),
                                        persisted_etag: resource.persisted_etag.clone(),
                                        absent_etag: absent_etag.clone(),
                                        active_source: resource.active_source.clone(),
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                load: OperationSnapshot::from_state(&state.config.load),
                patch: OperationSnapshot::from_state(&state.config.patch),
                reset: OperationSnapshot::from_state(&state.config.reset),
                watch: OperationSnapshot::from_parts(
                    state.config.watch.status,
                    state.config.watch.error.clone(),
                ),
            },
            status: StatusSnapshot {
                ready: state.status.current.is_some_and(|status| status.ready),
                watch: OperationSnapshot::from_parts(
                    state.status.watch.status,
                    state.status.watch.error.clone(),
                ),
            },
            sessions: SessionSnapshot {
                agents: SessionAgentCatalogSnapshot {
                    entries: state
                        .sessions
                        .agents
                        .entries
                        .iter()
                        .cloned()
                        .map(Into::into)
                        .collect(),
                    default_agent_name: state.sessions.agents.default_agent_name.clone(),
                    load: OperationSnapshot::from_state(&state.sessions.agents.load),
                },
                current: state
                    .sessions
                    .current
                    .values()
                    .cloned()
                    .map(SessionView::from)
                    .collect(),
                create: OperationSnapshot::from_state(&state.sessions.create),
                list: OperationSnapshot::from_state(&state.sessions.list),
                delete: OperationSnapshot::from_state(&state.sessions.delete),
                wipe: OperationSnapshot::from_state(&state.sessions.wipe),
                inference_settings_update: OperationSnapshot::from_state(
                    &state.sessions.inference_settings_update,
                ),
                agent_update: OperationSnapshot::from_state(&state.sessions.agent_update),
                submit_user_message: OperationSnapshot::from_state(
                    &state.sessions.submit_user_message,
                ),
                respond_external_request: OperationSnapshot::from_state(
                    &state.sessions.respond_external_request,
                ),
                interrupt: OperationSnapshot::from_state(&state.sessions.interrupt),
                rollback: OperationSnapshot::from_state(&state.sessions.rollback),
                continue_session: OperationSnapshot::from_state(&state.sessions.continue_session),
                watch: OperationSnapshot::from_parts(
                    state.sessions.watch.status,
                    state.sessions.watch.error.clone(),
                ),
                timelines: state
                    .sessions
                    .timelines
                    .iter()
                    .map(|(session_id, timeline)| {
                        (
                            session_id.clone(),
                            SessionTimelineView {
                                events: {
                                    let mut entries = timeline
                                        .entries
                                        .values()
                                        .cloned()
                                        .filter_map(|entry| entry.try_into().ok())
                                        .collect::<Vec<crate::ProjectedTimelineEntryView>>();
                                    entries.sort_by_key(|entry| entry.ordering_cursor());
                                    entries
                                },
                                projection_revision: timeline.projection_revision,
                                raw_page_boundary: timeline.raw_page_boundary,
                                has_more: timeline.has_more,
                                load: OperationSnapshot::from_state(&timeline.load),
                                watch: OperationSnapshot::from_parts(
                                    timeline.watch.status,
                                    timeline.watch.error.clone(),
                                ),
                            },
                        )
                    })
                    .collect(),
                template_searches: state
                    .sessions
                    .template_searches
                    .iter()
                    .map(|(session_id, search)| {
                        (
                            session_id.clone(),
                            SessionTemplateSearchSnapshot {
                                request_id: search.request_id.clone(),
                                session_id: search.session_id.clone(),
                                query: search.query.clone(),
                                status: operation_status_name(search.status),
                                error: search.error.clone(),
                                options: search
                                    .options
                                    .iter()
                                    .cloned()
                                    .map(SessionTemplateOptionView::from)
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
            },
            routines: RoutineSnapshot {
                definitions: RoutineDefinitionSnapshot {
                    watch: OperationSnapshot::from_parts(
                        state.routines.definition_watch.status,
                        state.routines.definition_watch.error.clone(),
                    ),
                    digest: state.routines.definition_digest.clone(),
                    definitions: state.routines.definitions.clone(),
                },
                history: RoutineHistorySnapshot {
                    watch: OperationSnapshot::from_parts(
                        state.routines.invocation_watch.status,
                        state.routines.invocation_watch.error.clone(),
                    ),
                    recent: {
                        let mut recent = state
                            .routines
                            .invocations
                            .values()
                            .cloned()
                            .collect::<Vec<_>>();
                        recent.sort_by(|left, right| {
                            (right.admission_sequence, right.latest_sequence)
                                .cmp(&(left.admission_sequence, left.latest_sequence))
                        });
                        recent
                    },
                    through_sequence: state.routines.through_sequence,
                    older_page_token: state.routines.older_page_token,
                    pagination: OperationSnapshot::from_parts(
                        state.routines.page.status,
                        state.routines.page.error.clone(),
                    ),
                },
                session_histories: state
                    .routines
                    .session_histories
                    .iter()
                    .map(|(session_id, history)| {
                        let mut recent = history.invocations.values().cloned().collect::<Vec<_>>();
                        recent.sort_by(|left, right| {
                            (right.admission_sequence, right.latest_sequence)
                                .cmp(&(left.admission_sequence, left.latest_sequence))
                        });
                        (
                            session_id.clone(),
                            RoutineHistorySnapshot {
                                watch: OperationSnapshot::from_parts(
                                    history.watch.status,
                                    history.watch.error.clone(),
                                ),
                                recent,
                                through_sequence: history.through_sequence,
                                older_page_token: None,
                                pagination: OperationSnapshot::from_parts(
                                    OperationStatus::Idle,
                                    None,
                                ),
                            },
                        )
                    })
                    .collect(),
                details: state
                    .routines
                    .details
                    .iter()
                    .map(|(id, detail)| {
                        (
                            id.clone(),
                            RoutineDetailSnapshot {
                                status: operation_status_name(detail.status),
                                detail: detail.detail.clone(),
                                error: detail.error.clone(),
                            },
                        )
                    })
                    .collect(),
                starts: state
                    .routines
                    .starts
                    .iter()
                    .map(|(key, start)| {
                        (key.clone(), RoutineStartOperationView::from(start.clone()))
                    })
                    .collect(),
            },
        }
    }
}

impl From<SessionTemplateOption> for SessionTemplateOptionView {
    fn from(option: SessionTemplateOption) -> Self {
        Self {
            kind: match SessionTemplateKind::try_from(option.kind).ok() {
                Some(SessionTemplateKind::File) => "file",
                Some(SessionTemplateKind::Tool) => "tool",
                Some(SessionTemplateKind::Prompt) => "prompt",
                _ => "unknown",
            },
            label: option.label,
            detail: option.detail,
            template: option.template,
        }
    }
}

impl ClientAuthTarget {
    fn into_auth_target(self) -> AuthTarget {
        let target = match self.kind.as_str() {
            "mcpServer" | "mcp_server" => {
                waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(self.key)
            }
            _ => waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(self.key),
        };

        AuthTarget {
            target: Some(target),
        }
    }
}

fn pending_auth_attempt_state(attempt: AuthAttemptSnapshot) -> Option<AuthAttemptState> {
    use waltr::grpc::waltr::auth::v1::{
        AuthAttemptLifecycle, AuthCompletionMode, McpOAuthRegistrationMethod,
    };

    if attempt.lifecycle != "pending" || attempt.attempt_id.is_empty() {
        return None;
    }
    let completion_mode = match attempt.completion_mode.as_str() {
        "serverCallback" => AuthCompletionMode::ServerCallback,
        "manual" => AuthCompletionMode::Manual,
        _ => return None,
    };
    let registration_method = match attempt.registration_method.as_str() {
        "preregistered" => McpOAuthRegistrationMethod::Preregistered,
        "clientMetadata" => McpOAuthRegistrationMethod::ClientMetadata,
        "dynamic" => McpOAuthRegistrationMethod::Dynamic,
        "" | "unknown" => McpOAuthRegistrationMethod::Unspecified,
        _ => return None,
    };
    if !matches!(attempt.target.kind.as_str(), "provider" | "mcpServer")
        || attempt.target.key.is_empty()
    {
        return None;
    }

    Some(AuthAttemptState {
        target: attempt.target.into_auth_target(),
        attempt_id: attempt.attempt_id,
        auth_url: crate::coordination::state::safe_http_url(&attempt.auth_url).unwrap_or_default(),
        verification_url: crate::coordination::state::safe_http_url(&attempt.verification_url)
            .unwrap_or_default(),
        user_code: crate::coordination::state::safe_text(&attempt.user_code, 256),
        expires_at_ms: attempt.expires_at_ms,
        poll_interval_seconds: attempt.poll_interval_seconds,
        messages: attempt
            .messages
            .into_iter()
            .map(|message| crate::coordination::state::safe_text(&message, 1_024))
            .collect(),
        completion_mode: completion_mode as i32,
        registration_method: registration_method as i32,
        lifecycle: AuthAttemptLifecycle::Pending as i32,
        failure_reason: String::new(),
        poll_generation: 0,
        poll_count: 0,
        poll_failures: 0,
    })
}

impl From<AuthTarget> for ClientAuthTarget {
    fn from(target: AuthTarget) -> Self {
        match target.target {
            Some(waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(key)) => Self {
                kind: "provider".to_owned(),
                key,
            },
            Some(waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(key)) => Self {
                kind: "mcpServer".to_owned(),
                key,
            },
            None => Self {
                kind: "unknown".to_owned(),
                key: String::new(),
            },
        }
    }
}

impl TryFrom<AuthStatus> for AuthStatusSnapshot {
    type Error = ();

    fn try_from(status: AuthStatus) -> Result<Self, Self::Error> {
        let target = status.target.ok_or(())?;
        let metadata_target = target.target.as_ref().ok_or(())?;
        let metadata = status
            .metadata
            .into_iter()
            .filter(|(key, value)| auth_status_metadata_is_safe(metadata_target, key, value))
            .collect();
        Ok(Self {
            target: ClientAuthTarget::from(target),
            kind: match waltr::grpc::waltr::auth::v1::AuthStatusKind::try_from(status.kind) {
                Ok(waltr::grpc::waltr::auth::v1::AuthStatusKind::NotConfigured) => "notConfigured",
                Ok(waltr::grpc::waltr::auth::v1::AuthStatusKind::ApiKeyConfigured) => {
                    "apiKeyConfigured"
                }
                Ok(waltr::grpc::waltr::auth::v1::AuthStatusKind::OauthConfigured) => {
                    "oauthConfigured"
                }
                Ok(waltr::grpc::waltr::auth::v1::AuthStatusKind::Expired) => "expired",
                Ok(waltr::grpc::waltr::auth::v1::AuthStatusKind::Unsupported) => "unsupported",
                Ok(waltr::grpc::waltr::auth::v1::AuthStatusKind::AuthorizationRequired) => {
                    "authorizationRequired"
                }
                _ => "unknown",
            }
            .to_owned(),
            supported_methods: status
                .supported_methods
                .into_iter()
                .map(|method| {
                    match waltr::grpc::waltr::auth::v1::AuthMethod::try_from(method) {
                        Ok(waltr::grpc::waltr::auth::v1::AuthMethod::OauthManual) => "oauthManual",
                        Ok(waltr::grpc::waltr::auth::v1::AuthMethod::OauthDevice) => "oauthDevice",
                        _ => "unknown",
                    }
                    .to_owned()
                })
                .collect(),
            expires_at_ms: status.expires_at_ms,
            metadata,
            message: status.message,
            registration_method:
                match waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::try_from(
                    status.registration_method,
                ) {
                    Ok(waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::Preregistered) => {
                        "preregistered"
                    }
                    Ok(waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::ClientMetadata) => {
                        "clientMetadata"
                    }
                    Ok(waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::Dynamic) => {
                        "dynamic"
                    }
                    _ => "unknown",
                }
                .to_owned(),
            can_refresh: status.can_refresh,
        })
    }
}

fn auth_status_metadata_is_safe(
    target: &waltr::grpc::waltr::auth::v1::auth_target::Target,
    key: &str,
    value: &str,
) -> bool {
    if value.is_empty() || value.chars().any(char::is_control) {
        return false;
    }
    match target {
        waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(_) => {
            key == "account_id" && value.len() <= 256
        }
        waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(_) => match key {
            "resource" | "issuer" => value.len() <= 2_048,
            "scope" => value.len() <= 1_024,
            "registration_method" => value.len() <= 64,
            _ => false,
        },
    }
}

impl From<OutboundAuthenticationFailure> for AuthenticationFailureSnapshot {
    fn from(failure: OutboundAuthenticationFailure) -> Self {
        Self {
            version: failure.version,
            domain: authentication_domain_name(failure.domain).to_owned(),
            reason: authentication_failure_reason_name(failure.reason).to_owned(),
            target: ClientAuthTarget::from(failure.target),
            mcp_context: failure
                .mcp_context
                .map(McpAuthorizationContextSnapshot::from),
        }
    }
}

impl From<McpAuthorizationContext> for McpAuthorizationContextSnapshot {
    fn from(context: McpAuthorizationContext) -> Self {
        Self {
            endpoint: context.endpoint,
            http_status: context.http_status,
            resource_metadata_url: context.resource_metadata_url,
            scope: context.scope,
            error: context.error,
            error_description: context.error_description,
            error_uri: context.error_uri,
            realm: context.realm,
        }
    }
}

fn authentication_domain_name(value: i32) -> &'static str {
    use waltr::grpc::waltr::auth::v1::AuthenticationDomain;
    match AuthenticationDomain::try_from(value) {
        Ok(AuthenticationDomain::OutboundProvider) => "outboundProvider",
        Ok(AuthenticationDomain::OutboundMcp) => "outboundMcp",
        Ok(AuthenticationDomain::WaltrAccess) => "waltrAccess",
        _ => "unknown",
    }
}

fn authentication_failure_reason_name(value: i32) -> &'static str {
    use waltr::grpc::waltr::auth::v1::AuthenticationFailureReason;
    match AuthenticationFailureReason::try_from(value) {
        Ok(AuthenticationFailureReason::CredentialRequired) => "credentialRequired",
        Ok(AuthenticationFailureReason::CredentialRejected) => "credentialRejected",
        Ok(AuthenticationFailureReason::NetworkFailed) => "networkFailed",
        Ok(AuthenticationFailureReason::TokenExchangeFailed) => "tokenExchangeFailed",
        Ok(AuthenticationFailureReason::ResponseInvalid) => "responseInvalid",
        Ok(AuthenticationFailureReason::ConfigurationRequired) => "configurationRequired",
        _ => "unknown",
    }
}

impl From<AuthAttemptState> for AuthAttemptSnapshot {
    fn from(attempt: AuthAttemptState) -> Self {
        Self {
            target: ClientAuthTarget::from(attempt.target),
            attempt_id: attempt.attempt_id,
            auth_url: attempt.auth_url,
            verification_url: attempt.verification_url,
            user_code: attempt.user_code,
            expires_at_ms: attempt.expires_at_ms,
            poll_interval_seconds: attempt.poll_interval_seconds,
            messages: attempt.messages,
            completion_mode: match waltr::grpc::waltr::auth::v1::AuthCompletionMode::try_from(
                attempt.completion_mode,
            ) {
                Ok(waltr::grpc::waltr::auth::v1::AuthCompletionMode::ServerCallback) => {
                    "serverCallback"
                }
                Ok(waltr::grpc::waltr::auth::v1::AuthCompletionMode::Manual) => "manual",
                _ => "unknown",
            }
            .to_owned(),
            registration_method:
                match waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::try_from(
                    attempt.registration_method,
                ) {
                    Ok(waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::Preregistered) => {
                        "preregistered"
                    }
                    Ok(waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::ClientMetadata) => {
                        "clientMetadata"
                    }
                    Ok(waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod::Dynamic) => {
                        "dynamic"
                    }
                    _ => "unknown",
                }
                .to_owned(),
            lifecycle: match waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::try_from(
                attempt.lifecycle,
            ) {
                Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Pending) => "pending",
                Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Completing) => "completing",
                Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Succeeded) => "succeeded",
                Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Failed) => "failed",
                Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Canceled) => "canceled",
                Ok(waltr::grpc::waltr::auth::v1::AuthAttemptLifecycle::Expired) => "expired",
                _ => "unknown",
            }
            .to_owned(),
            failure_reason: attempt.failure_reason,
        }
    }
}

impl From<ClientOperationState> for ClientOperationView {
    #[allow(clippy::too_many_lines)]
    fn from(operation: ClientOperationState) -> Self {
        match operation {
            ClientOperationState::AuthBegin {
                status,
                error,
                target,
                attempt,
            } => Self::AuthBegin {
                status: operation_status_name(status),
                error,
                target: ClientAuthTarget::from(target),
                attempt: attempt.map(AuthAttemptSnapshot::from),
            },
            ClientOperationState::AuthComplete { status, error, .. } => Self::AuthComplete {
                status: operation_status_name(status),
                error,
            },
            ClientOperationState::AuthCancel { status, error } => Self::AuthCancel {
                status: operation_status_name(status),
                error,
            },
            ClientOperationState::AuthRefresh {
                status,
                error,
                target,
                ..
            } => Self::AuthRefresh {
                status: operation_status_name(status),
                error,
                target: ClientAuthTarget::from(target),
            },
            ClientOperationState::AuthLogout {
                status,
                error,
                target,
                ..
            } => Self::AuthLogout {
                status: operation_status_name(status),
                error,
                target: ClientAuthTarget::from(target),
            },
            ClientOperationState::ConfigPatch {
                status,
                error,
                config,
            } => Self::ConfigPatch {
                status: operation_status_name(status),
                error,
                config: config.map(ConfigView::from),
            },
            ClientOperationState::ConfigReset {
                status,
                error,
                config,
            } => Self::ConfigReset {
                status: operation_status_name(status),
                error,
                config: config.map(ConfigView::from),
            },
            ClientOperationState::SessionCreate {
                status,
                error,
                session,
            } => Self::SessionCreate {
                status: operation_status_name(status),
                error,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionDelete { status, error, id } => Self::SessionDelete {
                status: operation_status_name(status),
                error,
                id,
            },
            ClientOperationState::SessionWipe { status, error } => Self::SessionWipe {
                status: operation_status_name(status),
                error,
            },
            ClientOperationState::SessionInferenceSettingsUpdate {
                status,
                error,
                session,
            } => Self::SessionInferenceSettingsUpdate {
                status: operation_status_name(status),
                error,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionAgentUpdate {
                status,
                error,
                session,
            } => Self::SessionAgentUpdate {
                status: operation_status_name(status),
                error,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionSubmitUserMessage {
                status,
                error,
                session_id,
                authentication_failure,
            } => Self::SessionSubmitUserMessage {
                status: operation_status_name(status),
                error,
                session_id,
                authentication_failure: authentication_failure.map(Into::into),
            },
            ClientOperationState::SessionRespondExternalRequest {
                status,
                error,
                session_id,
                request_id,
                authentication_failure,
            } => Self::SessionRespondExternalRequest {
                status: operation_status_name(status),
                error,
                session_id,
                request_id,
                authentication_failure: authentication_failure.map(Into::into),
            },
            ClientOperationState::SessionInterrupt {
                status,
                error,
                session_id,
            } => Self::SessionInterrupt {
                status: operation_status_name(status),
                error,
                session_id,
            },
            ClientOperationState::SessionRollback {
                status,
                error,
                session_id,
                session,
            } => Self::SessionRollback {
                status: operation_status_name(status),
                error,
                session_id,
                session: session.map(SessionView::from),
            },
            ClientOperationState::SessionContinue {
                status,
                error,
                session_id,
                target,
                authentication_failure,
            } => Self::SessionContinue {
                status: operation_status_name(status),
                error,
                session_id,
                target: RollbackTargetView::from(&target),
                authentication_failure: authentication_failure.map(Into::into),
            },
        }
    }
}

impl From<crate::RoutineStartOperationState> for RoutineStartOperationView {
    fn from(start: crate::RoutineStartOperationState) -> Self {
        Self {
            operation_key: start.operation_key,
            request_id: start.request_id,
            routine_name: start.routine_name,
            trigger_name: start.trigger_name,
            source_session_id: start.source_session_id,
            inputs: start.inputs,
            status: operation_status_name(start.status),
            invocation_id: start.invocation_id,
            validation_failure: start.validation_failure,
            error: start.error,
        }
    }
}

impl OperationSnapshot {
    fn from_state(state: &crate::OperationState) -> Self {
        Self {
            status: operation_status_name(state.status),
            error: state.error.clone(),
            key: state.key.clone(),
        }
    }

    const fn from_parts(status: OperationStatus, error: Option<String>) -> Self {
        Self {
            status: operation_status_name(status),
            error,
            key: None,
        }
    }
}

const fn operation_status_name(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Idle => "idle",
        OperationStatus::Loading => "loading",
        OperationStatus::Ready => "ready",
        OperationStatus::Opened => "opened",
        OperationStatus::Received => "received",
        OperationStatus::Waiting => "waiting",
        OperationStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use waltr::grpc::waltr::status::v1::SystemStatus;

    use crate::TimelineEvent;
    use crate::transport::{
        ConfigWatchUpdate, SessionTimelineWatchEvent, SessionTimelineWatchUpdate,
    };

    fn pending_attempt(completion_mode: &str) -> AuthAttemptSnapshot {
        AuthAttemptSnapshot {
            target: ClientAuthTarget {
                kind: "provider".to_owned(),
                key: "test".to_owned(),
            },
            attempt_id: "attempt-restored".to_owned(),
            auth_url: "https://provider.example/authorize?state=opaque".to_owned(),
            verification_url: String::new(),
            user_code: String::new(),
            expires_at_ms: i64::MAX,
            poll_interval_seconds: 2,
            messages: vec!["Continue in the browser.".to_owned()],
            completion_mode: completion_mode.to_owned(),
            registration_method: String::new(),
            lifecycle: "pending".to_owned(),
            failure_reason: String::new(),
        }
    }

    #[test]
    fn restored_pending_attempt_seeds_snapshot_and_only_callback_mode_resumes_polling() {
        let manual = ClientRuntime::with_transport_and_pending_auth_attempt(
            crate::NativeGrpcTransport::new("http://127.0.0.1:0"),
            pending_attempt("manual"),
        )
        .into_state_stream(|_| std::future::ready(()));
        assert_eq!(
            manual
                .snapshot()
                .auth
                .active_attempt
                .as_ref()
                .map(|attempt| attempt.attempt_id.as_str()),
            Some("attempt-restored")
        );
        assert!(manual.effects.is_empty());

        let callback = ClientRuntime::with_transport_and_pending_auth_attempt(
            crate::NativeGrpcTransport::new("http://127.0.0.1:0"),
            pending_attempt("serverCallback"),
        )
        .into_state_stream(|_| std::future::ready(()));
        assert!(matches!(
            callback.effects.as_slice(),
            [QueuedEffect {
                effect: ClientEffect::Auth(crate::AuthEffect::Poll { attempt_id, .. }),
                ..
            }] if attempt_id == "attempt-restored"
        ));
    }

    #[test]
    fn restored_attempt_rejects_terminal_and_malformed_state() {
        for mut attempt in [pending_attempt("manual"), pending_attempt("unknown")] {
            if attempt.completion_mode == "manual" {
                attempt.lifecycle = "succeeded".to_owned();
            }
            let runtime = ClientRuntime::with_transport_and_pending_auth_attempt(
                crate::NativeGrpcTransport::new("http://127.0.0.1:0"),
                attempt,
            );
            assert!(runtime.snapshot().auth.active_attempt.is_none());
        }
    }

    #[test]
    fn snapshot_exposes_latest_catalog_operations() {
        let mut client = Client::new();
        client.search_catalog_models("search-1", "sonnet", None, 10);
        client.apply(ClientEvent::Catalog(crate::CatalogEvent::ModelsSearched {
            request_id: "search-1".to_owned(),
            models: vec![crate::CatalogModelSummary {
                model: Some(crate::CatalogModelRef {
                    provider_id: "anthropic".to_owned(),
                    model_id: "claude-sonnet".to_owned(),
                }),
                model_name: "Claude Sonnet".to_owned(),
                ..crate::CatalogModelSummary::default()
            }],
            stale: true,
        }));
        client.suggest_catalog_matches("suggest-1", "claude", "sonnet", 5);

        let snapshot = ClientSnapshot::from_client(&client);

        assert_eq!(snapshot.catalog.search.request_id, "search-1");
        assert_eq!(snapshot.catalog.search.status, "ready");
        assert!(snapshot.catalog.search.stale);
        assert_eq!(
            snapshot.catalog.search.models[0]
                .model
                .as_ref()
                .map(|model| model.model_id.as_str()),
            Some("claude-sonnet")
        );
        assert_eq!(snapshot.catalog.suggestions.request_id, "suggest-1");
        assert_eq!(snapshot.catalog.suggestions.status, "loading");
    }

    #[test]
    fn snapshot_exposes_secret_safe_auth_statuses_and_refresh_capability() {
        let mut client = Client::new();
        client.apply(ClientEvent::Auth(crate::AuthEvent::Listed(vec![
            AuthStatus {
                target: Some(AuthTarget {
                    target: Some(
                        waltr::grpc::waltr::auth::v1::auth_target::Target::ProviderKey(
                            "codex".to_owned(),
                        ),
                    ),
                }),
                kind: waltr::grpc::waltr::auth::v1::AuthStatusKind::OauthConfigured as i32,
                supported_methods: vec![waltr::grpc::waltr::auth::v1::AuthMethod::OauthManual as i32],
                expires_at_ms: 42,
                metadata: std::collections::HashMap::from([
                    ("account_id".to_owned(), "account-1".to_owned()),
                    (
                        "token_url".to_owned(),
                        "https://secret.example/token".to_owned(),
                    ),
                    ("client_id".to_owned(), "secret-client".to_owned()),
                    (
                        "redirect_uri".to_owned(),
                        "https://secret.example/callback".to_owned(),
                    ),
                    ("access_token".to_owned(), "access-secret".to_owned()),
                    ("resource".to_owned(), "https://mcp.example".to_owned()),
                    ("unknown".to_owned(), "unknown-secret".to_owned()),
                ]),
                can_refresh: true,
                ..AuthStatus::default()
            },
        ])));

        let snapshot = ClientSnapshot::from_client(&client);
        let status = &snapshot.auth.statuses["provider:codex"];
        assert_eq!(status.kind, "oauthConfigured");
        assert_eq!(status.supported_methods, ["oauthManual"]);
        assert_eq!(status.metadata["account_id"], "account-1");
        assert_eq!(status.metadata.len(), 1);
        assert!(status.can_refresh);
        let rendered = serde_json::to_string(&snapshot).unwrap();
        assert!(!rendered.contains("access_token"));
        assert!(!rendered.contains("refresh_token"));
        assert!(!rendered.contains("token_url"));
        assert!(!rendered.contains("client_id"));
        assert!(!rendered.contains("redirect_uri"));
        assert!(!rendered.contains("unknown-secret"));
    }

    #[test]
    fn snapshot_retains_only_mcp_safe_metadata() {
        let mut client = Client::new();
        client.apply(ClientEvent::Auth(crate::AuthEvent::Listed(vec![
            AuthStatus {
                target: Some(AuthTarget {
                    target: Some(
                        waltr::grpc::waltr::auth::v1::auth_target::Target::McpServerKey(
                            "remote".to_owned(),
                        ),
                    ),
                }),
                metadata: std::collections::HashMap::from([
                    (
                        "resource".to_owned(),
                        "https://mcp.example/tools".to_owned(),
                    ),
                    ("issuer".to_owned(), "https://auth.example".to_owned()),
                    ("scope".to_owned(), "tools.read tools.write".to_owned()),
                    ("registration_method".to_owned(), "dynamic".to_owned()),
                    ("account_id".to_owned(), "provider-only".to_owned()),
                    ("refresh_token".to_owned(), "refresh-secret".to_owned()),
                    ("unknown".to_owned(), "unknown-secret".to_owned()),
                ]),
                ..AuthStatus::default()
            },
        ])));

        let snapshot = ClientSnapshot::from_client(&client);
        let metadata = &snapshot.auth.statuses["mcp:remote"].metadata;
        assert_eq!(metadata.len(), 4);
        assert_eq!(metadata["resource"], "https://mcp.example/tools");
        assert_eq!(metadata["issuer"], "https://auth.example");
        assert_eq!(metadata["scope"], "tools.read tools.write");
        assert_eq!(metadata["registration_method"], "dynamic");
        let rendered = serde_json::to_string(&snapshot).unwrap();
        assert!(!rendered.contains("provider-only"));
        assert!(!rendered.contains("refresh-secret"));
        assert!(!rendered.contains("unknown-secret"));
    }

    #[test]
    fn auth_status_metadata_allowlists_are_bounded_and_target_specific() {
        use waltr::grpc::waltr::auth::v1::auth_target::Target;

        let provider = Target::ProviderKey("provider".to_owned());
        let mcp = Target::McpServerKey("remote".to_owned());
        assert!(auth_status_metadata_is_safe(
            &provider,
            "account_id",
            "account-1"
        ));
        assert!(!auth_status_metadata_is_safe(
            &provider,
            "resource",
            "https://mcp.example"
        ));
        assert!(auth_status_metadata_is_safe(
            &mcp,
            "resource",
            "https://mcp.example"
        ));
        assert!(!auth_status_metadata_is_safe(
            &mcp,
            "account_id",
            "account-1"
        ));
        assert!(!auth_status_metadata_is_safe(&mcp, "scope", ""));
        assert!(!auth_status_metadata_is_safe(
            &mcp,
            "scope",
            "tools.read\nsecret"
        ));
        assert!(!auth_status_metadata_is_safe(
            &provider,
            "account_id",
            &"x".repeat(257)
        ));
        assert!(!auth_status_metadata_is_safe(
            &mcp,
            "resource",
            &"x".repeat(2_049)
        ));
    }

    #[test]
    fn replacing_callback_attempt_cancels_queued_poll_timer() {
        let mut stream = ready_state_stream();
        let target = ClientAuthTarget {
            kind: "mcpServer".to_owned(),
            key: "github".to_owned(),
        };
        stream
            .begin_command(ClientCommand::BeginAuth {
                target: target.clone(),
            })
            .expect("begin should queue");
        let queued = stream.effects.pop().expect("begin effect");
        let ClientEffect::Auth(crate::AuthEffect::Begin {
            operation_key,
            target: auth_target,
        }) = queued.effect.clone()
        else {
            panic!("begin effect");
        };
        stream.complete_effect(
            queued.access_generation,
            &queued.effect,
            ExecutedEffect::Events(vec![ClientEvent::Auth(crate::AuthEvent::Begun {
                operation_key,
                target: auth_target,
                attempt: crate::GenericAuthAttempt {
                    attempt_id: "attempt-1".to_owned(),
                    completion_mode: waltr::grpc::waltr::auth::v1::AuthCompletionMode::ServerCallback
                        as i32,
                    poll_interval_seconds: 1,
                    ..crate::GenericAuthAttempt::default()
                },
            })]),
        );
        assert!(matches!(
            stream.effects.as_slice(),
            [QueuedEffect {
                effect: ClientEffect::Auth(crate::AuthEffect::Poll { .. }),
                ..
            }]
        ));

        stream
            .begin_command(ClientCommand::BeginAuth { target })
            .expect("replacement begin should queue");
        assert!(stream.effects.iter().all(|queued| !matches!(
            queued.effect,
            ClientEffect::Auth(crate::AuthEffect::Poll { .. })
        )));
    }

    #[tokio::test]
    async fn state_stream_yields_status_loading_before_transport_work() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);

        let snapshot = stream.next().await;

        assert_eq!(snapshot.status.watch.status, "loading");
        assert!(!snapshot.status.ready);
    }

    #[tokio::test]
    async fn state_stream_yields_failed_status_before_reconnect_sleep() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);

        drop(stream.next().await);
        let snapshot = stream.next().await;

        assert_eq!(snapshot.status.watch.status, "failed");
        assert!(snapshot.status.watch.error.is_some());
    }

    #[tokio::test]
    async fn active_config_watch_does_not_starve_status_watch_failures() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchStarted));
        runtime.status_watch = Some((
            0,
            Box::pin(stream::once(async {
                Err(ClientError::new("status ended"))
            })),
        ));
        runtime.config_watch = Some(Box::pin(stream::pending::<Result<ConfigWatchUpdate, _>>()));

        let event = runtime.next_watch_event().await;
        let update = runtime.reducer.apply(event);
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.status.watch.status, "failed");
        assert_eq!(snapshot.config.watch.status, "waiting");
        assert!(!snapshot.status.ready);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::SleepReconnect {
                delay_ms: 500,
                ..
            })]
        ));
    }

    #[tokio::test]
    async fn busy_timeline_watch_does_not_starve_status_watch_failures() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        let key = TimelineWatchKey::new("session".to_owned(), Some(0), Vec::new(), None);
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime.reducer.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestRegistered {
                session_id: key.session_id.clone(),
                after_cursor: key.after_cursor,
                channels: key.channels.clone(),
                initial_limit: key.initial_limit,
                projection_mode: key.projection_mode,
            },
        ));
        runtime
            .reducer
            .apply(ClientEvent::Session(SessionEvent::TimelineWatchStarted {
                key: key.clone(),
            }));
        runtime.status_watch = Some((
            0,
            Box::pin(stream::once(async {
                Err(ClientError::new("status transport lost"))
            })),
        ));
        runtime.timeline_watches.insert(
            key,
            Box::pin(stream::iter((1..=16).map(|cursor| {
                Ok(SessionTimelineWatchUpdate {
                    sequence: cursor,
                    event: SessionTimelineWatchEvent::Event(timeline_event("session", cursor)),
                })
            }))),
        );

        let event = runtime.next_watch_event().await;
        let update = runtime.reducer.apply(event);
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.status.watch.status, "failed");
        assert!(!snapshot.status.ready);
        assert!(matches!(
            update.effects(),
            [ClientEffect::Status(StatusEffect::SleepReconnect { .. })]
        ));
    }

    #[tokio::test]
    async fn multiple_ready_timeline_watches_are_polled_fairly() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        let first =
            TimelineWatchKey::new("session".to_owned(), Some(0), vec!["a".to_owned()], None);
        let second =
            TimelineWatchKey::new("session".to_owned(), Some(0), vec!["b".to_owned()], None);
        register_timeline_watch(&mut runtime, &first);
        register_timeline_watch(&mut runtime, &second);
        runtime.timeline_watches.insert(
            first,
            Box::pin(stream::iter((1..=16).map(|cursor| {
                Ok(SessionTimelineWatchUpdate {
                    sequence: cursor,
                    event: SessionTimelineWatchEvent::Event(timeline_event("session", cursor)),
                })
            }))),
        );
        runtime.timeline_watches.insert(
            second,
            Box::pin(stream::once(async {
                Ok(SessionTimelineWatchUpdate {
                    sequence: 100,
                    event: SessionTimelineWatchEvent::Event(timeline_event("session", 100)),
                })
            })),
        );

        let first_event = runtime.next_watch_event().await;
        runtime.reducer.apply(first_event);
        let second_event = runtime.next_watch_event().await;
        runtime.reducer.apply(second_event);
        let timeline = runtime
            .reducer
            .state()
            .sessions
            .timelines
            .get("session")
            .expect("timeline should exist");

        assert!(timeline.entries.contains_key("durable:1:i1"));
        assert!(timeline.entries.contains_key("durable:100:i100"));
    }

    #[tokio::test]
    async fn config_transport_loss_reconnects_config_without_failing_status() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchStarted));
        runtime.status_watch = Some((0, Box::pin(stream::pending::<Result<SystemStatus, _>>())));
        runtime.config_watch = Some(Box::pin(stream::once(async {
            Err(ClientError::new("config transport lost"))
        })));

        let event = runtime.next_watch_event().await;
        let update = runtime.reducer.apply(event);
        let snapshot = runtime.snapshot();

        assert_eq!(snapshot.status.watch.status, "received");
        assert!(snapshot.status.ready);
        assert_eq!(snapshot.config.watch.status, "failed");
        assert!(matches!(
            update.effects(),
            [ClientEffect::Config(ConfigEffect::SleepReconnect {
                delay_ms: 500,
            })]
        ));
    }

    #[test]
    fn logout_drops_runtime_watch_handles_and_queued_protected_effects() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::WatchInterestRegistered));
        runtime.status_watch = Some((0, Box::pin(stream::pending())));
        runtime.config_watch = Some(Box::pin(stream::pending()));
        let mut stream = ClientStateStream {
            runtime,
            phase: ClientStateStreamPhase::Ready,
            effects: vec![QueuedEffect {
                access_generation: 0,
                effect: ClientEffect::Config(ConfigEffect::Watch { open_id: 1 }),
            }],
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };

        stream
            .begin_command(ClientCommand::LogoutWaltr)
            .expect("logout fact should be accepted");

        assert!(stream.runtime.status_watch.is_none());
        assert!(stream.runtime.config_watch.is_none());
        assert!(matches!(
            stream.effects.as_slice(),
            [QueuedEffect {
                effect: ClientEffect::Status(StatusEffect::Watch { .. }),
                ..
            }]
        ));
        assert_eq!(
            stream.snapshot().access.status,
            AccessStatus::UnknownChecking
        );
        assert_eq!(stream.snapshot().config.watch.status, "idle");
    }

    #[test]
    fn proactive_access_expiry_command_suspends_runtime_without_logging_out() {
        let mut stream = ClientStateStream {
            runtime: ClientRuntime::for_endpoint("http://127.0.0.1:0"),
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };
        stream.runtime.status_watch = Some((0, Box::pin(stream::pending())));

        stream
            .begin_command(ClientCommand::WaltrAccessExpired {
                error: "refresh failed".to_owned(),
            })
            .expect("platform expiry should be accepted");

        assert!(stream.runtime.status_watch.is_none());
        assert_eq!(stream.snapshot().access.status, AccessStatus::LoginRequired);
        assert_eq!(
            stream.snapshot().access.error.as_deref(),
            Some("refresh failed")
        );
    }

    #[test]
    fn routine_watch_ownership_commands_are_accepted_and_released_without_access() {
        let mut stream = ready_state_stream();
        for command in [
            ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ClientCommand::RegisterRoutineInvocationWatchInterest,
            ClientCommand::RegisterSessionRoutineInvocationWatchInterest {
                session_id: "session".to_owned(),
            },
        ] {
            stream
                .begin_command(command)
                .expect("initial Routine interest should register");
        }
        stream
            .begin_command(ClientCommand::WaltrAccessExpired {
                error: "expired".to_owned(),
            })
            .expect("access expiry should be accepted");

        for command in [
            ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ClientCommand::RegisterRoutineInvocationWatchInterest,
            ClientCommand::RegisterSessionRoutineInvocationWatchInterest {
                session_id: "session".to_owned(),
            },
        ] {
            stream
                .begin_command(command)
                .expect("Routine interest should register without access");
        }
        for command in [
            ClientCommand::ReleaseRoutineDefinitionWatchInterest,
            ClientCommand::ReleaseRoutineInvocationWatchInterest,
            ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
                session_id: "session".to_owned(),
            },
        ] {
            stream
                .begin_command(command.clone())
                .expect("Routine interest should decrement without access");
            stream
                .begin_command(command)
                .expect("final Routine interest should release without access");
        }

        let state = stream.runtime.reducer.state();
        assert_eq!(state.access.status, AccessStatus::LoginRequired);
        assert_eq!(state.routines.definition_watch.interests, 0);
        assert_eq!(state.routines.invocation_watch.interests, 0);
        assert!(state.routines.session_histories.is_empty());
        assert!(
            stream
                .begin_command(ClientCommand::StartManualRoutine {
                    operation_key: "denied".to_owned(),
                    request_id: "denied".to_owned(),
                    routine_name: "fork".to_owned(),
                    trigger_name: "manual".to_owned(),
                    source_session_id: "session".to_owned(),
                    inputs: BTreeMap::new(),
                })
                .is_err(),
            "Routine data mutations must remain denied without access"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn transient_reprobe_command_keeps_all_watch_interests() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        drop(runtime.reducer.register_config_watch_interest());
        drop(runtime.reducer.register_session_watch_interest());
        drop(runtime.reducer.register_session_timeline_watch_interest(
            "session",
            Some(0),
            Vec::new(),
            None,
        ));
        runtime
            .reducer
            .apply(ClientEvent::Config(ConfigEvent::Loaded(
                crate::ResourceCatalog {
                    etag: "etag-1".to_owned(),
                    effective_configuration: Some(crate::Configuration {
                        etag: "etag-1".to_owned(),
                        ..crate::Configuration::default()
                    }),
                    ..crate::ResourceCatalog::default()
                },
            )));
        runtime
            .reducer
            .apply(ClientEvent::Session(SessionEvent::Listed(vec![
                crate::SessionSummary {
                    id: "session".to_owned(),
                    ..crate::SessionSummary::default()
                },
            ])));
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchFailed(
                ClientError::waltr_login_required("current credential rejected"),
            )));
        let mut stream = ClientStateStream {
            runtime,
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };

        stream
            .begin_command(ClientCommand::ReprobeWaltrAccess)
            .expect("transient reprobe should be accepted without access");

        let state = stream.runtime.reducer.state();
        assert_eq!(state.access.status, AccessStatus::UnknownChecking);
        assert_eq!(state.config.watch.interests, 1);
        assert_eq!(state.sessions.watch.interests, 1);
        assert_eq!(state.timeline_watch_keys().len(), 1);
        assert!(state.access.reload_config);
        assert!(state.access.reload_sessions);
        assert!(matches!(
            stream.effects.as_slice(),
            [QueuedEffect {
                effect: ClientEffect::Status(StatusEffect::Watch { .. }),
                ..
            }]
        ));

        stream.effects.clear();
        let epoch = stream.runtime.reducer.state().status.watch_epoch;
        let update =
            stream
                .runtime
                .reducer
                .apply(ClientEvent::Status(StatusEvent::WatchChangedForEpoch {
                    epoch,
                    status: SystemStatus { ready: true },
                }));
        assert!(matches!(
            update.effects(),
            [
                ClientEffect::Config(ConfigEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::Watch { .. }),
                ClientEffect::Session(SessionEffect::ListAgents { .. }),
                ClientEffect::Config(ConfigEffect::Load),
                ClientEffect::Session(SessionEffect::List),
            ]
        ));
        stream.extend_effects(&update);
        assert_eq!(stream.effects.len(), 5);
        assert!(matches!(
            stream.runtime.reducer.maintain_watches().effects(),
            [ClientEffect::Session(SessionEffect::WatchTimeline { .. })]
        ));
        let state = stream.runtime.reducer.state();
        assert_eq!(state.access.status, AccessStatus::Authenticated);
        assert_eq!(state.config.watch.interests, 1);
        assert_eq!(state.sessions.watch.interests, 1);
        assert_eq!(state.timeline_watch_keys().len(), 1);
        assert!(!state.access.reload_config);
        assert!(!state.access.reload_sessions);
    }

    #[test]
    fn proactive_access_expiry_command_has_stable_json_contract() {
        let json = serde_json::to_value(ClientCommand::WaltrAccessExpired {
            error: "token expired".to_owned(),
        })
        .expect("command should serialize");

        assert_eq!(
            json,
            serde_json::json!({
                "type": "waltrAccessExpired",
                "error": "token expired",
            })
        );
        assert!(ClientCommand::decl().contains("waltrAccessExpired"));
    }

    #[test]
    fn transient_reprobe_command_has_stable_json_contract() {
        let json = serde_json::to_value(ClientCommand::ReprobeWaltrAccess)
            .expect("command should serialize");

        assert_eq!(json, serde_json::json!({ "type": "reprobeWaltrAccess" }));
        assert!(ClientCommand::decl().contains("reprobeWaltrAccess"));
    }

    #[test]
    fn wipe_sessions_command_has_stable_json_contract() {
        let json =
            serde_json::to_value(ClientCommand::WipeSessions).expect("command should serialize");

        assert_eq!(json, serde_json::json!({ "type": "wipeSessions" }));
        assert!(ClientCommand::decl().contains("wipeSessions"));
    }

    #[test]
    fn inference_commands_have_stable_json_contracts() {
        let defaults = serde_json::to_value(ClientCommand::SetInferenceDefaults {
            defaults: Some(InferenceDefaultsView {
                provider_key: "openai".to_owned(),
                model_id: "gpt-5".to_owned(),
                reasoning_effort: Some("high".to_owned()),
            }),
        })
        .expect("defaults command should serialize");
        assert_eq!(
            defaults,
            serde_json::json!({
                "type": "setInferenceDefaults",
                "defaults": {
                    "providerKey": "openai",
                    "modelId": "gpt-5",
                    "reasoningEffort": "high",
                },
            })
        );

        let create = serde_json::to_value(ClientCommand::CreateSession {
            agent_name: "coding".to_owned(),
        })
        .unwrap();
        assert_eq!(
            create,
            serde_json::json!({"type": "createSession", "agentName": "coding"})
        );

        let switch = serde_json::to_value(ClientCommand::UpdateSessionAgent {
            id: "session-1".to_owned(),
            agent_name: "research".to_owned(),
        })
        .unwrap();
        assert_eq!(
            switch,
            serde_json::json!({
                "type": "updateSessionAgent",
                "id": "session-1",
                "agentName": "research"
            })
        );

        let session = serde_json::to_value(ClientCommand::UpdateSessionInferenceSettings {
            id: "session-1".to_owned(),
            provider_key: "openai".to_owned(),
            model_id: "gpt-5".to_owned(),
            reasoning_effort: None,
        })
        .expect("session command should serialize");
        assert_eq!(
            session,
            serde_json::json!({
                "type": "updateSessionInferenceSettings",
                "id": "session-1",
                "providerKey": "openai",
                "modelId": "gpt-5",
                "reasoningEffort": null,
            })
        );
    }

    #[test]
    fn session_clear_drops_timeline_runtime_state_but_preserves_list_watch_work() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);
        let key = TimelineWatchKey::new("session".to_owned(), None, Vec::new(), None);
        stream
            .runtime
            .timeline_watches
            .insert(key.clone(), Box::pin(stream::pending()));
        stream.effects = vec![
            QueuedEffect {
                access_generation: 0,
                effect: ClientEffect::Session(SessionEffect::Watch { open_id: 1 }),
            },
            QueuedEffect {
                access_generation: 0,
                effect: ClientEffect::Session(SessionEffect::List),
            },
            QueuedEffect {
                access_generation: 0,
                effect: ClientEffect::Session(SessionEffect::WatchTimeline { key, open_id: 2 }),
            },
            QueuedEffect {
                access_generation: 0,
                effect: ClientEffect::Session(SessionEffect::GetTimeline {
                    session_id: "session".to_owned(),
                    request_id: 0,
                    baseline_mode: crate::SessionTimelineProjectionMode::Raw,
                    baseline_revision: None,
                    query: None,
                    projection_mode: crate::SessionTimelineProjectionMode::Raw,
                }),
            },
            QueuedEffect {
                access_generation: 0,
                effect: ClientEffect::Session(SessionEffect::SearchTemplates {
                    request_id: "search".to_owned(),
                    session_id: "session".to_owned(),
                    query: String::new(),
                    limit: 1,
                    kinds: Vec::new(),
                    target: crate::SessionTemplateSearchTarget::Resources,
                    cursor: None,
                }),
            },
        ];
        let event = ClientEvent::Session(SessionEvent::WatchChanged(crate::SessionWatchUpdate {
            sequence: 1,
            event: crate::SessionWatchEvent::Cleared,
        }));

        stream.clear_session_runtime_for_event(&event);

        assert!(stream.runtime.timeline_watches.is_empty());
        assert!(matches!(
            stream.effects.as_slice(),
            [QueuedEffect {
                effect: ClientEffect::Session(SessionEffect::Watch { open_id: 1 }),
                ..
            }]
        ));
    }

    #[test]
    fn stale_wipe_completion_does_not_cancel_current_session_runtime() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);
        let wipe = stream.runtime.reducer.wipe_sessions();
        let [ClientEffect::Session(SessionEffect::Wipe { operation_key })] = wipe.effects() else {
            panic!("session wipe effect");
        };
        let operation_key = operation_key.clone();
        let key = TimelineWatchKey::new("session".to_owned(), None, Vec::new(), None);
        stream
            .runtime
            .timeline_watches
            .insert(key, Box::pin(stream::pending()));
        stream.in_flight = Some(InFlightEffect {
            access_generation: 0,
            effect: ClientEffect::Session(SessionEffect::List),
            future: async {
                ExecutedEffect::Events(vec![ClientEvent::Session(SessionEvent::Listed(vec![
                    crate::SessionSummary {
                        id: "stale".to_owned(),
                        ..crate::SessionSummary::default()
                    },
                ]))])
            }
            .boxed_local(),
        });

        stream.clear_session_runtime_for_event(&ClientEvent::Session(SessionEvent::Wiped {
            operation_key: "stale".to_owned(),
        }));
        assert_eq!(stream.runtime.timeline_watches.len(), 1);
        assert!(stream.in_flight.is_some());

        stream.clear_session_runtime_for_event(&ClientEvent::Session(SessionEvent::Wiped {
            operation_key,
        }));
        assert!(stream.runtime.timeline_watches.is_empty());
        assert!(stream.in_flight.is_none());
        assert!(stream.runtime.reducer.state().sessions.current.is_empty());
    }

    #[test]
    fn remote_clear_cancels_in_flight_list_before_stale_completion() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);
        let update = stream.runtime.reducer.request_sessions();
        assert_eq!(
            update.effects(),
            &[ClientEffect::Session(SessionEffect::List)]
        );
        let (sender, receiver) = oneshot::channel();
        stream.in_flight = Some(InFlightEffect {
            access_generation: 0,
            effect: ClientEffect::Session(SessionEffect::List),
            future: async move {
                ExecutedEffect::Events(vec![ClientEvent::Session(SessionEvent::Listed(
                    receiver.await.expect("list completion sender"),
                ))])
            }
            .boxed_local(),
        });
        let clear = ClientEvent::Session(SessionEvent::WatchChanged(crate::SessionWatchUpdate {
            sequence: 1,
            event: crate::SessionWatchEvent::Cleared,
        }));

        stream.clear_session_runtime_for_event(&clear);
        drop(stream.runtime.reducer.apply(clear));
        let stale = vec![crate::SessionSummary {
            id: "stale".to_owned(),
            ..crate::SessionSummary::default()
        }];

        assert!(stream.in_flight.is_none());
        assert!(sender.send(stale).is_err());
        assert!(stream.runtime.reducer.state().sessions.current.is_empty());
        assert_eq!(
            stream.runtime.reducer.state().sessions.list.status,
            OperationStatus::Ready
        );
        assert_eq!(
            stream.runtime.reducer.request_sessions().effects(),
            &[ClientEffect::Session(SessionEffect::List)]
        );
    }

    #[tokio::test]
    async fn ready_state_without_config_interest_yields_without_waiting_for_status_watch() {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        runtime.status_watch = Some((0, Box::pin(stream::pending::<Result<SystemStatus, _>>())));
        let mut stream = ClientStateStream {
            runtime,
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };

        let snapshot = tokio::time::timeout(std::time::Duration::from_millis(10), stream.next())
            .await
            .expect("ready stream should not wait indefinitely for idle status watch");

        assert_eq!(snapshot.status.watch.status, "received");
        assert_eq!(snapshot.config.watch.status, "idle");
    }

    #[tokio::test]
    async fn command_preemption_preserves_in_flight_effect() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let started_tx = Rc::new(std::cell::RefCell::new(Some(started_tx)));
        let release_rx = Rc::new(std::cell::RefCell::new(Some(release_rx)));
        let mut stream = ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream({
            let started_tx = Rc::clone(&started_tx);
            let release_rx = Rc::clone(&release_rx);
            move |_| {
                let started_tx = started_tx.borrow_mut().take();
                let release_rx = release_rx.borrow_mut().take();
                async move {
                    started_tx.expect("sleep should start once").send(()).ok();
                    release_rx
                        .expect("sleep should wait once")
                        .await
                        .expect("sleep release should arrive");
                }
            }
        });
        stream.phase = ClientStateStreamPhase::Ready;
        stream.effects.push(QueuedEffect {
            access_generation: stream.runtime.reducer.state().access_generation,
            effect: ClientEffect::Config(ConfigEffect::SleepReconnect { delay_ms: 1 }),
        });

        let event = stream
            .next_or_dispatch(async move {
                started_rx.await.ok()?;
                Some(ClientCommandRequest::new(
                    ClientCommand::RegisterConfigWatchInterest,
                ))
            })
            .await;
        assert!(matches!(event, ClientStateStreamEvent::Snapshot(_)));
        assert!(stream.in_flight.is_some());

        release_tx.send(()).expect("effect should still be alive");
        tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
            .await
            .expect("preserved effect should complete");
        assert!(stream.in_flight.is_none());
    }

    #[test]
    fn newer_catalog_search_cancels_the_superseded_effect() {
        let mut stream = ready_state_stream();
        stream
            .begin_command(ClientCommand::SearchCatalogModels {
                request_id: "catalog-old".to_owned(),
                query: "old".to_owned(),
                provider_id: None,
                limit: 10,
            })
            .expect("old search should start");
        let old = stream.effects.pop().expect("old search should queue");
        let (in_flight, _started, release) =
            controlled_in_flight(old, ExecutedEffect::Events(Vec::new()));
        stream.in_flight = Some(in_flight);

        stream
            .begin_command(ClientCommand::SearchCatalogModels {
                request_id: "catalog-new".to_owned(),
                query: "new".to_owned(),
                provider_id: None,
                limit: 10,
            })
            .expect("new search should start");

        assert!(release.send(()).is_err(), "old search should be canceled");
        assert!(stream.in_flight.is_none());
        let [
            QueuedEffect {
                effect:
                    ClientEffect::Catalog(CatalogEffect::SearchModels {
                        request_id, query, ..
                    }),
                ..
            },
        ] = stream.effects.as_slice()
        else {
            panic!("only the new catalog search should remain");
        };
        assert_eq!(request_id, "catalog-new");
        assert_eq!(query, "new");
    }

    #[test]
    fn template_search_supersession_is_keyed_by_session() {
        let mut stream = ready_state_stream();
        for (request_id, session_id) in [("a-old", "session-a"), ("b", "session-b")] {
            stream
                .begin_command(ClientCommand::SearchSessionTemplates {
                    request_id: request_id.to_owned(),
                    session_id: session_id.to_owned(),
                    query: request_id.to_owned(),
                    limit: 10,
                    kinds: Vec::new(),
                    target: ClientSessionTemplateSearchTarget::Resources,
                    cursor: None,
                })
                .expect("template search should start");
        }

        stream
            .begin_command(ClientCommand::SearchSessionTemplates {
                request_id: "a-new".to_owned(),
                session_id: "session-a".to_owned(),
                query: "new".to_owned(),
                limit: 10,
                kinds: Vec::new(),
                target: ClientSessionTemplateSearchTarget::Resources,
                cursor: None,
            })
            .expect("replacement search should start");

        let searches = stream
            .effects
            .iter()
            .filter_map(|queued| match &queued.effect {
                ClientEffect::Session(SessionEffect::SearchTemplates {
                    request_id,
                    session_id,
                    ..
                }) => Some((request_id.as_str(), session_id.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(searches, vec![("b", "session-b"), ("a-new", "session-a")]);
    }

    #[tokio::test]
    async fn final_config_interest_release_cancels_open_and_queues_one_fresh_reopen() {
        let mut stream = ready_state_stream();
        stream
            .begin_command(ClientCommand::RegisterConfigWatchInterest)
            .expect("config interest should register");
        let queued = stream.effects.pop().expect("config open should queue");
        let effect = queued.effect.clone();
        let generation = queued.access_generation;
        let ClientEffect::Config(ConfigEffect::Watch {
            open_id: old_open_id,
        }) = effect
        else {
            panic!("config open effect should carry identity");
        };
        stream.effects.push(queued.clone());
        let (in_flight, started, release) = controlled_in_flight(
            queued,
            ExecutedEffect::ConfigWatch {
                open_id: old_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        stream.in_flight = Some(in_flight);

        stream
            .next_or_dispatch(async move {
                started.await.ok()?;
                Some(ClientCommandRequest::new(
                    ClientCommand::ReleaseConfigWatchInterest,
                ))
            })
            .await;

        assert!(release.send(()).is_err(), "open future should be canceled");
        assert!(stream.in_flight.is_none());
        assert!(stream.effects.is_empty());
        assert!(stream.runtime.config_watch.is_none());
        assert_eq!(stream.runtime.reducer.state().config.watch.interests, 0);
        stream
            .begin_command(ClientCommand::RegisterConfigWatchInterest)
            .expect("config interest should reregister");
        assert_eq!(stream.effects.len(), 1);
        let fresh = stream.effects.pop().unwrap();
        let ClientEffect::Config(ConfigEffect::Watch {
            open_id: fresh_open_id,
        }) = fresh.effect.clone()
        else {
            panic!("fresh config open should carry identity");
        };
        assert!(fresh_open_id > old_open_id);
        stream.complete_effect(
            generation,
            &ClientEffect::Config(ConfigEffect::Watch {
                open_id: old_open_id,
            }),
            ExecutedEffect::ConfigWatch {
                open_id: old_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        assert!(stream.runtime.config_watch.is_none());
        stream.complete_effect(
            fresh.access_generation,
            &fresh.effect,
            ExecutedEffect::ConfigWatch {
                open_id: fresh_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        assert!(stream.runtime.config_watch.is_some());
    }

    #[tokio::test]
    async fn final_session_interest_release_cancels_open_and_queues_one_fresh_reopen() {
        let mut stream = ready_state_stream();
        stream
            .begin_command(ClientCommand::RegisterSessionWatchInterest)
            .expect("session interest should register");
        let queued = stream.effects.pop().expect("session open should queue");
        let effect = queued.effect.clone();
        let generation = queued.access_generation;
        let ClientEffect::Session(SessionEffect::Watch {
            open_id: old_open_id,
        }) = effect
        else {
            panic!("session open effect should carry identity");
        };
        stream.effects.push(queued.clone());
        let (in_flight, started, release) = controlled_in_flight(
            queued,
            ExecutedEffect::SessionWatch {
                open_id: old_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        stream.in_flight = Some(in_flight);

        stream
            .next_or_dispatch(async move {
                started.await.ok()?;
                Some(ClientCommandRequest::new(
                    ClientCommand::ReleaseSessionWatchInterest,
                ))
            })
            .await;

        assert!(release.send(()).is_err(), "open future should be canceled");
        assert!(stream.in_flight.is_none());
        assert!(stream.effects.is_empty());
        assert!(stream.runtime.session_watch.is_none());
        assert_eq!(stream.runtime.reducer.state().sessions.watch.interests, 0);
        stream
            .begin_command(ClientCommand::RegisterSessionWatchInterest)
            .expect("session interest should reregister");
        assert_eq!(stream.effects.len(), 1);
        let fresh = stream.effects.pop().unwrap();
        let ClientEffect::Session(SessionEffect::Watch {
            open_id: fresh_open_id,
        }) = fresh.effect.clone()
        else {
            panic!("fresh session open should carry identity");
        };
        assert!(fresh_open_id > old_open_id);
        stream.complete_effect(
            generation,
            &ClientEffect::Session(SessionEffect::Watch {
                open_id: old_open_id,
            }),
            ExecutedEffect::SessionWatch {
                open_id: old_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        assert!(stream.runtime.session_watch.is_none());
        stream.complete_effect(
            fresh.access_generation,
            &fresh.effect,
            ExecutedEffect::SessionWatch {
                open_id: fresh_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        assert!(stream.runtime.session_watch.is_some());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn timeline_interest_replacement_cancels_old_open_and_queues_only_new_key() {
        let mut stream = ready_state_stream();
        let old_key = TimelineWatchKey::new_with_projection(
            "session".to_owned(),
            Some(1),
            vec!["old".to_owned()],
            Some(5),
            crate::SessionTimelineProjectionMode::Effective,
        );
        stream
            .begin_command(ClientCommand::RegisterSessionTimelineWatchInterest {
                session_id: old_key.session_id.clone(),
                after_cursor: old_key.after_cursor,
                channels: old_key.channels.clone(),
                initial_limit: old_key.initial_limit,
            })
            .expect("old timeline interest should register");
        let queued = stream.effects.pop().expect("timeline open should queue");
        let effect = queued.effect.clone();
        let generation = queued.access_generation;
        let ClientEffect::Session(SessionEffect::WatchTimeline {
            open_id: old_open_id,
            ..
        }) = effect
        else {
            panic!("timeline open effect should carry identity");
        };
        stream.effects.push(queued.clone());
        let (in_flight, started, release) = controlled_in_flight(
            queued,
            ExecutedEffect::TimelineWatch {
                key: old_key.clone(),
                open_id: old_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        stream.in_flight = Some(in_flight);

        stream
            .next_or_dispatch(async move {
                started.await.ok()?;
                Some(ClientCommandRequest::new(
                    ClientCommand::ReleaseSessionTimelineWatchInterest {
                        session_id: "session".to_owned(),
                    },
                ))
            })
            .await;

        assert!(release.send(()).is_err(), "old open should be canceled");
        assert!(stream.in_flight.is_none());
        assert!(stream.effects.is_empty());
        assert!(stream.runtime.timeline_watches.is_empty());
        let new_key = old_key.clone();
        stream
            .begin_command(ClientCommand::RegisterSessionTimelineWatchInterest {
                session_id: new_key.session_id.clone(),
                after_cursor: new_key.after_cursor,
                channels: new_key.channels.clone(),
                initial_limit: new_key.initial_limit,
            })
            .expect("replacement timeline interest should register");
        assert_eq!(stream.effects.len(), 1);
        let fresh = stream.effects.pop().unwrap();
        let ClientEffect::Session(SessionEffect::WatchTimeline {
            key: fresh_key,
            open_id: fresh_open_id,
        }) = fresh.effect.clone()
        else {
            panic!("fresh timeline open should carry identity");
        };
        assert_eq!(fresh_key, new_key);
        assert!(fresh_open_id > old_open_id);
        stream.complete_effect(
            generation,
            &ClientEffect::Session(SessionEffect::WatchTimeline {
                key: old_key.clone(),
                open_id: old_open_id,
            }),
            ExecutedEffect::TimelineWatch {
                key: old_key,
                open_id: old_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        assert!(stream.runtime.timeline_watches.is_empty());
        stream.complete_effect(
            fresh.access_generation,
            &fresh.effect,
            ExecutedEffect::TimelineWatch {
                key: fresh_key.clone(),
                open_id: fresh_open_id,
                result: Ok(Box::pin(stream::pending())),
            },
        );
        assert!(stream.runtime.timeline_watches.contains_key(&fresh_key));
    }

    #[tokio::test]
    async fn denied_state_parks_but_commands_still_wake_stream() {
        let mut stream =
            ClientRuntime::for_endpoint("http://127.0.0.1:0").into_state_stream(no_sleep);
        stream.runtime.reducer.expire_waltr_access("login required");

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), stream.next())
                .await
                .is_err()
        );

        let event = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            stream.next_or_dispatch(async {
                Some(ClientCommandRequest::new(ClientCommand::StartWaltrLogin))
            }),
        )
        .await
        .expect("login command should wake parked stream");
        let ClientStateStreamEvent::Snapshot(snapshot) = event else {
            panic!("command should publish a snapshot");
        };
        assert_eq!(snapshot.access.status, AccessStatus::LoginInProgress);
    }

    #[test]
    fn request_timeline_command_uses_requested_direction() {
        let mut runtime = ClientStateStream {
            runtime: ClientRuntime::for_endpoint("http://127.0.0.1:0"),
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };

        let update = runtime.begin_timeline_command(ClientCommand::RequestSessionTimeline {
            session_id: "session".to_owned(),
            direction: ClientTimelineDirection::Backward,
            cursor: Some(42),
            limit: 25,
            channels: vec!["tool_result".to_owned()],
        });

        assert!(matches!(
            update.effects(),
            [ClientEffect::Session(SessionEffect::GetTimeline {
                query,
                projection_mode: crate::SessionTimelineProjectionMode::Effective,
                ..
            })]
                if query.as_ref().is_some_and(|query| {
                    query.direction == TimelineDirection::Backward as i32
                        && query.cursor == Some(42)
                        && query.limit == 25
                        && query.channels == ["tool_result"]
                })
        ));

        runtime
            .runtime
            .reducer
            .apply(ClientEvent::Session(SessionEvent::TimelineLoaded {
                session_id: "session".to_owned(),
                request_id: 0,
                baseline_mode: crate::SessionTimelineProjectionMode::Raw,
                baseline_revision: None,
                page: crate::SessionTimelinePage {
                    projection_mode: crate::SessionTimelineProjectionMode::Effective,
                    entries: Vec::new(),
                    projection: Some(crate::TimelineProjectionMetadata {
                        revision: 2,
                        raw_page_boundary: Some(7),
                        has_more: true,
                    }),
                },
            }));
        let update = runtime.begin_timeline_command(ClientCommand::RequestSessionTimeline {
            session_id: "session".to_owned(),
            direction: ClientTimelineDirection::Backward,
            cursor: Some(1),
            limit: 25,
            channels: Vec::new(),
        });
        assert!(matches!(
            update.effects(),
            [ClientEffect::Session(SessionEffect::GetTimeline { query, .. })]
                if query.as_ref().is_some_and(|query| query.cursor == Some(7))
        ));
    }

    #[test]
    fn overlapping_runtime_timeline_pages_get_distinct_identity_and_same_baseline() {
        let mut runtime = ClientStateStream {
            runtime: ClientRuntime::for_endpoint("http://127.0.0.1:0"),
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        };
        let command = || ClientCommand::RequestSessionTimeline {
            session_id: "session".to_owned(),
            direction: ClientTimelineDirection::Backward,
            cursor: None,
            limit: 25,
            channels: Vec::new(),
        };

        let first = runtime.begin_timeline_command(command());
        let second = runtime.begin_timeline_command(command());
        let descriptor = |update: &ClientUpdate| match update.effects() {
            [
                ClientEffect::Session(SessionEffect::GetTimeline {
                    request_id,
                    baseline_mode,
                    baseline_revision,
                    ..
                }),
            ] => (*request_id, *baseline_mode, *baseline_revision),
            effects => panic!("unexpected timeline effects: {effects:?}"),
        };
        let first = descriptor(&first);
        let second = descriptor(&second);

        assert!(second.0 > first.0);
        assert_eq!(first.1, crate::SessionTimelineProjectionMode::Raw);
        assert_eq!(first.1, second.1);
        assert_eq!(first.2, second.2);
    }

    async fn no_sleep(_: u64) {}

    fn ready_state_stream() -> ClientStateStream {
        let mut runtime = ClientRuntime::for_endpoint("http://127.0.0.1:0");
        runtime
            .reducer
            .apply(ClientEvent::Status(StatusEvent::WatchChanged(
                SystemStatus { ready: true },
            )));
        ClientStateStream {
            runtime,
            phase: ClientStateStreamPhase::Ready,
            effects: Vec::new(),
            in_flight: None,
            sleep: Rc::new(|delay_ms| Box::pin(no_sleep(delay_ms))),
        }
    }

    fn controlled_in_flight(
        queued: QueuedEffect,
        executed: ExecutedEffect,
    ) -> (InFlightEffect, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let effect = queued.effect;
        let future = async move {
            started_tx.send(()).ok();
            release_rx.await.ok();
            executed
        }
        .boxed_local();
        (
            InFlightEffect {
                access_generation: queued.access_generation,
                effect,
                future,
            },
            started_rx,
            release_tx,
        )
    }

    fn register_timeline_watch(runtime: &mut ClientRuntime, key: &TimelineWatchKey) {
        runtime.reducer.apply(ClientEvent::Session(
            SessionEvent::TimelineWatchInterestRegistered {
                session_id: key.session_id.clone(),
                after_cursor: key.after_cursor,
                channels: key.channels.clone(),
                initial_limit: key.initial_limit,
                projection_mode: key.projection_mode,
            },
        ));
        runtime
            .reducer
            .apply(ClientEvent::Session(SessionEvent::TimelineWatchStarted {
                key: key.clone(),
            }));
    }

    fn timeline_event(session_id: &str, cursor: u64) -> TimelineEvent {
        TimelineEvent {
            session_id: session_id.to_owned(),
            cursor,
            item_id: format!("i{cursor}"),
            ..TimelineEvent::default()
        }
    }
}
