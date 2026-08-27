use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use waltr::grpc::waltr::session::v1::{
    ProjectedTimelineEvent, SessionCapabilities, SessionCoordinationPhase,
    SessionCoordinationReason, SessionCoordinationState, SessionProgress, SessionProgressPhase,
    SessionPromptDisposition, SessionRunStage, SyntheticTimelineEvent, TimelinePresentation,
    projected_timeline_event, timeline_content,
};

use crate::{
    AuthenticationFailureSnapshot, InferenceProviderSettings, InferenceSettings,
    InferenceSettingsMatch, OutboundAuthenticationFailure, ProviderKindView, SessionMetrics,
    SessionPhase, SessionStatus, SessionSummary, SuspensionKind, TimelineContent, TimelineEvent,
    TimelineEventKind, TimelineStatus,
};

/// Stable selectable session agent view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SelectableAgentView {
    /// Exact selectable agent name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
}

impl From<crate::SelectableAgent> for SelectableAgentView {
    fn from(agent: crate::SelectableAgent) -> Self {
        Self {
            name: agent.name,
            description: agent.description,
        }
    }
}

/// Stable session summary view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    /// Server-assigned session id.
    pub id: String,
    /// Exact selectable agent currently assigned to the session.
    pub agent_name: String,
    /// Session revision.
    pub revision: u64,
    /// Current session status.
    pub status: SessionStatusView,
    /// Ephemeral process-local coordination reported by the server.
    pub coordination: SessionCoordinationView,
    /// Agent-owned unified lifecycle projection.
    pub progress: SessionProgressView,
    /// Agent-owned action availability projection.
    pub capabilities: SessionCapabilitiesView,
    /// Current prompt, when the session has one.
    pub current_prompt: Option<String>,
    /// Creation timestamp, when supplied by the server.
    pub created_at: Option<TimestampView>,
    /// Last update timestamp, when supplied by the server.
    pub updated_at: Option<TimestampView>,
    /// Last activity timestamp, when supplied by the server.
    pub last_activity_at: Option<TimestampView>,
    /// Inference settings snapshot stored with the session.
    pub inference_settings: Option<InferenceSettingsView>,
    /// Aggregate usage and cost derived from journaled provider turns.
    pub metrics: SessionMetricsView,
    /// Typed associations derived from known session metadata keys.
    pub associations: SessionAssociationsView,
    /// Complete JSON session metadata object.
    #[ts(type = "Record<string, unknown>")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Stable associations derived from known session metadata keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionAssociationsView {
    /// Parent session that created this direct task session.
    pub task_parent_session_id: Option<String>,
    /// Direct task description supplied by the parent session.
    pub task_description: Option<String>,
    /// Routine invocation that owns this helper session.
    pub routine_invocation_id: Option<String>,
}

impl From<&BTreeMap<String, serde_json::Value>> for SessionAssociationsView {
    fn from(metadata: &BTreeMap<String, serde_json::Value>) -> Self {
        let string_value = |key| {
            metadata
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };

        Self {
            task_parent_session_id: string_value("waltr.task.parent_session_id"),
            task_description: string_value("waltr.task.description"),
            routine_invocation_id: string_value("waltr.routine.invocation_id"),
        }
    }
}

impl SessionView {
    /// Return a nonempty string title from session metadata.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.metadata
            .get("title")
            .and_then(serde_json::Value::as_str)
            .filter(|title| !title.is_empty())
    }

    /// Return the title when present, otherwise the stable session id.
    #[must_use]
    pub fn display_label(&self) -> &str {
        self.title().unwrap_or(&self.id)
    }
}

/// Stable aggregate session metrics view shared by Waltr shells.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetricsView {
    /// Non-cache/base-priced input tokens consumed by provider requests.
    pub input_tokens: u64,
    /// Output/completion tokens produced by provider responses.
    pub output_tokens: u64,
    /// Input tokens served from provider cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to provider cache.
    pub cache_write_tokens: u64,
    /// Image input tokens consumed by provider requests.
    pub image_input_tokens: u64,
    /// Image count consumed by provider requests.
    pub image_input_count: u64,
    /// Total billable/token-counted input and output tokens.
    pub total_tokens: u64,
    /// Full input tokens represented by provider usage reports, including cache reads and writes.
    pub total_input_tokens: u64,
    /// Full context tokens represented by provider usage reports, including cache reads, writes, and output.
    pub context_tokens: u64,
    /// Currency for total cost, or `mixed` when journaled turns differ.
    pub cost_currency: Option<String>,
    /// Total cost in currency micro-units.
    pub total_cost_micros: u64,
}

/// Stable session inference settings view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InferenceSettingsView {
    /// Stored provider inference settings.
    pub provider: Option<InferenceProviderSettingsView>,
}

/// Stable provider inference settings view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InferenceProviderSettingsView {
    /// Provider key selected when inference settings were stored.
    pub key: String,
    /// Provider type selected when inference settings were stored.
    pub kind: ProviderKindView,
    /// Provider URL selected when inference settings were stored.
    pub url: String,
    /// Exact provider model id selected when inference settings were stored.
    pub model_id: String,
    /// Per-session reasoning effort, when configured.
    pub reasoning_effort: Option<String>,
    /// Whether the stored provider/model still matches current config.
    pub match_status: InferenceSettingsMatchView,
}

/// Stable inference settings match status shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum InferenceSettingsMatchView {
    /// Match status was omitted or intentionally unspecified.
    Unspecified,
    /// Stored inference settings still match current server config.
    Configured,
    /// Stored inference settings reference a missing provider or model.
    Missing,
    /// Stored inference settings differ from current server config.
    Changed,
    /// Match status is unknown to this client version.
    Unknown,
}

/// Stable session status view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusView {
    /// Session phase as a shell-friendly enum.
    pub phase: SessionPhaseView,
    /// Suspension reason when the session phase is suspended.
    pub suspension_kind: Option<SessionSuspensionKindView>,
    /// Failure message, when phase is failed.
    pub failed_message: String,
    /// Structured outbound authentication failure, when present.
    pub authentication_failure: Option<AuthenticationFailureSnapshot>,
}

/// Stable shell-neutral session coordination state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionCoordinationView {
    /// Current process-local coordination phase.
    pub phase: SessionCoordinationPhaseView,
    /// Machine-readable reason descriptor, when supplied.
    pub reason: Option<SessionCoordinationReasonView>,
    /// Number of queued user submissions.
    pub queued_count: u32,
    /// Whether continuation is armed after the current pause.
    pub continuation_armed: bool,
}

/// Stable shell-neutral coordination phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionCoordinationPhaseView {
    /// No process-local coordination blocks progress.
    #[default]
    Ready,
    /// A run is active.
    Running,
    /// Post-run coordination is selecting or preparing follow-up work.
    CompletionBarrier,
    /// A host or Routine owns a pause.
    Paused,
    /// Phase is unknown to this client version.
    Unknown,
}

/// Stable machine-readable coordination reason descriptor.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionCoordinationReasonView {
    pub code: String,
    pub source: Option<String>,
    pub trigger: Option<String>,
}

/// Stable shell-neutral unified session progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionProgressView {
    /// Current unified lifecycle phase.
    pub phase: SessionProgressPhaseView,
    /// Detailed durable active stage, when supplied.
    pub run_stage: Option<SessionRunStageView>,
    /// Machine-readable barrier or pause reason, when supplied.
    pub reason: Option<SessionCoordinationReasonView>,
    /// Number of queued logical submissions.
    pub queued_count: u64,
    /// Whether a continuation is armed ahead of queued work.
    pub continuation_armed: bool,
    /// Agent-owned activity projection used by hosted MCP.
    pub running: bool,
}

/// Stable unified session lifecycle phase.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionProgressPhaseView {
    #[default]
    Unknown,
    Idle,
    Running,
    CompletionBarrier,
    Paused,
    AwaitingToolResults,
    AwaitingExternalInput,
    Interrupted,
    Finished,
    Failed,
}

/// Stable detailed stage for active durable work.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionRunStageView {
    Unknown,
    BuildingRequest,
    StreamingProvider,
    RetryingProvider,
    EvaluatingToolCall,
    RunningTool,
}

/// Stable agent-owned session action availability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilitiesView {
    /// How a newly submitted prompt will be handled.
    pub prompt_disposition: SessionPromptDispositionView,
    /// Whether the pending external request can be answered now.
    pub can_respond_external_request: bool,
    /// Whether a real process-local active run can be interrupted.
    pub can_interrupt: bool,
}

/// Stable prompt submission disposition.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionPromptDispositionView {
    #[default]
    Unknown,
    Immediate,
    Queued,
    SupersedeSuspension,
}

/// Stable session suspension reason shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionSuspensionKindView {
    /// Suspension kind was omitted or intentionally unspecified.
    Unspecified,
    /// Session is waiting for outstanding tool results.
    AwaitingToolResults,
    /// Session is waiting for external input.
    AwaitingExternalInput,
    /// Session execution was interrupted.
    Interrupted,
    /// Suspension kind is unknown to this client version.
    Unknown,
}

/// Stable session phase view shared by Waltr shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionPhaseView {
    /// Session phase was omitted or intentionally unspecified.
    Unspecified,
    /// Session is idle.
    Idle,
    /// Session is building a provider request.
    BuildingRequest,
    /// Session is streaming from a provider.
    StreamingProvider,
    /// Session is waiting before retrying a provider request.
    RetryingProvider,
    /// Session is evaluating a tool call.
    EvaluatingToolCall,
    /// Session is running a tool.
    RunningTool,
    /// Session is suspended.
    Suspended,
    /// Session has finished.
    Finished,
    /// Session failed.
    Failed,
    /// Session phase is unknown to this client version.
    Unknown,
}

/// Stable timestamp view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TimestampView {
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Nanoseconds within the second.
    pub nanos: i32,
}

/// Stable session timeline view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionTimelineView {
    /// Already-projected timeline entries ordered by server cursor.
    pub events: Vec<ProjectedTimelineEntryView>,
    /// Revision of the current effective projection.
    pub projection_revision: Option<u64>,
    /// Raw page boundary for the next effective backward request.
    pub raw_page_boundary: Option<u64>,
    /// Whether another effective backward page is available.
    pub has_more: bool,
    /// Timeline page load operation snapshot.
    pub load: crate::OperationSnapshot,
    /// Timeline watch operation snapshot.
    pub watch: crate::OperationSnapshot,
}

/// Stable already-projected timeline entry.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "entryKind", rename_all = "camelCase")]
pub enum ProjectedTimelineEntryView {
    /// Durable timeline entry retaining the existing action-capable view.
    Durable {
        identity: String,
        presentation: TimelinePresentationView,
        event: TimelineEventView,
    },
    /// Synthetic projected entry without durable item or action identity.
    Synthetic {
        identity: String,
        presentation: TimelinePresentationView,
        event: SyntheticTimelineEventView,
    },
}

impl ProjectedTimelineEntryView {
    /// Return the server ordering cursor without implying durable action support.
    #[must_use]
    pub const fn ordering_cursor(&self) -> u64 {
        match self {
            Self::Durable { event, .. } => event.cursor,
            Self::Synthetic { event, .. } => event.ordering_cursor,
        }
    }

    /// Return the durable action-capable event, when this entry is durable.
    #[must_use]
    pub const fn durable_event(&self) -> Option<&TimelineEventView> {
        match self {
            Self::Durable { event, .. } => Some(event),
            Self::Synthetic { .. } => None,
        }
    }

    /// Return the synthetic presentation-only event, when projected by the server.
    #[must_use]
    pub const fn synthetic_event(&self) -> Option<&SyntheticTimelineEventView> {
        match self {
            Self::Durable { .. } => None,
            Self::Synthetic { event, .. } => Some(event),
        }
    }
}

/// Stable presentation selected by the server projection.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TimelinePresentationView {
    Unspecified,
    Durable,
    MaskReplacement,
    Unknown,
}

/// Stable synthetic event without durable action fields.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SyntheticTimelineEventView {
    /// Server ordering position; this is not a durable action cursor.
    pub ordering_cursor: u64,
    pub session_id: String,
    pub turn: u64,
    pub kind: TimelineEventKindView,
    pub channel: String,
    pub content: TimelineContentView,
    pub status: TimelineStatusView,
}

/// Stable timeline event view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEventView {
    /// Durable timeline cursor.
    pub cursor: u64,
    /// Session id that owns the event.
    pub session_id: String,
    /// Turn number that produced the event.
    pub turn: u64,
    /// Timeline item id.
    pub item_id: String,
    /// Parent timeline item id, when present.
    pub parent_id: Option<String>,
    /// Event kind.
    pub kind: TimelineEventKindView,
    /// Display channel, when present.
    pub channel: Option<String>,
    /// Event content.
    pub content: TimelineContentView,
    /// Event status, when present.
    pub status: Option<TimelineStatusView>,
    /// JSON metadata object as returned by the server.
    pub metadata_json: String,
}

/// Stable timeline event kind view shared by Waltr shells.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TimelineEventKindView {
    Unspecified,
    ItemStarted,
    ContentDelta,
    ItemUpdated,
    ItemFinished,
    ExternalRequestOpened,
    ExternalRequestResolved,
    SessionStatusChanged,
    Unknown,
}

/// Stable timeline status view shared by Waltr shells.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TimelineStatusView {
    Unspecified,
    Idle,
    BuildingRequest,
    StreamingProvider,
    RetryingProvider,
    EvaluatingToolCall,
    RunningTool,
    Pending,
    AwaitingToolResults,
    AwaitingExternalInput,
    Interrupted,
    Resolved,
    Finished,
    Failed,
    Canceled,
    Unknown,
}

/// Stable timeline content view shared by Waltr shells.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TimelineContentView {
    Empty,
    TextDelta {
        delta: String,
    },
    TextSnapshot {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments_json: String,
    },
    ToolResult {
        id: String,
        name: String,
        content: String,
        error: bool,
    },
    ExternalRequest {
        request: Option<ExternalRequestView>,
    },
    ContentRef {
        content_id: String,
        bytes: u64,
    },
    Json {
        value_json: String,
    },
    Unknown,
}

/// Stable external request view embedded in timeline content.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRequestView {
    pub id: String,
    pub kind: i32,
    pub payload_json: String,
}

impl From<SessionSummary> for SessionView {
    fn from(session: SessionSummary) -> Self {
        let metadata = session.metadata.map_or_else(BTreeMap::new, |metadata| {
            metadata
                .fields
                .into_iter()
                .map(|(key, value)| (key, proto_value_to_json(value)))
                .collect()
        });
        Self {
            id: session.id,
            agent_name: session.agent_name,
            revision: session.revision,
            status: session
                .status
                .map_or_else(SessionStatusView::default, Into::into),
            coordination: session
                .coordination
                .map_or_else(SessionCoordinationView::default, Into::into),
            progress: session
                .progress
                .map_or_else(SessionProgressView::default, Into::into),
            capabilities: session
                .capabilities
                .map_or_else(SessionCapabilitiesView::default, Into::into),
            current_prompt: session.current_prompt,
            created_at: session.created_at.map(Into::into),
            updated_at: session.updated_at.map(Into::into),
            last_activity_at: session.last_activity_at.map(Into::into),
            inference_settings: session.inference_settings.map(Into::into),
            metrics: session
                .metrics
                .map_or_else(SessionMetricsView::default, Into::into),
            associations: SessionAssociationsView::from(&metadata),
            metadata,
        }
    }
}

impl From<SessionCoordinationState> for SessionCoordinationView {
    fn from(coordination: SessionCoordinationState) -> Self {
        Self {
            phase: coordination.phase.into(),
            reason: coordination.reason.map(Into::into),
            queued_count: coordination.queued_count,
            continuation_armed: coordination.continuation_armed,
        }
    }
}

impl From<i32> for SessionCoordinationPhaseView {
    fn from(value: i32) -> Self {
        match SessionCoordinationPhase::try_from(value) {
            Ok(SessionCoordinationPhase::Unspecified | SessionCoordinationPhase::Ready) => {
                Self::Ready
            }
            Ok(SessionCoordinationPhase::Running) => Self::Running,
            Ok(SessionCoordinationPhase::CompletionBarrier) => Self::CompletionBarrier,
            Ok(SessionCoordinationPhase::Paused) => Self::Paused,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<SessionCoordinationReason> for SessionCoordinationReasonView {
    fn from(reason: SessionCoordinationReason) -> Self {
        Self {
            code: reason.code,
            source: reason.source,
            trigger: reason.trigger,
        }
    }
}

impl From<SessionProgress> for SessionProgressView {
    fn from(progress: SessionProgress) -> Self {
        Self {
            phase: progress.phase.into(),
            run_stage: progress.run_stage.map(Into::into),
            reason: progress.reason.map(Into::into),
            queued_count: progress.queued_count,
            continuation_armed: progress.continuation_armed,
            running: progress.running,
        }
    }
}

impl From<i32> for SessionProgressPhaseView {
    fn from(value: i32) -> Self {
        match SessionProgressPhase::try_from(value) {
            Ok(SessionProgressPhase::Unspecified) | Err(_) => Self::Unknown,
            Ok(SessionProgressPhase::Idle) => Self::Idle,
            Ok(SessionProgressPhase::Running) => Self::Running,
            Ok(SessionProgressPhase::CompletionBarrier) => Self::CompletionBarrier,
            Ok(SessionProgressPhase::Paused) => Self::Paused,
            Ok(SessionProgressPhase::AwaitingToolResults) => Self::AwaitingToolResults,
            Ok(SessionProgressPhase::AwaitingExternalInput) => Self::AwaitingExternalInput,
            Ok(SessionProgressPhase::Interrupted) => Self::Interrupted,
            Ok(SessionProgressPhase::Finished) => Self::Finished,
            Ok(SessionProgressPhase::Failed) => Self::Failed,
        }
    }
}

impl From<i32> for SessionRunStageView {
    fn from(value: i32) -> Self {
        match SessionRunStage::try_from(value) {
            Ok(SessionRunStage::Unspecified) | Err(_) => Self::Unknown,
            Ok(SessionRunStage::BuildingRequest) => Self::BuildingRequest,
            Ok(SessionRunStage::StreamingProvider) => Self::StreamingProvider,
            Ok(SessionRunStage::RetryingProvider) => Self::RetryingProvider,
            Ok(SessionRunStage::EvaluatingToolCall) => Self::EvaluatingToolCall,
            Ok(SessionRunStage::RunningTool) => Self::RunningTool,
        }
    }
}

impl From<SessionCapabilities> for SessionCapabilitiesView {
    fn from(capabilities: SessionCapabilities) -> Self {
        Self {
            prompt_disposition: capabilities.prompt_disposition.into(),
            can_respond_external_request: capabilities.can_respond_external_request,
            can_interrupt: capabilities.can_interrupt,
        }
    }
}

impl From<i32> for SessionPromptDispositionView {
    fn from(value: i32) -> Self {
        match SessionPromptDisposition::try_from(value) {
            Ok(SessionPromptDisposition::Unspecified) | Err(_) => Self::Unknown,
            Ok(SessionPromptDisposition::Immediate) => Self::Immediate,
            Ok(SessionPromptDisposition::Queued) => Self::Queued,
            Ok(SessionPromptDisposition::SupersedeSuspension) => Self::SupersedeSuspension,
        }
    }
}

fn proto_value_to_json(value: prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;

    match value.kind {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(value)) => serde_json::Value::Bool(value),
        Some(Kind::NumberValue(value)) => serde_json::Number::from_f64(value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Some(Kind::StringValue(value)) => serde_json::Value::String(value),
        Some(Kind::ListValue(value)) => {
            serde_json::Value::Array(value.values.into_iter().map(proto_value_to_json).collect())
        }
        Some(Kind::StructValue(value)) => serde_json::Value::Object(
            value
                .fields
                .into_iter()
                .map(|(key, value)| (key, proto_value_to_json(value)))
                .collect(),
        ),
    }
}

impl From<SessionMetrics> for SessionMetricsView {
    fn from(metrics: SessionMetrics) -> Self {
        Self {
            input_tokens: metrics.input_tokens,
            output_tokens: metrics.output_tokens,
            cache_read_tokens: metrics.cache_read_tokens,
            cache_write_tokens: metrics.cache_write_tokens,
            image_input_tokens: metrics.image_input_tokens,
            image_input_count: metrics.image_input_count,
            total_tokens: metrics.total_tokens,
            total_input_tokens: metrics.total_input_tokens,
            context_tokens: metrics.context_tokens,
            cost_currency: metrics.cost_currency,
            total_cost_micros: metrics.total_cost_micros,
        }
    }
}

impl From<&SessionSummary> for SessionView {
    fn from(session: &SessionSummary) -> Self {
        session.clone().into()
    }
}

impl Default for SessionStatusView {
    fn default() -> Self {
        Self {
            phase: SessionPhaseView::Unspecified,
            suspension_kind: None,
            failed_message: String::new(),
            authentication_failure: None,
        }
    }
}

impl From<SessionStatus> for SessionStatusView {
    fn from(status: SessionStatus) -> Self {
        let is_suspended = status.phase == SessionPhase::Suspended as i32;
        let suspension_kind = is_suspended
            .then_some(status.suspension)
            .flatten()
            .map(|suspension| suspension.kind.into());
        let authentication_failure = (status.phase == SessionPhase::Failed as i32)
            .then_some(status.authentication_failure)
            .flatten()
            .and_then(OutboundAuthenticationFailure::from_proto)
            .map(Into::into);
        Self {
            phase: status.phase.into(),
            suspension_kind,
            failed_message: status.failed_message,
            authentication_failure,
        }
    }
}

impl From<i32> for SessionSuspensionKindView {
    fn from(value: i32) -> Self {
        match SuspensionKind::try_from(value) {
            Ok(SuspensionKind::Unspecified) => Self::Unspecified,
            Ok(SuspensionKind::AwaitingToolResults) => Self::AwaitingToolResults,
            Ok(SuspensionKind::AwaitingExternalInput) => Self::AwaitingExternalInput,
            Ok(SuspensionKind::Interrupted) => Self::Interrupted,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<i32> for SessionPhaseView {
    fn from(value: i32) -> Self {
        match SessionPhase::try_from(value) {
            Ok(SessionPhase::Unspecified) => Self::Unspecified,
            Ok(SessionPhase::Idle) => Self::Idle,
            Ok(SessionPhase::BuildingRequest) => Self::BuildingRequest,
            Ok(SessionPhase::StreamingProvider) => Self::StreamingProvider,
            Ok(SessionPhase::RetryingProvider) => Self::RetryingProvider,
            Ok(SessionPhase::EvaluatingToolCall) => Self::EvaluatingToolCall,
            Ok(SessionPhase::RunningTool) => Self::RunningTool,
            Ok(SessionPhase::Suspended) => Self::Suspended,
            Ok(SessionPhase::Finished) => Self::Finished,
            Ok(SessionPhase::Failed) => Self::Failed,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<InferenceSettings> for InferenceSettingsView {
    fn from(settings: InferenceSettings) -> Self {
        Self {
            provider: settings.provider.map(Into::into),
        }
    }
}

impl From<InferenceProviderSettings> for InferenceProviderSettingsView {
    fn from(provider: InferenceProviderSettings) -> Self {
        Self {
            key: provider.key,
            kind: provider.r#type.into(),
            url: provider.url,
            model_id: provider.model_id,
            reasoning_effort: provider.reasoning_effort,
            match_status: provider.r#match.into(),
        }
    }
}

impl From<i32> for InferenceSettingsMatchView {
    fn from(value: i32) -> Self {
        match InferenceSettingsMatch::try_from(value) {
            Ok(InferenceSettingsMatch::Unspecified) => Self::Unspecified,
            Ok(InferenceSettingsMatch::Configured) => Self::Configured,
            Ok(InferenceSettingsMatch::Missing) => Self::Missing,
            Ok(InferenceSettingsMatch::Changed) => Self::Changed,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<prost_types::Timestamp> for TimestampView {
    fn from(timestamp: prost_types::Timestamp) -> Self {
        Self {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }
    }
}

impl From<TimelineEvent> for TimelineEventView {
    fn from(event: TimelineEvent) -> Self {
        Self {
            cursor: event.cursor,
            session_id: event.session_id,
            turn: event.turn,
            item_id: event.item_id,
            parent_id: event.parent_id,
            kind: event.kind.into(),
            channel: event.channel,
            content: event.content.map_or(TimelineContentView::Empty, Into::into),
            status: event.status.map(TimelineStatusView::from),
            metadata_json: event.metadata_json,
        }
    }
}

impl TryFrom<ProjectedTimelineEvent> for ProjectedTimelineEntryView {
    type Error = ();

    fn try_from(entry: ProjectedTimelineEvent) -> Result<Self, Self::Error> {
        let presentation = TimelinePresentationView::from(entry.presentation);
        match entry.event.ok_or(())? {
            projected_timeline_event::Event::Durable(event) => Ok(Self::Durable {
                identity: format!("durable:{}:{}", event.cursor, event.item_id),
                presentation,
                event: event.into(),
            }),
            projected_timeline_event::Event::Synthetic(event) => Ok(Self::Synthetic {
                identity: format!("mask:{}", entry.mask_id.ok_or(())?),
                presentation,
                event: event.into(),
            }),
        }
    }
}

impl From<i32> for TimelinePresentationView {
    fn from(value: i32) -> Self {
        match TimelinePresentation::try_from(value) {
            Ok(TimelinePresentation::Unspecified) => Self::Unspecified,
            Ok(TimelinePresentation::Durable) => Self::Durable,
            Ok(TimelinePresentation::MaskReplacement) => Self::MaskReplacement,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<SyntheticTimelineEvent> for SyntheticTimelineEventView {
    fn from(event: SyntheticTimelineEvent) -> Self {
        Self {
            ordering_cursor: event.cursor,
            session_id: event.session_id,
            turn: event.turn,
            kind: event.kind.into(),
            channel: event.channel,
            content: event.content.map_or(TimelineContentView::Empty, Into::into),
            status: event.status.into(),
        }
    }
}

impl From<TimelineContent> for TimelineContentView {
    fn from(content: TimelineContent) -> Self {
        match content.content {
            Some(timeline_content::Content::Empty(_)) | None => Self::Empty,
            Some(timeline_content::Content::TextDelta(content)) => Self::TextDelta {
                delta: content.delta,
            },
            Some(timeline_content::Content::TextSnapshot(content)) => {
                Self::TextSnapshot { text: content.text }
            }
            Some(timeline_content::Content::ToolCall(content)) => Self::ToolCall {
                id: content.id,
                name: content.name,
                arguments_json: content.arguments_json,
            },
            Some(timeline_content::Content::ToolResult(content)) => Self::ToolResult {
                id: content.id,
                name: content.name,
                content: content.content,
                error: content.error,
            },
            Some(timeline_content::Content::ExternalRequest(content)) => Self::ExternalRequest {
                request: content.request.map(|request| ExternalRequestView {
                    id: request.id,
                    kind: request.kind,
                    payload_json: request.payload_json,
                }),
            },
            Some(timeline_content::Content::ContentRef(content)) => Self::ContentRef {
                content_id: content.content_id,
                bytes: content.bytes,
            },
            Some(timeline_content::Content::Json(content)) => Self::Json {
                value_json: content.value_json,
            },
        }
    }
}

impl From<i32> for TimelineEventKindView {
    fn from(value: i32) -> Self {
        match TimelineEventKind::try_from(value) {
            Ok(TimelineEventKind::Unspecified) => Self::Unspecified,
            Ok(TimelineEventKind::ItemStarted) => Self::ItemStarted,
            Ok(TimelineEventKind::ContentDelta) => Self::ContentDelta,
            Ok(TimelineEventKind::ItemUpdated) => Self::ItemUpdated,
            Ok(TimelineEventKind::ItemFinished) => Self::ItemFinished,
            Ok(TimelineEventKind::ExternalRequestOpened) => Self::ExternalRequestOpened,
            Ok(TimelineEventKind::ExternalRequestResolved) => Self::ExternalRequestResolved,
            Ok(TimelineEventKind::SessionStatusChanged) => Self::SessionStatusChanged,
            Err(_) => Self::Unknown,
        }
    }
}

impl From<i32> for TimelineStatusView {
    fn from(value: i32) -> Self {
        match TimelineStatus::try_from(value) {
            Ok(TimelineStatus::Unspecified) => Self::Unspecified,
            Ok(TimelineStatus::Idle) => Self::Idle,
            Ok(TimelineStatus::BuildingRequest) => Self::BuildingRequest,
            Ok(TimelineStatus::StreamingProvider) => Self::StreamingProvider,
            Ok(TimelineStatus::RetryingProvider) => Self::RetryingProvider,
            Ok(TimelineStatus::EvaluatingToolCall) => Self::EvaluatingToolCall,
            Ok(TimelineStatus::RunningTool) => Self::RunningTool,
            Ok(TimelineStatus::Pending) => Self::Pending,
            Ok(TimelineStatus::AwaitingToolResults) => Self::AwaitingToolResults,
            Ok(TimelineStatus::AwaitingExternalInput) => Self::AwaitingExternalInput,
            Ok(TimelineStatus::Interrupted) => Self::Interrupted,
            Ok(TimelineStatus::Resolved) => Self::Resolved,
            Ok(TimelineStatus::Finished) => Self::Finished,
            Ok(TimelineStatus::Failed) => Self::Failed,
            Ok(TimelineStatus::Canceled) => Self::Canceled,
            Err(_) => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waltr::grpc::waltr::session::v1::{SessionSummary, Suspension};
    use prost_types::value::Kind;

    #[test]
    fn session_view_preserves_metadata_and_uses_only_nonempty_string_titles() {
        let metadata = prost_types::Struct {
            fields: BTreeMap::from([
                (
                    "title".to_owned(),
                    proto_value(Kind::StringValue("Live title".to_owned())),
                ),
                ("null".to_owned(), proto_value(Kind::NullValue(0))),
                ("bool".to_owned(), proto_value(Kind::BoolValue(true))),
                ("number".to_owned(), proto_value(Kind::NumberValue(42.0))),
                (
                    "array".to_owned(),
                    proto_value(Kind::ListValue(prost_types::ListValue {
                        values: vec![proto_value(Kind::StringValue("value".to_owned()))],
                    })),
                ),
                (
                    "object".to_owned(),
                    proto_value(Kind::StructValue(prost_types::Struct {
                        fields: BTreeMap::from([(
                            "nested".to_owned(),
                            proto_value(Kind::BoolValue(false)),
                        )]),
                    })),
                ),
            ]),
        };
        let view = SessionView::from(SessionSummary {
            id: "session-id".to_owned(),
            metadata: Some(metadata),
            ..SessionSummary::default()
        });

        assert_eq!(view.title(), Some("Live title"));
        assert_eq!(view.display_label(), "Live title");
        assert_eq!(view.metadata["null"], serde_json::Value::Null);
        assert_eq!(view.metadata["bool"], serde_json::json!(true));
        assert_eq!(view.metadata["number"], serde_json::json!(42.0));
        assert_eq!(view.metadata["array"], serde_json::json!(["value"]));
        assert_eq!(
            view.metadata["object"],
            serde_json::json!({ "nested": false })
        );

        let mut empty_title = view;
        empty_title
            .metadata
            .insert("title".to_owned(), serde_json::json!(""));
        assert_eq!(empty_title.title(), None);
        assert_eq!(empty_title.display_label(), "session-id");

        empty_title
            .metadata
            .insert("title".to_owned(), serde_json::json!(false));
        assert_eq!(empty_title.display_label(), "session-id");
    }

    #[test]
    fn session_view_projects_known_associations_and_preserves_their_metadata() {
        let fields = BTreeMap::from([
            (
                "waltr.task.parent_session_id".to_owned(),
                proto_value(Kind::StringValue("parent-1".to_owned())),
            ),
            (
                "waltr.task.description".to_owned(),
                proto_value(Kind::StringValue("Investigate failure".to_owned())),
            ),
            (
                "waltr.routine.invocation_id".to_owned(),
                proto_value(Kind::StringValue("invocation-1".to_owned())),
            ),
        ]);

        let view = SessionView::from(SessionSummary {
            metadata: Some(prost_types::Struct { fields }),
            ..SessionSummary::default()
        });

        assert_eq!(
            view.associations,
            SessionAssociationsView {
                task_parent_session_id: Some("parent-1".to_owned()),
                task_description: Some("Investigate failure".to_owned()),
                routine_invocation_id: Some("invocation-1".to_owned()),
            }
        );
        assert_eq!(
            view.metadata["waltr.task.description"],
            serde_json::json!("Investigate failure")
        );
    }

    #[test]
    fn session_view_ignores_missing_and_non_string_associations() {
        let malformed = SessionView::from(SessionSummary {
            metadata: Some(prost_types::Struct {
                fields: BTreeMap::from([
                    (
                        "waltr.task.parent_session_id".to_owned(),
                        proto_value(Kind::NumberValue(42.0)),
                    ),
                    (
                        "waltr.task.description".to_owned(),
                        proto_value(Kind::BoolValue(true)),
                    ),
                    (
                        "waltr.routine.invocation_id".to_owned(),
                        proto_value(Kind::NullValue(0)),
                    ),
                ]),
            }),
            ..SessionSummary::default()
        });
        let missing = SessionView::from(SessionSummary::default());

        assert_eq!(malformed.associations, SessionAssociationsView::default());
        assert_eq!(missing.associations, SessionAssociationsView::default());
    }

    #[test]
    fn session_status_view_projects_suspension_kinds() {
        for (kind, expected) in [
            (
                SuspensionKind::AwaitingToolResults as i32,
                SessionSuspensionKindView::AwaitingToolResults,
            ),
            (
                SuspensionKind::AwaitingExternalInput as i32,
                SessionSuspensionKindView::AwaitingExternalInput,
            ),
            (
                SuspensionKind::Interrupted as i32,
                SessionSuspensionKindView::Interrupted,
            ),
            (
                SuspensionKind::Unspecified as i32,
                SessionSuspensionKindView::Unspecified,
            ),
            (i32::MAX, SessionSuspensionKindView::Unknown),
        ] {
            let view = SessionStatusView::from(SessionStatus {
                phase: SessionPhase::Suspended as i32,
                suspension: Some(Suspension {
                    kind,
                    ..Suspension::default()
                }),
                ..SessionStatus::default()
            });

            assert_eq!(view.suspension_kind, Some(expected));
        }
    }

    #[test]
    fn session_status_view_omits_suspension_kind_outside_suspended_phase() {
        let idle = SessionStatusView::from(SessionStatus {
            phase: SessionPhase::Idle as i32,
            suspension: Some(Suspension {
                kind: SuspensionKind::Interrupted as i32,
                ..Suspension::default()
            }),
            ..SessionStatus::default()
        });
        let suspended_without_detail = SessionStatusView::from(SessionStatus {
            phase: SessionPhase::Suspended as i32,
            ..SessionStatus::default()
        });

        assert_eq!(idle.suspension_kind, None);
        assert_eq!(suspended_without_detail.suspension_kind, None);
    }

    #[test]
    fn coordination_view_maps_every_phase_and_fallback() {
        for (phase, expected) in [
            (
                SessionCoordinationPhase::Unspecified as i32,
                SessionCoordinationPhaseView::Ready,
            ),
            (
                SessionCoordinationPhase::Ready as i32,
                SessionCoordinationPhaseView::Ready,
            ),
            (
                SessionCoordinationPhase::Running as i32,
                SessionCoordinationPhaseView::Running,
            ),
            (
                SessionCoordinationPhase::CompletionBarrier as i32,
                SessionCoordinationPhaseView::CompletionBarrier,
            ),
            (
                SessionCoordinationPhase::Paused as i32,
                SessionCoordinationPhaseView::Paused,
            ),
            (i32::MAX, SessionCoordinationPhaseView::Unknown),
        ] {
            let view = SessionCoordinationView::from(SessionCoordinationState {
                phase,
                reason: Some(SessionCoordinationReason {
                    code: "future-reason".to_owned(),
                    source: Some("routine".to_owned()),
                    trigger: Some("manual".to_owned()),
                }),
                queued_count: 3,
                continuation_armed: true,
            });
            assert_eq!(view.phase, expected);
            assert_eq!(view.reason.as_ref().unwrap().code, "future-reason");
            assert_eq!(view.queued_count, 3);
            assert!(view.continuation_armed);
        }

        assert_eq!(
            SessionView::from(SessionSummary::default())
                .coordination
                .phase,
            SessionCoordinationPhaseView::Ready
        );
    }

    #[test]
    fn progress_and_capability_views_map_all_values_and_preserve_fields() {
        for (phase, expected) in [
            (
                SessionProgressPhase::Unspecified,
                SessionProgressPhaseView::Unknown,
            ),
            (SessionProgressPhase::Idle, SessionProgressPhaseView::Idle),
            (
                SessionProgressPhase::Running,
                SessionProgressPhaseView::Running,
            ),
            (
                SessionProgressPhase::CompletionBarrier,
                SessionProgressPhaseView::CompletionBarrier,
            ),
            (
                SessionProgressPhase::Paused,
                SessionProgressPhaseView::Paused,
            ),
            (
                SessionProgressPhase::AwaitingToolResults,
                SessionProgressPhaseView::AwaitingToolResults,
            ),
            (
                SessionProgressPhase::AwaitingExternalInput,
                SessionProgressPhaseView::AwaitingExternalInput,
            ),
            (
                SessionProgressPhase::Interrupted,
                SessionProgressPhaseView::Interrupted,
            ),
            (
                SessionProgressPhase::Finished,
                SessionProgressPhaseView::Finished,
            ),
            (
                SessionProgressPhase::Failed,
                SessionProgressPhaseView::Failed,
            ),
        ] {
            assert_eq!(SessionProgressPhaseView::from(phase as i32), expected);
        }
        assert_eq!(
            SessionProgressPhaseView::from(i32::MAX),
            SessionProgressPhaseView::Unknown
        );

        for (stage, expected) in [
            (SessionRunStage::Unspecified, SessionRunStageView::Unknown),
            (
                SessionRunStage::BuildingRequest,
                SessionRunStageView::BuildingRequest,
            ),
            (
                SessionRunStage::StreamingProvider,
                SessionRunStageView::StreamingProvider,
            ),
            (
                SessionRunStage::RetryingProvider,
                SessionRunStageView::RetryingProvider,
            ),
            (
                SessionRunStage::EvaluatingToolCall,
                SessionRunStageView::EvaluatingToolCall,
            ),
            (
                SessionRunStage::RunningTool,
                SessionRunStageView::RunningTool,
            ),
        ] {
            assert_eq!(SessionRunStageView::from(stage as i32), expected);
        }
        assert_eq!(
            SessionRunStageView::from(i32::MAX),
            SessionRunStageView::Unknown
        );

        for (disposition, expected) in [
            (
                SessionPromptDisposition::Unspecified,
                SessionPromptDispositionView::Unknown,
            ),
            (
                SessionPromptDisposition::Immediate,
                SessionPromptDispositionView::Immediate,
            ),
            (
                SessionPromptDisposition::Queued,
                SessionPromptDispositionView::Queued,
            ),
            (
                SessionPromptDisposition::SupersedeSuspension,
                SessionPromptDispositionView::SupersedeSuspension,
            ),
        ] {
            assert_eq!(
                SessionPromptDispositionView::from(disposition as i32),
                expected
            );
        }
        assert_eq!(
            SessionPromptDispositionView::from(i32::MAX),
            SessionPromptDispositionView::Unknown
        );

        let view = SessionView::from(SessionSummary {
            progress: Some(SessionProgress {
                phase: SessionProgressPhase::Paused as i32,
                run_stage: Some(SessionRunStage::RunningTool as i32),
                reason: Some(SessionCoordinationReason {
                    code: "host-operation".to_owned(),
                    source: Some("routine".to_owned()),
                    trigger: Some("manual".to_owned()),
                }),
                queued_count: 3,
                continuation_armed: true,
                running: true,
            }),
            capabilities: Some(SessionCapabilities {
                prompt_disposition: SessionPromptDisposition::Queued as i32,
                can_respond_external_request: true,
                can_interrupt: true,
            }),
            ..SessionSummary::default()
        });
        assert_eq!(view.progress.phase, SessionProgressPhaseView::Paused);
        assert_eq!(
            view.progress.run_stage,
            Some(SessionRunStageView::RunningTool)
        );
        assert_eq!(
            view.progress.reason.as_ref().unwrap().code,
            "host-operation"
        );
        assert_eq!(view.progress.queued_count, 3);
        assert!(view.progress.continuation_armed);
        assert!(view.progress.running);
        assert_eq!(
            view.capabilities.prompt_disposition,
            SessionPromptDispositionView::Queued
        );
        assert!(view.capabilities.can_respond_external_request);
        assert!(view.capabilities.can_interrupt);
    }

    #[test]
    fn absent_progress_and_capabilities_are_conservative_without_status_inference() {
        let view = SessionView::from(SessionSummary {
            status: Some(SessionStatus {
                phase: SessionPhase::StreamingProvider as i32,
                ..SessionStatus::default()
            }),
            coordination: Some(SessionCoordinationState {
                phase: SessionCoordinationPhase::Running as i32,
                ..SessionCoordinationState::default()
            }),
            ..SessionSummary::default()
        });

        assert_eq!(view.progress, SessionProgressView::default());
        assert_eq!(view.progress.phase, SessionProgressPhaseView::Unknown);
        assert!(!view.progress.running);
        assert_eq!(view.capabilities, SessionCapabilitiesView::default());
        assert_eq!(
            view.capabilities.prompt_disposition,
            SessionPromptDispositionView::Unknown
        );
        assert!(!view.capabilities.can_respond_external_request);
        assert!(!view.capabilities.can_interrupt);
    }

    #[test]
    fn synthetic_projection_has_distinct_identity_and_no_durable_action_event() {
        let entry = ProjectedTimelineEntryView::try_from(ProjectedTimelineEvent {
            presentation: TimelinePresentation::MaskReplacement as i32,
            mask_id: Some("mask-42".to_owned()),
            event: Some(projected_timeline_event::Event::Synthetic(
                SyntheticTimelineEvent {
                    cursor: 9,
                    session_id: "session".to_owned(),
                    turn: 2,
                    channel: "user_text".to_owned(),
                    ..SyntheticTimelineEvent::default()
                },
            )),
        })
        .expect("valid synthetic projection");

        assert!(entry.durable_event().is_none());
        assert_eq!(entry.ordering_cursor(), 9);
        assert!(matches!(
            entry,
            ProjectedTimelineEntryView::Synthetic {
                identity,
                presentation: TimelinePresentationView::MaskReplacement,
                ..
            } if identity == "mask:mask-42"
        ));
        assert_eq!(
            TimelinePresentationView::from(i32::MAX),
            TimelinePresentationView::Unknown
        );
    }

    fn proto_value(kind: Kind) -> prost_types::Value {
        prost_types::Value { kind: Some(kind) }
    }
}
