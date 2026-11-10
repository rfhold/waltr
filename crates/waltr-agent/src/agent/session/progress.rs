//! Derived session progress and capability projection.

use serde::{Deserialize, Serialize};

use super::{
    SessionCoordinationPhase, SessionCoordinationReason, SessionCoordinationState, SessionStatus,
    Suspension,
};

/// User-facing session lifecycle phase derived from durable and process-local state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionProgressPhase {
    /// No recognized lifecycle state is available.
    #[default]
    Unknown,
    /// Session is ready and has no active durable work.
    Idle,
    /// Session is executing or has durable active work.
    Running,
    /// A completed run is undergoing host admission checks.
    CompletionBarrier,
    /// A process-local host operation owns the session.
    Paused,
    /// Durable work is waiting for tool results.
    AwaitingToolResults,
    /// Durable work was interrupted.
    Interrupted,
    /// Session completed normally.
    Finished,
    /// Session failed.
    Failed,
}

/// Detailed durable stage for active session work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunStage {
    /// A provider request is being assembled.
    BuildingRequest,
    /// Provider streaming is in progress.
    StreamingProvider,
    /// Provider retry backoff is in progress.
    RetryingProvider,
    /// Tool call middleware is being evaluated.
    EvaluatingToolCall,
    /// A local tool executor is running.
    RunningTool,
}

/// Unified session progress derived from durable status and process-local coordination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProgress {
    /// Current lifecycle phase.
    pub phase: SessionProgressPhase,
    /// Detailed durable active stage, when present.
    pub run_stage: Option<SessionRunStage>,
    /// Machine-readable completion-barrier or pause reason, when present.
    pub reason: Option<SessionCoordinationReason>,
    /// Logical submitted runs queued behind current ownership.
    pub queued_count: usize,
    /// Whether a host continuation is armed ahead of queued user work.
    pub continuation_armed: bool,
    /// MCP-compatible activity indicator.
    pub running: bool,
}

/// How a new prompt will be handled by the authoritative session state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionPromptDisposition {
    /// Prompt can start immediately.
    #[default]
    Immediate,
    /// Prompt will queue behind process-local ownership.
    Queued,
    /// Prompt will supersede the current durable suspension.
    SupersedeSuspension,
}

/// Authoritative actions currently available for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCapabilities {
    /// How a submitted prompt will be handled.
    pub prompt_disposition: SessionPromptDisposition,
    /// Whether a real process-local active run can be interrupted.
    pub can_interrupt: bool,
}

impl SessionProgress {
    /// Derive progress from exact durable and process-local state.
    #[must_use]
    pub fn from_state(status: &SessionStatus, coordination: &SessionCoordinationState) -> Self {
        let run_stage = run_stage(status);
        let phase = match (status, coordination.phase) {
            (SessionStatus::Failed { .. }, _) => SessionProgressPhase::Failed,
            (_, SessionCoordinationPhase::Paused) => SessionProgressPhase::Paused,
            (_, SessionCoordinationPhase::CompletionBarrier) => {
                SessionProgressPhase::CompletionBarrier
            }
            (SessionStatus::Suspended(Suspension::AwaitingToolResults { .. }), _) => {
                SessionProgressPhase::AwaitingToolResults
            }
            (SessionStatus::Suspended(Suspension::Interrupted { .. }), _) => {
                SessionProgressPhase::Interrupted
            }
            (_, SessionCoordinationPhase::Running) => SessionProgressPhase::Running,
            (
                SessionStatus::BuildingRequest
                | SessionStatus::StreamingProvider
                | SessionStatus::RetryingProvider { .. }
                | SessionStatus::EvaluatingToolCall
                | SessionStatus::RunningTool,
                SessionCoordinationPhase::Ready,
            ) => SessionProgressPhase::Running,
            (SessionStatus::Finished, SessionCoordinationPhase::Ready) => {
                SessionProgressPhase::Finished
            }
            (SessionStatus::Idle, SessionCoordinationPhase::Ready) => SessionProgressPhase::Idle,
        };
        Self {
            phase,
            run_stage,
            reason: coordination.reason.clone(),
            queued_count: coordination.queued_count,
            continuation_armed: coordination.continuation_armed,
            running: match coordination.phase {
                SessionCoordinationPhase::Paused | SessionCoordinationPhase::CompletionBarrier => {
                    true
                }
                SessionCoordinationPhase::Running => !matches!(status, SessionStatus::Suspended(_)),
                SessionCoordinationPhase::Ready => run_stage.is_some(),
            },
        }
    }
}

impl SessionCapabilities {
    /// Derive capabilities from exact durable and process-local state.
    #[must_use]
    pub fn from_state(status: &SessionStatus, coordination: &SessionCoordinationState) -> Self {
        let ready = matches!(coordination.phase, SessionCoordinationPhase::Ready);
        Self {
            prompt_disposition: if ready {
                if matches!(status, SessionStatus::Suspended(_)) {
                    SessionPromptDisposition::SupersedeSuspension
                } else {
                    SessionPromptDisposition::Immediate
                }
            } else {
                SessionPromptDisposition::Queued
            },
            can_interrupt: matches!(coordination.phase, SessionCoordinationPhase::Running),
        }
    }
}

fn run_stage(status: &SessionStatus) -> Option<SessionRunStage> {
    match status {
        SessionStatus::BuildingRequest => Some(SessionRunStage::BuildingRequest),
        SessionStatus::StreamingProvider => Some(SessionRunStage::StreamingProvider),
        SessionStatus::RetryingProvider { .. } => Some(SessionRunStage::RetryingProvider),
        SessionStatus::EvaluatingToolCall => Some(SessionRunStage::EvaluatingToolCall),
        SessionStatus::RunningTool => Some(SessionRunStage::RunningTool),
        SessionStatus::Idle
        | SessionStatus::Suspended(_)
        | SessionStatus::Finished
        | SessionStatus::Failed { .. } => None,
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn durable_active_stage_after_restart_is_running_but_not_interruptible() {
        let coordination = SessionCoordinationState::default();
        let progress =
            SessionProgress::from_state(&SessionStatus::StreamingProvider, &coordination);
        let capabilities =
            SessionCapabilities::from_state(&SessionStatus::StreamingProvider, &coordination);

        assert_eq!(progress.phase, SessionProgressPhase::Running);
        assert_eq!(progress.run_stage, Some(SessionRunStage::StreamingProvider));
        assert!(progress.running);
        assert!(!capabilities.can_interrupt);
        assert_eq!(
            capabilities.prompt_disposition,
            SessionPromptDisposition::Immediate
        );
    }
}
