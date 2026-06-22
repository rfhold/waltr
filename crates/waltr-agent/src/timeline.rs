//! Public agent timeline model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ToolCall, ToolResult,
    agent::session::{
        ExternalRequest, ExternalRequestStatus, PendingWorkStatus, SessionId, SessionStatus,
        Suspension,
    },
};

/// Assistant text display channel.
pub const CHANNEL_ASSISTANT_TEXT: &str = "assistant_text";
/// Provider thinking/reasoning display channel.
pub const CHANNEL_THINKING: &str = "thinking";
/// Tool call preview display channel.
pub const CHANNEL_TOOL_CALL: &str = "tool_call";
/// Informational tool progress display channel.
pub const CHANNEL_TOOL_PROGRESS_INFO: &str = "tool_progress_info";
/// Error tool progress display channel.
pub const CHANNEL_TOOL_PROGRESS_ERROR: &str = "tool_progress_error";
/// Tool result display channel.
pub const CHANNEL_TOOL_RESULT: &str = "tool_result";
/// External request display channel.
pub const CHANNEL_EXTERNAL_REQUEST: &str = "external_request";

/// Monotonic timeline cursor within one session.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct TimelineCursor(pub u64);

impl TimelineCursor {
    /// Initial cursor value for a session timeline.
    pub const ZERO: Self = Self(0);

    /// Create a cursor from its raw value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the raw cursor value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the next cursor in the same session.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl From<u64> for TimelineCursor {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

/// Stable timeline item identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimelineItemId(pub String);

impl TimelineItemId {
    /// Create a timeline item identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TimelineItemId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for TimelineItemId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Stable identifier for referenced timeline content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimelineContentId(pub String);

impl TimelineContentId {
    /// Create a timeline content identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TimelineContentId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for TimelineContentId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// String-backed timeline display channel.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimelineChannel(pub String);

impl TimelineChannel {
    /// Create a timeline channel.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the channel string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TimelineChannel {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for TimelineChannel {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Stable lifecycle kind for a timeline event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimelineEventKind {
    /// A timeline item became visible.
    ItemStarted,
    /// A timeline item received incremental content.
    ContentDelta,
    /// A timeline item snapshot or metadata changed.
    ItemUpdated,
    /// A timeline item reached a terminal state.
    ItemFinished,
    /// An external request opened and is awaiting host input.
    ExternalRequestOpened,
    /// An external request resolved or was canceled.
    ExternalRequestResolved,
    /// Session runtime status changed.
    SessionStatusChanged,
}

/// Direction for bounded timeline history reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimelineDirection {
    /// Read events after a cursor, or from the start when no cursor is supplied.
    Forward,
    /// Read events before a cursor, or from the end when no cursor is supplied.
    Backward,
}

/// Store-neutral timeline range query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineQuery {
    /// Read direction.
    pub direction: TimelineDirection,
    /// Exclusive cursor boundary.
    pub cursor: Option<TimelineCursor>,
    /// Maximum number of events to return.
    pub limit: usize,
    /// Optional display-channel filter.
    pub channels: Option<Vec<TimelineChannel>>,
}

impl TimelineQuery {
    /// Create a forward query after `cursor`, or from the start when no cursor is supplied.
    #[must_use]
    pub const fn forward(cursor: Option<TimelineCursor>, limit: usize) -> Self {
        Self {
            direction: TimelineDirection::Forward,
            cursor,
            limit,
            channels: None,
        }
    }

    /// Create a backward query before `cursor`, or from the end when no cursor is supplied.
    #[must_use]
    pub const fn backward(cursor: Option<TimelineCursor>, limit: usize) -> Self {
        Self {
            direction: TimelineDirection::Backward,
            cursor,
            limit,
            channels: None,
        }
    }

    /// Limit the query to the supplied display channels.
    #[must_use]
    pub fn with_channels(mut self, channels: impl IntoIterator<Item = TimelineChannel>) -> Self {
        self.channels = Some(channels.into_iter().collect());
        self
    }
}

/// Reconstructed text content for one timeline item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineItemText {
    /// Timeline item identity.
    pub item_id: TimelineItemId,
    /// Reconstructed UTF-8 text.
    pub text: String,
    /// Reconstructed text size in bytes.
    pub bytes: u64,
    /// Last cursor observed for this item.
    pub last_cursor: Option<TimelineCursor>,
}

/// Coarse status attached to timeline items or session-status events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimelineStatus {
    /// Session is ready for new input.
    Idle,
    /// A provider request is being assembled.
    BuildingRequest,
    /// Provider streaming is in progress.
    StreamingProvider,
    /// Tool call middleware is being evaluated.
    EvaluatingToolCall,
    /// A local tool executor is running.
    RunningTool,
    /// Work is waiting to start or complete.
    Pending,
    /// Runtime is waiting for tool results.
    AwaitingToolResults,
    /// Runtime is waiting for external input.
    AwaitingExternalInput,
    /// Runtime was interrupted while work was active.
    Interrupted,
    /// External input was resolved.
    Resolved,
    /// Work or the session completed normally.
    Finished,
    /// Work, an item, or the session failed.
    Failed,
    /// Work or external input was canceled.
    Canceled,
}

impl From<&SessionStatus> for TimelineStatus {
    fn from(status: &SessionStatus) -> Self {
        match status {
            SessionStatus::Idle => Self::Idle,
            SessionStatus::BuildingRequest => Self::BuildingRequest,
            SessionStatus::StreamingProvider => Self::StreamingProvider,
            SessionStatus::EvaluatingToolCall => Self::EvaluatingToolCall,
            SessionStatus::RunningTool => Self::RunningTool,
            SessionStatus::Suspended(Suspension::AwaitingToolResults { .. }) => {
                Self::AwaitingToolResults
            }
            SessionStatus::Suspended(Suspension::AwaitingExternalInput { .. }) => {
                Self::AwaitingExternalInput
            }
            SessionStatus::Suspended(Suspension::Interrupted { .. }) => Self::Interrupted,
            SessionStatus::Finished => Self::Finished,
            SessionStatus::Failed { .. } => Self::Failed,
        }
    }
}

impl From<&PendingWorkStatus> for TimelineStatus {
    fn from(status: &PendingWorkStatus) -> Self {
        match status {
            PendingWorkStatus::Pending => Self::Pending,
            PendingWorkStatus::Canceled(_) => Self::Canceled,
            PendingWorkStatus::Finished(_) => Self::Finished,
            PendingWorkStatus::Failed { .. } => Self::Failed,
        }
    }
}

impl From<&ExternalRequestStatus> for TimelineStatus {
    fn from(status: &ExternalRequestStatus) -> Self {
        match status {
            ExternalRequestStatus::Pending => Self::Pending,
            ExternalRequestStatus::Responded(_) => Self::Resolved,
            ExternalRequestStatus::Canceled(_) => Self::Canceled,
        }
    }
}

/// Content carried by a timeline event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimelineContent {
    /// No content payload.
    Empty,
    /// Incremental text content.
    TextDelta {
        /// Text delta.
        delta: String,
    },
    /// Full text snapshot.
    TextSnapshot {
        /// Complete text.
        text: String,
    },
    /// Provider-neutral tool call.
    ToolCall {
        /// Tool call payload.
        call: ToolCall,
    },
    /// Provider-neutral tool result.
    ToolResult {
        /// Tool result payload.
        result: ToolResult,
    },
    /// Durable external request payload.
    ExternalRequest {
        /// External request payload.
        request: ExternalRequest,
    },
    /// Reference to content stored out-of-line.
    ContentRef {
        /// Referenced content identifier.
        content_id: TimelineContentId,
        /// Full content size in bytes.
        bytes: u64,
    },
    /// Provider-neutral JSON payload.
    Json {
        /// JSON payload.
        value: Value,
    },
}

impl TimelineContent {
    /// Create a text delta payload.
    pub fn text_delta(delta: impl Into<String>) -> Self {
        Self::TextDelta {
            delta: delta.into(),
        }
    }

    /// Create a content reference payload.
    pub fn content_ref(content_id: impl Into<TimelineContentId>, bytes: u64) -> Self {
        Self::ContentRef {
            content_id: content_id.into(),
            bytes,
        }
    }
}

/// Timeline event before durable cursor assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTimelineEvent {
    /// Session that owns this event.
    pub session_id: SessionId,
    /// Provider turn associated with this event.
    pub turn: u64,
    /// Timeline item identity.
    pub item_id: TimelineItemId,
    /// Parent item identity for child events.
    pub parent_id: Option<TimelineItemId>,
    /// Event lifecycle kind.
    pub kind: TimelineEventKind,
    /// Display channel.
    pub channel: Option<TimelineChannel>,
    /// Event content.
    pub content: TimelineContent,
    /// Optional item or session status.
    pub status: Option<TimelineStatus>,
    /// Feature-specific metadata.
    pub metadata: Value,
}

impl PendingTimelineEvent {
    /// Create a pending timeline event.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        kind: TimelineEventKind,
        channel: Option<TimelineChannel>,
        content: TimelineContent,
    ) -> Self {
        Self {
            session_id,
            turn,
            item_id: item_id.into(),
            parent_id: None,
            kind,
            channel,
            content,
            status: None,
            metadata: empty_metadata(),
        }
    }

    /// Create an assistant text delta event.
    #[must_use]
    pub fn assistant_text_delta(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        delta: impl Into<String>,
    ) -> Self {
        text_delta_event(session_id, turn, item_id, CHANNEL_ASSISTANT_TEXT, delta)
    }

    /// Create a thinking delta event.
    #[must_use]
    pub fn thinking_delta(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        delta: impl Into<String>,
    ) -> Self {
        text_delta_event(session_id, turn, item_id, CHANNEL_THINKING, delta)
    }

    /// Create an informational tool progress delta event.
    #[must_use]
    pub fn tool_progress_info(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        parent_id: impl Into<TimelineItemId>,
        delta: impl Into<String>,
    ) -> Self {
        let mut event =
            text_delta_event(session_id, turn, item_id, CHANNEL_TOOL_PROGRESS_INFO, delta);
        event.parent_id = Some(parent_id.into());
        event
    }

    /// Create an error tool progress delta event.
    #[must_use]
    pub fn tool_progress_error(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        parent_id: impl Into<TimelineItemId>,
        delta: impl Into<String>,
    ) -> Self {
        let mut event = text_delta_event(
            session_id,
            turn,
            item_id,
            CHANNEL_TOOL_PROGRESS_ERROR,
            delta,
        );
        event.parent_id = Some(parent_id.into());
        event
    }

    /// Create a tool call preview event.
    #[must_use]
    pub fn tool_call(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        call: ToolCall,
    ) -> Self {
        let mut event = Self::new(
            session_id,
            turn,
            item_id,
            TimelineEventKind::ItemStarted,
            Some(TimelineChannel::from(CHANNEL_TOOL_CALL)),
            TimelineContent::ToolCall { call },
        );
        event.status = Some(TimelineStatus::Pending);
        event
    }

    /// Create a tool result event.
    #[must_use]
    pub fn tool_result(
        session_id: SessionId,
        turn: u64,
        item_id: impl Into<TimelineItemId>,
        parent_id: impl Into<TimelineItemId>,
        result: ToolResult,
    ) -> Self {
        let mut event = Self::new(
            session_id,
            turn,
            item_id,
            TimelineEventKind::ItemFinished,
            Some(TimelineChannel::from(CHANNEL_TOOL_RESULT)),
            TimelineContent::ToolResult { result },
        );
        event.parent_id = Some(parent_id.into());
        event.status = Some(TimelineStatus::Finished);
        event
    }

    /// Assign a durable session cursor to this pending event.
    #[must_use]
    pub fn with_cursor(self, cursor: TimelineCursor) -> AgentTimelineEvent {
        AgentTimelineEvent {
            cursor,
            session_id: self.session_id,
            turn: self.turn,
            item_id: self.item_id,
            parent_id: self.parent_id,
            kind: self.kind,
            channel: self.channel,
            content: self.content,
            status: self.status,
            metadata: self.metadata,
        }
    }
}

/// Timeline event after durable cursor assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTimelineEvent {
    /// Stable per-session cursor assigned at append time.
    pub cursor: TimelineCursor,
    /// Session that owns this event.
    pub session_id: SessionId,
    /// Provider turn associated with this event.
    pub turn: u64,
    /// Timeline item identity.
    pub item_id: TimelineItemId,
    /// Parent item identity for child events.
    pub parent_id: Option<TimelineItemId>,
    /// Event lifecycle kind.
    pub kind: TimelineEventKind,
    /// Display channel.
    pub channel: Option<TimelineChannel>,
    /// Event content.
    pub content: TimelineContent,
    /// Optional item or session status.
    pub status: Option<TimelineStatus>,
    /// Feature-specific metadata.
    pub metadata: Value,
}

impl AgentTimelineEvent {
    /// Create a cursor-assigned event from a pending event.
    #[must_use]
    pub fn from_pending(cursor: TimelineCursor, pending: PendingTimelineEvent) -> Self {
        pending.with_cursor(cursor)
    }
}

fn text_delta_event(
    session_id: SessionId,
    turn: u64,
    item_id: impl Into<TimelineItemId>,
    channel: &'static str,
    delta: impl Into<String>,
) -> PendingTimelineEvent {
    PendingTimelineEvent::new(
        session_id,
        turn,
        item_id,
        TimelineEventKind::ContentDelta,
        Some(TimelineChannel::from(channel)),
        TimelineContent::text_delta(delta),
    )
}

fn empty_metadata() -> Value {
    Value::Object(serde_json::Map::default())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::session::{CancelReason, ExternalResponse};

    #[test]
    fn channel_constants_are_documented_strings() {
        let constants = [
            CHANNEL_ASSISTANT_TEXT,
            CHANNEL_THINKING,
            CHANNEL_TOOL_CALL,
            CHANNEL_TOOL_PROGRESS_INFO,
            CHANNEL_TOOL_PROGRESS_ERROR,
            CHANNEL_TOOL_RESULT,
            CHANNEL_EXTERNAL_REQUEST,
        ];

        assert_eq!(constants.len(), 7);
        assert_eq!(CHANNEL_ASSISTANT_TEXT, "assistant_text");
        assert_eq!(CHANNEL_THINKING, "thinking");
        assert_eq!(CHANNEL_TOOL_CALL, "tool_call");
        assert_eq!(CHANNEL_TOOL_PROGRESS_INFO, "tool_progress_info");
        assert_eq!(CHANNEL_TOOL_PROGRESS_ERROR, "tool_progress_error");
        assert_eq!(CHANNEL_TOOL_RESULT, "tool_result");
        assert_eq!(CHANNEL_EXTERNAL_REQUEST, "external_request");
        assert_eq!(
            TimelineChannel::from("custom_status").as_str(),
            "custom_status"
        );
    }

    #[test]
    fn cursors_order_within_a_session() {
        let first = TimelineCursor::ZERO.next();
        let second = first.next();

        assert_eq!(TimelineCursor::new(42).get(), 42);
        assert!(TimelineCursor::ZERO < first);
        assert!(first < second);
        assert_eq!(second.get(), 2);
    }

    #[test]
    fn content_ref_records_identity_and_size() {
        let content = TimelineContent::content_ref("blob-1", 1024);

        assert_eq!(
            content,
            TimelineContent::ContentRef {
                content_id: TimelineContentId::from("blob-1"),
                bytes: 1024,
            }
        );
    }

    #[test]
    fn helper_constructors_set_common_channels_and_content() {
        let session_id = SessionId::new("session-1");
        let assistant = PendingTimelineEvent::assistant_text_delta(
            session_id.clone(),
            3,
            "assistant-turn-3",
            "hello",
        );
        let progress = PendingTimelineEvent::tool_progress_error(
            session_id.clone(),
            3,
            "progress-1",
            "tool-call-1",
            "stderr",
        );
        let call = ToolCall::new("call-1", "read", json!({ "path": "README.md" }));
        let tool_call =
            PendingTimelineEvent::tool_call(session_id.clone(), 3, "tool-call-1", call.clone());
        let result = ToolResult::new("call-1", "read", "contents");
        let tool_result = PendingTimelineEvent::tool_result(
            session_id,
            3,
            "tool-result-1",
            "tool-call-1",
            result.clone(),
        );

        assert_eq!(assistant.kind, TimelineEventKind::ContentDelta);
        assert_eq!(
            assistant.channel,
            Some(TimelineChannel::from(CHANNEL_ASSISTANT_TEXT))
        );
        assert_eq!(assistant.content, TimelineContent::text_delta("hello"));
        assert_eq!(assistant.metadata, json!({}));

        assert_eq!(
            progress.channel,
            Some(TimelineChannel::from(CHANNEL_TOOL_PROGRESS_ERROR))
        );
        assert_eq!(
            progress.parent_id,
            Some(TimelineItemId::from("tool-call-1"))
        );

        assert_eq!(tool_call.kind, TimelineEventKind::ItemStarted);
        assert_eq!(tool_call.status, Some(TimelineStatus::Pending));
        assert_eq!(tool_call.content, TimelineContent::ToolCall { call });

        assert_eq!(tool_result.kind, TimelineEventKind::ItemFinished);
        assert_eq!(
            tool_result.parent_id,
            Some(TimelineItemId::from("tool-call-1"))
        );
        assert_eq!(tool_result.status, Some(TimelineStatus::Finished));
        assert_eq!(tool_result.content, TimelineContent::ToolResult { result });
    }

    #[test]
    fn assigning_cursor_preserves_pending_fields() {
        let pending = PendingTimelineEvent::thinking_delta(
            SessionId::new("session-1"),
            1,
            "thinking-1",
            "considering",
        );
        let event = pending.clone().with_cursor(TimelineCursor::new(9));

        assert_eq!(event.cursor, TimelineCursor::new(9));
        assert_eq!(event.session_id, pending.session_id);
        assert_eq!(event.turn, pending.turn);
        assert_eq!(event.item_id, pending.item_id);
        assert_eq!(event.kind, pending.kind);
        assert_eq!(event.channel, pending.channel);
        assert_eq!(event.content, pending.content);
        assert_eq!(event.status, pending.status);
        assert_eq!(event.metadata, pending.metadata);
    }

    #[test]
    fn statuses_cover_current_session_and_work_statuses() {
        assert_eq!(
            TimelineStatus::from(&SessionStatus::BuildingRequest),
            TimelineStatus::BuildingRequest
        );
        assert_eq!(
            TimelineStatus::from(&SessionStatus::Suspended(Suspension::AwaitingToolResults {
                work: Vec::new()
            })),
            TimelineStatus::AwaitingToolResults
        );
        assert_eq!(
            TimelineStatus::from(&PendingWorkStatus::Canceled(
                CancelReason::SessionInterrupted
            )),
            TimelineStatus::Canceled
        );
        assert_eq!(
            TimelineStatus::from(&ExternalRequestStatus::Responded(ExternalResponse::new(
                json!({ "answer": true })
            ))),
            TimelineStatus::Resolved
        );
    }

    #[test]
    fn timeline_event_serializes_with_cursor_only_after_assignment() {
        let pending = PendingTimelineEvent::assistant_text_delta(
            SessionId::new("session-1"),
            0,
            "assistant-turn-0",
            "hi",
        );
        let pending_value = serde_json::to_value(&pending).expect("serialize pending event");
        let event_value = serde_json::to_value(AgentTimelineEvent::from_pending(
            TimelineCursor::new(1),
            pending,
        ))
        .expect("serialize assigned event");

        assert!(pending_value.get("cursor").is_none());
        assert_eq!(event_value.get("cursor"), Some(&json!(1)));
    }
}
