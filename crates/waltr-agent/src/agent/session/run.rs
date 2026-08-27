//! Durable session run contracts.

use std::{
    error::Error,
    fmt,
    future::Future,
    num::NonZeroU64,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::oneshot;

use crate::{
    AuthTarget, AuthenticationFailure, AuthenticationFailureReason, LlmError,
    McpAuthorizationContext, ToolResult,
};

use super::{ExternalRequestId, ExternalResponse, SessionId, SessionStatus};
use crate::{SessionStoreError, TimelineCursor};

/// Maximum UTF-8 byte length of a coordination reason code.
pub const MAX_SESSION_COORDINATION_REASON_CODE_BYTES: usize = 64;
/// Maximum UTF-8 byte length of an optional coordination reason attribute.
pub const MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES: usize = 128;

/// Process-local session coordination phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionCoordinationPhase {
    /// No process-local work currently owns the session.
    #[default]
    Ready,
    /// A submitted run currently owns the session.
    Running,
    /// A completed run is undergoing bounded host admission checks.
    CompletionBarrier,
    /// A host operation owns a process-local pause.
    Paused,
}

/// Validated machine-readable reason for a completion barrier or owned pause.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionCoordinationReason {
    code: String,
    source: Option<String>,
    trigger: Option<String>,
}

impl SessionCoordinationReason {
    /// Construct a bounded host-neutral reason descriptor.
    ///
    /// # Errors
    ///
    /// Returns an error when the required code is empty or any field exceeds its byte limit.
    pub fn new(
        code: impl Into<String>,
        source: Option<String>,
        trigger: Option<String>,
    ) -> Result<Self, SessionCoordinationReasonError> {
        let code = code.into();
        validate_reason_field(
            "code",
            &code,
            MAX_SESSION_COORDINATION_REASON_CODE_BYTES,
            false,
        )?;
        if let Some(source) = source.as_deref() {
            validate_reason_field(
                "source",
                source,
                MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES,
                true,
            )?;
        }
        if let Some(trigger) = trigger.as_deref() {
            validate_reason_field(
                "trigger",
                trigger,
                MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES,
                true,
            )?;
        }
        Ok(Self {
            code,
            source,
            trigger,
        })
    }

    /// Borrow the machine-readable reason code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Borrow the optional host operation or source identity.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Borrow the optional trigger identity.
    #[must_use]
    pub fn trigger(&self) -> Option<&str> {
        self.trigger.as_deref()
    }

    pub(super) fn post_run_automation() -> Self {
        Self {
            code: "post-run-automation".to_owned(),
            source: None,
            trigger: None,
        }
    }
}

/// Invalid coordination reason descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCoordinationReasonError {
    field: &'static str,
    message: &'static str,
}

impl fmt::Display for SessionCoordinationReasonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid session coordination reason {}: {}",
            self.field, self.message
        )
    }
}

impl Error for SessionCoordinationReasonError {}

impl<'de> serde::Deserialize<'de> for SessionCoordinationReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Fields {
            code: String,
            source: Option<String>,
            trigger: Option<String>,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.code, fields.source, fields.trigger).map_err(serde::de::Error::custom)
    }
}

fn validate_reason_field(
    field: &'static str,
    value: &str,
    maximum: usize,
    optional: bool,
) -> Result<(), SessionCoordinationReasonError> {
    if value.is_empty() {
        return Err(SessionCoordinationReasonError {
            field,
            message: if optional {
                "must be omitted rather than empty"
            } else {
                "must not be empty"
            },
        });
    }
    if value.len() > maximum {
        return Err(SessionCoordinationReasonError {
            field,
            message: "exceeds the maximum UTF-8 byte length",
        });
    }
    Ok(())
}

/// Process-local coordination facts projected onto a session summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionCoordinationState {
    /// Current process-local ownership phase.
    pub phase: SessionCoordinationPhase,
    /// Machine-readable barrier or pause reason, when applicable.
    pub reason: Option<SessionCoordinationReason>,
    /// Logical submitted runs queued behind current ownership.
    pub queued_count: usize,
    /// Whether a host continuation is armed ahead of queued user work.
    pub continuation_armed: bool,
}

/// Bounded future preparing host completion coordination before any pause is claimed.
pub type SessionCompletionCoordinationFuture = Pin<
    Box<
        dyn Future<Output = std::result::Result<SessionCompletionCoordinationPreparation, String>>
            + Send,
    >,
>;

/// Host completion decision carrying the reason used if pause ownership is claimed.
pub enum SessionCompletionCoordinationPreparation {
    /// Continue ordinary queue draining.
    Continue,
    /// Atomically replace the completion barrier with a reasoned pause, then finalize admission.
    Claim {
        /// Descriptor installed when the pause claim becomes authoritative.
        reason: SessionCoordinationReason,
        /// Host finalizer invoked only after the pause is claimed.
        finalizer: Box<dyn crate::SessionCompletionAdmissionFinalizer>,
    },
}

impl fmt::Debug for SessionCompletionCoordinationPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continue => formatter.write_str("Continue"),
            Self::Claim { reason, .. } => formatter
                .debug_struct("Claim")
                .field("reason", reason)
                .finish_non_exhaustive(),
        }
    }
}

/// Host callback consulted before queued work drains after a completed run.
pub trait SessionCompletionCoordinationHandler: Send + Sync {
    /// Prepare optional reasoned pause admission without claiming coordinator ownership.
    fn prepare(
        &self,
        snapshot: crate::SessionRunCompletionSnapshot,
    ) -> SessionCompletionCoordinationFuture;
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) type RunResult = std::result::Result<AgentSessionRunOutcome, AgentSessionRunError>;
#[allow(clippy::redundant_pub_crate)]
pub(crate) type RunOutcomeSender = oneshot::Sender<RunResult>;

/// Immutable effective inference identity captured for one admitted run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionRunInferenceIdentity {
    pub(super) provider_key: String,
    pub(super) model_id: String,
}

impl SessionRunInferenceIdentity {
    pub(super) fn from_settings(settings: &super::InferenceSettings) -> Self {
        Self {
            provider_key: settings.provider.key.clone(),
            model_id: settings.provider.model_id.clone(),
        }
    }
}

/// Opaque process-local identity for one owned session pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionPauseId(u64);

impl SessionPauseId {
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Owned process-local lease pinning one completed session boundary.
#[derive(Debug)]
pub struct SessionPauseLease {
    pub(super) session_id: SessionId,
    pub(super) id: SessionPauseId,
    pub(super) source_cursor: TimelineCursor,
    pub(super) timeline_boundary: TimelineCursor,
    pub(super) released: Arc<AtomicBool>,
}

impl SessionPauseLease {
    pub(super) fn new(
        session_id: SessionId,
        id: SessionPauseId,
        source_cursor: TimelineCursor,
        timeline_boundary: TimelineCursor,
    ) -> Self {
        Self {
            session_id,
            id,
            source_cursor,
            timeline_boundary,
            released: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return the paused session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Return the exact completed timeline boundary pinned by this lease.
    #[must_use]
    pub const fn source_cursor(&self) -> TimelineCursor {
        self.source_cursor
    }

    pub(super) fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }

    pub(super) fn mark_released(&self) {
        self.released.store(true, Ordering::Release);
    }
}

/// Typed failure from owned pause coordination.
#[derive(Debug)]
pub enum SessionPauseError {
    /// A run already owns the session supervisor.
    Active {
        /// Busy session.
        session_id: SessionId,
    },
    /// Another pause already owns the session supervisor.
    Paused {
        /// Busy session.
        session_id: SessionId,
    },
    /// Lifecycle or mutation work currently owns the session.
    LifecycleBusy {
        /// Busy session.
        session_id: SessionId,
    },
    /// The coordinator is shutting down or wiping sessions.
    Unavailable {
        /// Unavailable session.
        session_id: SessionId,
    },
    /// The lease no longer owns the live pause.
    OwnershipLost {
        /// Session formerly owned by the lease.
        session_id: SessionId,
    },
    /// The source has no exact latest completed timeline boundary.
    CompletedBoundaryRequired {
        /// Session lacking an eligible boundary.
        session_id: SessionId,
    },
    /// Timeline work changed the boundary pinned by the lease.
    BoundaryChanged {
        /// Session whose timeline changed.
        session_id: SessionId,
        /// Boundary pinned by the lease.
        expected: TimelineCursor,
        /// Current latest timeline boundary.
        actual: Option<TimelineCursor>,
    },
    /// A continuation is already armed for this pause.
    ContinuationAlreadyArmed {
        /// Paused session.
        session_id: SessionId,
    },
    /// The requested continuation cursor was not committed by this pause.
    ContinuationCursorMismatch {
        /// Paused session.
        session_id: SessionId,
        /// Last mutation cursor committed through the pause.
        expected: Option<TimelineCursor>,
        /// Cursor supplied by the caller.
        actual: TimelineCursor,
    },
    /// Effective capture failed after pause admission.
    Capture(super::SessionCaptureError),
    /// The atomic mask mutation failed.
    Store(SessionStoreError),
    /// Runtime resolution for continuation failed.
    Run(AgentSessionRunError),
}

impl fmt::Display for SessionPauseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active { session_id } => write!(
                formatter,
                "session has an active run: {}",
                session_id.as_str()
            ),
            Self::Paused { session_id } => write!(
                formatter,
                "session is already paused: {}",
                session_id.as_str()
            ),
            Self::LifecycleBusy { session_id } => write!(
                formatter,
                "session lifecycle is busy: {}",
                session_id.as_str()
            ),
            Self::Unavailable { session_id } => write!(
                formatter,
                "session coordinator is unavailable: {}",
                session_id.as_str()
            ),
            Self::OwnershipLost { session_id } => write!(
                formatter,
                "session pause ownership was lost: {}",
                session_id.as_str()
            ),
            Self::CompletedBoundaryRequired { session_id } => write!(
                formatter,
                "session has no exact latest completed boundary: {}",
                session_id.as_str()
            ),
            Self::BoundaryChanged {
                session_id,
                expected,
                actual,
            } => write!(
                formatter,
                "session timeline boundary changed for {}: expected {}, actual {actual:?}",
                session_id.as_str(),
                expected.get()
            ),
            Self::ContinuationAlreadyArmed { session_id } => write!(
                formatter,
                "session pause already has a continuation: {}",
                session_id.as_str()
            ),
            Self::ContinuationCursorMismatch {
                session_id,
                expected,
                actual,
            } => write!(
                formatter,
                "session continuation cursor mismatch for {}: expected {expected:?}, actual {}",
                session_id.as_str(),
                actual.get()
            ),
            Self::Capture(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Run(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for SessionPauseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Capture(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Run(error) => Some(error),
            _ => None,
        }
    }
}

impl From<super::SessionCaptureError> for SessionPauseError {
    fn from(error: super::SessionCaptureError) -> Self {
        Self::Capture(error)
    }
}

/// Error returned while resolving a session runtime.
#[derive(Debug)]
pub enum AgentSessionRuntimeError {
    /// The session is missing required runtime configuration.
    FailedPrecondition(String),
    /// Runtime resolution failed due to an internal host error.
    Internal(String),
    /// Runtime resolution failed with a provider-neutral agent error.
    Agent(LlmError),
    /// Runtime resolution encountered a structured authentication failure.
    Authentication {
        /// Target whose authentication failed.
        target: AuthTarget,
        /// Human-readable reason.
        message: String,
        /// Machine-readable reason for the failure.
        reason: AuthenticationFailureReason,
        /// Sanitized MCP challenge context, when the target is an MCP server.
        mcp_authorization: Option<Box<McpAuthorizationContext>>,
    },
}

impl AgentSessionRuntimeError {
    /// Return structured authentication data carried by this runtime error.
    #[must_use]
    pub fn authentication_failure(&self) -> Option<AuthenticationFailure> {
        match self {
            Self::Authentication {
                target,
                reason,
                mcp_authorization,
                ..
            } => Some(AuthenticationFailure {
                target: target.clone(),
                reason: *reason,
                mcp_authorization: mcp_authorization.clone(),
            }),
            Self::Agent(error) => error.authentication_failure().cloned(),
            Self::FailedPrecondition(_) | Self::Internal(_) => None,
        }
    }
}

impl fmt::Display for AgentSessionRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FailedPrecondition(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::Authentication {
                target, message, ..
            } => {
                write!(formatter, "authentication failed for {target:?}: {message}")
            }
        }
    }
}

impl Error for AgentSessionRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::FailedPrecondition(_) | Self::Internal(_) | Self::Authentication { .. } => None,
        }
    }
}

impl From<LlmError> for AgentSessionRuntimeError {
    fn from(error: LlmError) -> Self {
        Self::Agent(error)
    }
}

/// Optional execution limits captured for one submitted agent run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentRunLimits {
    /// Maximum logical provider turns. Stream events and transport retries do not consume turns.
    pub max_provider_turns: Option<NonZeroU64>,
}

impl AgentRunLimits {
    /// Limit the run to `max_provider_turns` logical provider dispatches.
    #[must_use]
    pub const fn with_max_provider_turns(mut self, max_provider_turns: NonZeroU64) -> Self {
        self.max_provider_turns = Some(max_provider_turns);
        self
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
    /// An owned pause prevents this conflict policy from starting work.
    Paused {
        /// Session held by an owned pause.
        session_id: SessionId,
    },
    /// Session run admission is disabled during lifecycle shutdown.
    Unavailable {
        /// Session that rejected admission.
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
    /// Return structured authentication data carried by this run error.
    #[must_use]
    pub fn authentication_failure(&self) -> Option<AuthenticationFailure> {
        match self {
            Self::Runtime(error) => error.authentication_failure(),
            Self::Agent(error) => error.authentication_failure().cloned(),
            Self::AlreadyRunning { .. }
            | Self::Paused { .. }
            | Self::Unavailable { .. }
            | Self::Canceled { .. }
            | Self::OutcomeDropped { .. } => None,
        }
    }

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
            Self::Paused { session_id } => {
                write!(
                    formatter,
                    "session is held by an owned pause: {}",
                    session_id.as_str()
                )
            }
            Self::Unavailable { session_id } => write!(
                formatter,
                "session run admission is unavailable: {}",
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
            Self::AlreadyRunning { .. }
            | Self::Paused { .. }
            | Self::Unavailable { .. }
            | Self::Canceled { .. }
            | Self::OutcomeDropped { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordination_reason_rejects_empty_and_oversized_fields() {
        assert!(SessionCoordinationReason::new("", None, None).is_err());
        assert!(
            SessionCoordinationReason::new("host-operation", Some(String::new()), None,).is_err()
        );
        assert!(
            SessionCoordinationReason::new(
                "x".repeat(MAX_SESSION_COORDINATION_REASON_CODE_BYTES + 1),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            SessionCoordinationReason::new(
                "host-operation",
                None,
                Some("x".repeat(MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES + 1)),
            )
            .is_err()
        );
    }

    #[test]
    fn coordination_reason_exposes_validated_machine_fields() {
        let reason = SessionCoordinationReason::new(
            "context-compaction",
            Some("compact-and-continue".to_owned()),
            Some("manual".to_owned()),
        )
        .unwrap();
        assert_eq!(reason.code(), "context-compaction");
        assert_eq!(reason.source(), Some("compact-and-continue"));
        assert_eq!(reason.trigger(), Some("manual"));
    }
}
