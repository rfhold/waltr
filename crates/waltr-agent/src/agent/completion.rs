//! Opt-in proposal validation, independent of any interpreter or host integration.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{SessionId, TimelineItemId, ToolCall, ToolResult};

/// Maximum cumulative encoded tool evidence and proposal bytes.
pub const MAX_COMPLETION_EVIDENCE_BYTES: usize = 262_144;
/// Maximum corrective message bytes.
pub const MAX_COMPLETION_FEEDBACK_BYTES: usize = 4096;
/// Maximum logical provider turns in a validated run.
pub const MAX_COMPLETION_TURNS: u64 = 64;
/// Maximum total validated-run duration.
pub const MAX_COMPLETION_DURATION: Duration = Duration::from_secs(300);

/// Immutable evidence after host execution and result projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionToolEvidence {
    /// Opaque host-selected provenance, independent of the model-visible tool name.
    /// This excludes raw transport metadata and authorization secrets.
    pub target: Option<String>,
    /// Effective post-middleware call, never rewritten with validation feedback.
    pub call: ToolCall,
    /// Actual provider-visible post-policy result, including errors and images.
    pub result: ToolResult,
    /// Durable tool-call timeline identity.
    pub item_id: TimelineItemId,
    /// Provider turn that executed this call.
    pub turn: u64,
}

/// A candidate is provisional until the observer accepts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionProposal {
    /// Durable logical helper identity.
    pub session_id: SessionId,
    /// Stable proposal identity, scoped to the helper.
    pub proposal_id: String,
    /// Provider turn producing the candidate.
    pub turn: u64,
    /// Candidate final response, not committed output.
    pub response: String,
    /// Cumulative immutable evidence from this run, not copied source history.
    pub evidence: Vec<CompletionToolEvidence>,
}

/// Corrective feedback linked to one proposal and optionally one original tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompletionFeedback {
    /// Must equal the observed proposal identity.
    pub proposal_id: String,
    /// If supplied, must name a call in the proposal's evidence.
    pub call_id: Option<String>,
    /// Nonempty corrective instructions, bounded to 4096 UTF-8 bytes.
    pub message: String,
}

/// Semantic validation, separate from capture-schema validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase", deny_unknown_fields)]
pub enum CompletionDecision {
    /// Commit this candidate as the final response.
    Accept,
    /// Continue the same helper with durable corrective feedback.
    Reject {
        /// Bounded correction tied to this exact proposal.
        feedback: CompletionFeedback,
    },
}

/// Trusted observer failure; never includes untrusted exception text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionObserverError {
    /// Callback evaluation failed.
    Failed,
    /// Callback returned an invalid verdict.
    Invalid,
}

/// Object-safe, owned asynchronous observer work.
pub type CompletionObserverFuture = Pin<
    Box<dyn Future<Output = Result<CompletionDecision, CompletionObserverError>> + Send + 'static>,
>;

/// Consumer-supplied semantic validator. No engine values cross this boundary.
pub trait CompletionObserver: Send + Sync {
    /// Observe immutable post-policy evidence; cancellation applies to nested work too.
    fn observe(
        &self,
        proposal: CompletionProposal,
        cancellation: CancellationToken,
    ) -> CompletionObserverFuture;
}

/// Invocation-local validation policy; budgets never reset on rejection.
#[derive(Clone)]
pub struct CompletionValidation {
    pub(super) observer: Arc<dyn CompletionObserver>,
    pub(super) encoded_byte_limit: usize,
    pub(super) max_turns: u64,
    pub(super) deadline: tokio::time::Instant,
}

impl CompletionValidation {
    /// Capture the total deadline now, including queue wait.
    ///
    /// # Errors
    /// Rejects zero or excessive turn and duration bounds.
    pub fn new(
        observer: Arc<dyn CompletionObserver>,
        max_turns: u64,
        duration: Duration,
    ) -> Result<Self, CompletionObserverError> {
        if !(1..=MAX_COMPLETION_TURNS).contains(&max_turns)
            || duration.is_zero()
            || duration > MAX_COMPLETION_DURATION
        {
            return Err(CompletionObserverError::Invalid);
        }
        Ok(Self {
            observer,
            encoded_byte_limit: MAX_COMPLETION_EVIDENCE_BYTES,
            max_turns,
            deadline: tokio::time::Instant::now() + duration,
        })
    }

    /// Pin the cumulative encoded evidence and proposal ceiling for this run.
    ///
    /// # Errors
    /// Rejects zero or limits above 32 MiB. Other run limits remain unchanged.
    pub fn with_encoded_byte_limit(
        mut self,
        bytes: usize,
    ) -> Result<Self, CompletionObserverError> {
        if bytes == 0 || bytes > 32 * 1024 * 1024 {
            return Err(CompletionObserverError::Invalid);
        }
        self.encoded_byte_limit = bytes;
        Ok(self)
    }
}

impl std::fmt::Debug for CompletionValidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionValidation")
            .field("max_turns", &self.max_turns)
            .finish_non_exhaustive()
    }
}

pub(super) fn validation_error(message: &'static str) -> crate::LlmError {
    crate::LlmError::Protocol {
        provider: "completion-validation",
        message: message.into(),
    }
}

pub(super) async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
