//! Durable session run contracts.

use std::{error::Error, fmt};

use tokio::sync::oneshot;

use crate::{
    AuthTarget, AuthenticationFailureReason, LlmError, McpAuthorizationContext, ToolResult,
};

use super::{ExternalRequestId, ExternalResponse, SessionId, SessionStatus};

#[allow(clippy::redundant_pub_crate)]
pub(crate) type RunResult = std::result::Result<AgentSessionRunOutcome, AgentSessionRunError>;
#[allow(clippy::redundant_pub_crate)]
pub(crate) type RunOutcomeSender = oneshot::Sender<RunResult>;

/// Error returned while resolving a session runtime.
#[derive(Debug)]
pub enum AgentSessionRuntimeError {
    /// The session is missing required runtime configuration.
    FailedPrecondition(String),
    /// Runtime resolution failed due to an internal host error.
    Internal(String),
    /// Runtime resolution failed with a provider-neutral agent error.
    Agent(LlmError),
    /// Runtime resolution found a target that needs authentication.
    AuthorizationRequired {
        /// Target that requires authentication.
        target: AuthTarget,
        /// Human-readable reason.
        message: String,
        /// Machine-readable reason for the failure.
        reason: AuthenticationFailureReason,
        /// Sanitized MCP challenge context, when the target is an MCP server.
        mcp_authorization: Option<Box<McpAuthorizationContext>>,
    },
}

impl fmt::Display for AgentSessionRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FailedPrecondition(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::AuthorizationRequired {
                target, message, ..
            } => {
                write!(
                    formatter,
                    "authentication required for {target:?}: {message}"
                )
            }
        }
    }
}

impl Error for AgentSessionRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::FailedPrecondition(_)
            | Self::Internal(_)
            | Self::AuthorizationRequired { .. } => None,
        }
    }
}

impl From<LlmError> for AgentSessionRuntimeError {
    fn from(error: LlmError) -> Self {
        Self::Agent(error)
    }
}

/// Policy for submitting work to a session that already has an active run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunConflictPolicy {
    /// Cancel the active run and start the submitted run immediately.
    ReplaceActive,
    /// Reject the submitted run while another run is active.
    RejectIfActive,
    /// Queue the submitted run until active work for the session finishes.
    QueueIfActive,
}

/// Input that can start or resume a durable session run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionRunInput {
    /// Continue from the existing durable checkpoint without adding input.
    Continue,
    /// Start or resume with a new user prompt.
    Prompt(String),
    /// Start or resume with multiple adjacent user prompts.
    Prompts(Vec<String>),
    /// Resume with an answer to a pending external request.
    ExternalResponse {
        /// Pending external request id.
        request_id: ExternalRequestId,
        /// Provider-neutral response payload.
        response: ExternalResponse,
    },
    /// Resume with externally completed tool results.
    ToolResults(Vec<ToolResult>),
}

impl AgentSessionRunInput {
    pub(super) const fn kind(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Prompt(_) | Self::Prompts(_) => "prompt",
            Self::ExternalResponse { .. } => "external_response",
            Self::ToolResults(_) => "tool_results",
        }
    }
}

/// How a submitted run was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionRunDisposition {
    /// The run started immediately.
    Started,
    /// A previously active run was canceled and replaced.
    ReplacedActive,
    /// The run was queued behind an active run.
    Queued {
        /// One-based position in the session queue.
        position: usize,
    },
}

/// Result of a submitted session run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRunOutcome {
    /// Session that ran.
    pub session_id: SessionId,
    /// Supervisor-assigned run id.
    pub run_id: u64,
    /// Durable status after the run stream ended.
    pub status: SessionStatus,
    /// Assistant text emitted by this run, when any was streamed.
    pub assistant_text: Option<String>,
}

/// Current active-run state for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionActiveRunState {
    /// Active supervisor-assigned run id.
    pub run_id: u64,
    /// Number of inputs queued behind the active run.
    pub queued_count: usize,
}

/// A submitted session run and its completion handle.
#[derive(Debug)]
pub struct AgentSessionRunSubmission {
    /// Session receiving the run.
    pub session_id: SessionId,
    /// Supervisor-assigned run id.
    pub run_id: u64,
    /// How the coordinator handled the submission.
    pub disposition: AgentSessionRunDisposition,
    /// Completion handle for the submitted run.
    pub handle: AgentSessionRunHandle,
}

/// Completion handle for a submitted session run.
#[derive(Debug)]
pub struct AgentSessionRunHandle {
    session_id: SessionId,
    run_id: u64,
    outcome_rx: oneshot::Receiver<RunResult>,
}

impl AgentSessionRunHandle {
    pub(super) const fn new(
        session_id: SessionId,
        run_id: u64,
        outcome_rx: oneshot::Receiver<RunResult>,
    ) -> Self {
        Self {
            session_id,
            run_id,
            outcome_rx,
        }
    }

    /// Return the session id this handle waits on.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Return the supervisor-assigned run id.
    #[must_use]
    pub const fn run_id(&self) -> u64 {
        self.run_id
    }

    /// Wait for the submitted run to finish.
    ///
    /// # Errors
    ///
    /// Returns an error when the run fails or the supervisor task exits without
    /// reporting an outcome.
    pub async fn wait(self) -> std::result::Result<AgentSessionRunOutcome, AgentSessionRunError> {
        self.outcome_rx
            .await
            .map_err(|_| AgentSessionRunError::OutcomeDropped {
                session_id: self.session_id,
                run_id: self.run_id,
            })?
    }
}

/// Error returned by active session run supervision.
#[derive(Debug)]
pub enum AgentSessionRunError {
    /// The session already has an active run and the selected policy rejects it.
    AlreadyRunning {
        /// Session that already had active work.
        session_id: SessionId,
    },
    /// A queued run was canceled before it started.
    Canceled {
        /// Session that owned the canceled run.
        session_id: SessionId,
        /// Supervisor-assigned run id.
        run_id: u64,
        /// Human-readable cancellation reason.
        reason: String,
    },
    /// The run task ended without sending a completion outcome.
    OutcomeDropped {
        /// Session whose run outcome was dropped.
        session_id: SessionId,
        /// Supervisor-assigned run id.
        run_id: u64,
    },
    /// Agent execution or session storage failed.
    Agent(LlmError),
    /// The runtime for the session could not be resolved.
    Runtime(AgentSessionRuntimeError),
}

impl AgentSessionRunError {
    pub(super) fn canceled(session_id: SessionId, run_id: u64, reason: impl Into<String>) -> Self {
        Self::Canceled {
            session_id,
            run_id,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for AgentSessionRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning { session_id } => write!(
                formatter,
                "session already has an active run: {}",
                session_id.as_str()
            ),
            Self::Canceled {
                session_id,
                run_id,
                reason,
            } => write!(
                formatter,
                "session run {run_id} for {} was canceled: {reason}",
                session_id.as_str()
            ),
            Self::OutcomeDropped { session_id, run_id } => write!(
                formatter,
                "session run {run_id} for {} ended without an outcome",
                session_id.as_str()
            ),
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::Runtime(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for AgentSessionRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::AlreadyRunning { .. } | Self::Canceled { .. } | Self::OutcomeDropped { .. } => {
                None
            }
        }
    }
}

impl From<AgentSessionRuntimeError> for AgentSessionRunError {
    fn from(error: AgentSessionRuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<LlmError> for AgentSessionRunError {
    fn from(error: LlmError) -> Self {
        Self::Agent(error)
    }
}

impl From<AgentSessionRunError> for LlmError {
    fn from(error: AgentSessionRunError) -> Self {
        match error {
            AgentSessionRunError::Agent(error)
            | AgentSessionRunError::Runtime(AgentSessionRuntimeError::Agent(error)) => error,
            error => Self::Protocol {
                provider: "session-run",
                message: error.to_string(),
            },
        }
    }
}
