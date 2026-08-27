//! Agent stream and host-neutral semantic event types.

use std::{collections::BTreeMap, future::Future, pin::Pin};

use futures_util::Stream;
use serde_json::Value;

use crate::{AgentTimelineEvent, Result, SessionId, SessionPauseLease, TimelineCursor};

/// Boxed stream of cursor-assigned agent timeline events.
pub type BoxAgentTimelineStream<'a> =
    std::pin::Pin<Box<dyn Stream<Item = Result<AgentTimelineEvent>> + Send + 'a>>;

/// Host-neutral semantic session event emitted after its source fact is durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A user message was durably appended to a session.
    UserMessageCreated {
        /// Deterministic event identity.
        id: String,
        /// Session receiving the message.
        session_id: SessionId,
        /// Zero-based durable prompt index.
        prompt_index: u64,
        /// One-based durable user-message count.
        user_message_count: u64,
        /// Durable message text after template rendering.
        text: String,
        /// Opaque session metadata captured from the durable record.
        metadata: BTreeMap<String, Value>,
        /// Session revision containing the appended prompt.
        revision: crate::Revision,
        /// Last durable timeline cursor available at event creation.
        cursor: Option<TimelineCursor>,
    },
}

/// Asynchronous delivery returned by a [`SessionEventSink`].
pub type SessionEventSinkFuture =
    Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send + 'static>>;

/// Injected delivery boundary for semantic session events.
pub trait SessionEventSink: Send + Sync {
    /// Deliver one event. Delivery failure never changes the source session operation.
    fn send(&self, event: SessionEvent) -> SessionEventSinkFuture;
}

/// Immutable provider-neutral facts for one durably terminal session run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRunCompletionSnapshot {
    /// Completed session.
    pub session_id: SessionId,
    /// Stable identity for this process-local submitted run.
    pub run_id: String,
    /// Last durable timeline cursor written by the terminal run.
    pub terminal_cursor: TimelineCursor,
    /// Latest completed provider turn's full context usage, when reported.
    pub context_tokens: Option<u64>,
    /// Host-defined session purpose, when present.
    pub purpose: Option<String>,
    /// Effective configured provider key used by the completed run.
    pub provider_key: String,
    /// Effective configured model identity used by the completed run.
    pub model_id: String,
}

/// Detached work started only after the coordinator accepts a claimed pause.
pub type SessionCompletionWorkerFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Bounded future preparing completion admission before any pause is claimed.
pub type SessionCompletionPreparationFuture =
    Pin<Box<dyn Future<Output = std::result::Result<SessionCompletionPreparation, String>> + Send>>;

/// Result of bounded completion preparation.
pub enum SessionCompletionPreparation {
    /// Continue ordinary queue draining.
    Continue,
    /// Atomically claim the barrier, then run this authoritative finalizer.
    Claim(Box<dyn SessionCompletionAdmissionFinalizer>),
}

impl std::fmt::Debug for SessionCompletionPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Continue => formatter.write_str("Continue"),
            Self::Claim(_) => formatter.write_str("Claim(..)"),
        }
    }
}

/// Non-abortable future finalizing durable admission after an atomic pause claim.
pub type SessionCompletionFinalizationFuture = Pin<
    Box<dyn Future<Output = std::result::Result<SessionCompletionWorkerFuture, String>> + Send>,
>;

/// Host-owned authoritative admission performed only after the coordinator claims a pause.
pub trait SessionCompletionAdmissionFinalizer: Send {
    /// Complete durable admission and transfer the lease into detached work.
    fn finalize(self: Box<Self>, lease: SessionPauseLease) -> SessionCompletionFinalizationFuture;
}

/// Host callback synchronously consulted before queued work drains after completion.
pub trait SessionCompletionAdmissionHandler: Send + Sync {
    /// Prepare optional admission without claiming or mutating coordinator ownership.
    fn prepare(&self, snapshot: SessionRunCompletionSnapshot)
    -> SessionCompletionPreparationFuture;
}

pub(super) fn user_message_event_id(session_id: &SessionId, journal_index: usize) -> String {
    format!("user-message:{}:j{journal_index}", session_id.as_str())
}

pub(super) fn spawn_session_event(sink: std::sync::Arc<dyn SessionEventSink>, event: SessionEvent) {
    drop(tokio::spawn(async move {
        if let Err(message) = sink.send(event).await {
            tracing::warn!(%message, "session event delivery failed");
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::user_message_event_id;
    use crate::agent::session::SessionRollbackTarget;
    use crate::{Revision, SessionId, SessionJournalEvent, SessionRecord};

    #[test]
    fn user_message_identity_is_deterministic_per_physical_append() {
        let session_id = SessionId::new("session-1");

        assert_eq!(
            user_message_event_id(&session_id, 4),
            user_message_event_id(&session_id, 4)
        );
        assert_ne!(
            user_message_event_id(&session_id, 4),
            user_message_event_id(&session_id, 6)
        );
    }

    #[test]
    fn rollback_replacement_has_new_identity_that_is_stable_after_reload() {
        let mut record = SessionRecord::new("session-1");
        record.append_journal_event(SessionJournalEvent::PromptAppended {
            prompt: "first".to_owned(),
        });
        let first_id = user_message_event_id(&record.id, 0);
        record.append_journal_event(SessionJournalEvent::Rollback {
            target: SessionRollbackTarget {
                journal_len: 0,
                after_cursor: None,
                next_turn: 0,
                remove_prompt_index: Some(0),
            },
        });
        record.append_journal_event(SessionJournalEvent::PromptAppended {
            prompt: "replacement".to_owned(),
        });
        record.revision = Revision::new(2);
        let replacement_id = user_message_event_id(&record.id, 2);
        let reloaded: SessionRecord =
            serde_json::from_str(&serde_json::to_string(&record).expect("record serializes"))
                .expect("record reloads");

        assert_ne!(first_id, replacement_id);
        assert_eq!(replacement_id, user_message_event_id(&reloaded.id, 2));
    }
}
