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
        /// Durable session incarnation at the append.
        incarnation: Option<String>,
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
    /// Best-effort nonblocking post-commit semantic notification. Never replayed on hydration.
    fn semantic(&self, _event: SessionSemanticEvent) {}
}

/// Durable live facts, distinct from provider progress and preview text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSemanticKind {
    /// A live run committed its terminal finished journal entry.
    RunFinished,
    /// A live journal transaction committed a completed provider turn.
    TurnCompleted {
        /// Completed provider turn ordinal.
        turn: u64,
    },
    /// A tool result committed. This does not assert that an asynchronous task completed.
    ToolResultCommitted {
        /// Durable tool-call identity.
        call_id: String,
        /// Effective tool name, not provider preview text.
        tool_name: String,
        /// Whether the result reports an execution error.
        succeeded: bool,
    },
}

/// Bounded semantic notification with no model output or tool-result payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSemanticEvent {
    /// Deterministic live-commit identity within this session incarnation.
    pub id: String,
    /// Owning source session.
    pub session_id: SessionId,
    /// Durable incarnation, absent only in legacy source records.
    pub incarnation: Option<String>,
    /// Committed journal revision.
    pub revision: crate::Revision,
    /// Last durable timeline boundary at the commit.
    pub cursor: Option<TimelineCursor>,
    /// Typed committed fact.
    pub kind: SessionSemanticKind,
}

/// Bounded live-only semantic fanout. Lost events are explicit; there is no backfill.
#[derive(Debug)]
pub struct SessionSemanticHub {
    sender: tokio::sync::broadcast::Sender<SessionSemanticEvent>,
    subscribers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SessionSemanticHub {
    /// Create a live event ring of 1..=1024 events with at most 32 subscribers.
    ///
    /// # Errors
    /// Rejects zero or unbounded capacities.
    pub fn new(capacity: usize) -> std::result::Result<Self, &'static str> {
        if !(1..=1024).contains(&capacity) {
            return Err("semantic event capacity must be 1..=1024");
        }
        let (sender, _) = tokio::sync::broadcast::channel(capacity);
        Ok(Self {
            sender,
            subscribers: std::sync::Arc::default(),
        })
    }

    /// Subscribe without historical replay.
    ///
    /// # Errors
    /// Rejects the 33rd concurrent subscriber.
    pub fn subscribe(&self) -> std::result::Result<SessionSemanticSubscription, &'static str> {
        use std::sync::atomic::Ordering;
        self.subscribers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 32).then_some(count + 1)
            })
            .map_err(|_| "semantic subscriber capacity reached")?;
        Ok(SessionSemanticSubscription {
            receiver: self.sender.subscribe(),
            subscribers: self.subscribers.clone(),
        })
    }

    /// Enqueue one bounded notification without awaiting consumers.
    ///
    /// # Errors
    /// Rejects oversized identities rather than silently dropping an invalid notification.
    pub fn publish(&self, event: SessionSemanticEvent) -> std::result::Result<(), &'static str> {
        let bounded = event.id.len() <= 512
            && event.session_id.as_str().len() <= 256
            && event.incarnation.as_ref().is_none_or(|id| id.len() <= 128)
            && match &event.kind {
                SessionSemanticKind::RunFinished => true,
                SessionSemanticKind::TurnCompleted { .. } => true,
                SessionSemanticKind::ToolResultCommitted {
                    call_id, tool_name, ..
                } => call_id.len() <= 256 && tool_name.len() <= 256,
            };
        if !bounded {
            return Err("semantic notification exceeds identity bounds");
        }
        drop(self.sender.send(event));
        Ok(())
    }
}

/// One bounded semantic subscriber. A lag error must not be interpreted as coverage.
#[derive(Debug)]
pub struct SessionSemanticSubscription {
    receiver: tokio::sync::broadcast::Receiver<SessionSemanticEvent>,
    subscribers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl SessionSemanticSubscription {
    /// Receive the next live fact or an explicit `Lagged`/`Closed` error.
    ///
    /// # Errors
    /// Reports loss or shutdown without recovery replay.
    pub async fn recv(
        &mut self,
    ) -> std::result::Result<SessionSemanticEvent, tokio::sync::broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}
impl Drop for SessionSemanticSubscription {
    fn drop(&mut self) {
        self.subscribers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Compose existing user-message delivery with bounded semantic notifications.
pub struct SemanticSessionEventSink {
    /// Existing user-message sink; its lifecycle and delivery contract are unchanged.
    pub messages: std::sync::Arc<dyn SessionEventSink>,
    /// Live-only bounded semantic source.
    pub semantics: std::sync::Arc<SessionSemanticHub>,
}
impl std::fmt::Debug for SemanticSessionEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticSessionEventSink")
            .finish_non_exhaustive()
    }
}
impl SessionEventSink for SemanticSessionEventSink {
    fn send(&self, event: SessionEvent) -> SessionEventSinkFuture {
        self.messages.send(event)
    }
    fn semantic(&self, event: SessionSemanticEvent) {
        if self.semantics.publish(event).is_err() {
            tracing::warn!("oversized semantic notification was not delivered");
        }
    }
}

/// Immutable provider-neutral facts for one durably terminal session run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRunCompletionSnapshot {
    /// Completed session.
    pub session_id: SessionId,
    /// Durable incarnation containing this completion.
    pub incarnation: Option<String>,
    /// Journal revision captured with the terminal boundary.
    pub revision: crate::Revision,
    /// Stable identity for this process-local submitted run.
    pub run_id: String,
    /// Last durable timeline cursor written by the terminal run.
    pub terminal_cursor: TimelineCursor,
    /// Latest completed provider turn's full context usage, when reported.
    pub context_tokens: Option<u64>,
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
