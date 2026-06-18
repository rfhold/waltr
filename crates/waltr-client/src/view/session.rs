use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{SessionPhase, SessionStatus, SessionSummary};

/// Stable session summary view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    /// Server-assigned session id.
    pub id: String,
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
}

/// Stable session status view shared by Waltr shells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusView {
    /// Session phase as a shell-friendly enum.
    pub phase: SessionPhaseView,
    /// Failure message, when phase is failed.
    pub failed_message: String,
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

impl From<SessionSummary> for SessionView {
    fn from(session: SessionSummary) -> Self {
        Self {
            id: session.id,
            revision: session.revision,
            status: session
                .status
                .map_or_else(SessionStatusView::default, Into::into),
            current_prompt: session.current_prompt,
            created_at: session.created_at.map(Into::into),
            updated_at: session.updated_at.map(Into::into),
            last_activity_at: session.last_activity_at.map(Into::into),
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
        }
    }
}

impl From<SessionStatus> for SessionStatusView {
    fn from(status: SessionStatus) -> Self {
        Self {
            phase: status.phase.into(),
            failed_message: status.failed_message,
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
            Ok(SessionPhase::EvaluatingToolCall) => Self::EvaluatingToolCall,
            Ok(SessionPhase::RunningTool) => Self::RunningTool,
            Ok(SessionPhase::Suspended) => Self::Suspended,
            Ok(SessionPhase::Finished) => Self::Finished,
            Ok(SessionPhase::Failed) => Self::Failed,
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
