use serde::{Deserialize, Serialize};
use ts_rs::TS;

use waltr::grpc::waltr::session::v1::timeline_content;

use crate::{
    AuthenticationFailureSnapshot, InferenceProviderSettings, InferenceSettings,
    InferenceSettingsMatch, OutboundAuthenticationFailure, ProviderKindView, SessionMetrics,
    SessionPhase, SessionStatus, SessionSummary, TimelineContent, TimelineEvent, TimelineEventKind,
    TimelineStatus,
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
    /// Failure message, when phase is failed.
    pub failed_message: String,
    /// Structured outbound authentication failure, when present.
    pub authentication_failure: Option<AuthenticationFailureSnapshot>,
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
    /// Persisted timeline events ordered by cursor.
    pub events: Vec<TimelineEventView>,
    /// Timeline page load operation snapshot.
    pub load: crate::OperationSnapshot,
    /// Timeline watch operation snapshot.
    pub watch: crate::OperationSnapshot,
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
        Self {
            id: session.id,
            agent_name: session.agent_name,
            revision: session.revision,
            status: session
                .status
                .map_or_else(SessionStatusView::default, Into::into),
            current_prompt: session.current_prompt,
            created_at: session.created_at.map(Into::into),
            updated_at: session.updated_at.map(Into::into),
            last_activity_at: session.last_activity_at.map(Into::into),
            inference_settings: session.inference_settings.map(Into::into),
            metrics: session
                .metrics
                .map_or_else(SessionMetricsView::default, Into::into),
        }
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
            failed_message: String::new(),
            authentication_failure: None,
        }
    }
}

impl From<SessionStatus> for SessionStatusView {
    fn from(status: SessionStatus) -> Self {
        let authentication_failure = (status.phase == SessionPhase::Failed as i32)
            .then_some(status.authentication_failure)
            .flatten()
            .and_then(OutboundAuthenticationFailure::from_proto)
            .map(Into::into);
        Self {
            phase: status.phase.into(),
            failed_message: status.failed_message,
            authentication_failure,
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
