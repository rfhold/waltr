//! Per-session timeline watch support.

#![allow(clippy::redundant_pub_crate)]

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

use crate::timeline::{AgentTimelineEvent, TimelineCursor};

use super::{Revision, SessionId};

const SESSION_TIMELINE_CHANNEL_CAPACITY: usize = 1024;

/// A timeline update emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimelineUpdate {
    /// Monotonic sequence for detecting missed live updates.
    pub sequence: u64,
    /// Timeline update payload.
    pub event: SessionTimelineEvent,
}

/// Effective-projection update emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimelineProjectionUpdate {
    /// Monotonic sequence shared with raw timeline publication.
    pub sequence: u64,
    /// Effective projection update payload.
    pub event: SessionTimelineProjectionUpdateEvent,
}

/// Live update relevant to an effective timeline projection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionTimelineProjectionUpdateEvent {
    /// Newly persisted raw event. Active masks never synthesize raw publication.
    Event(AgentTimelineEvent),
    /// Administrative state changed and consumers must replace their projection.
    Invalidated {
        /// Session whose effective projection changed.
        session_id: SessionId,
        /// Monotonic revision of the replacement projection.
        projection_revision: Revision,
    },
    /// Broadcast retention was exceeded; replace state from a fresh projection query.
    ResetRequired {
        /// Session whose projection must be reloaded.
        session_id: SessionId,
        /// Number of updates dropped by the local subscription.
        missed_updates: u64,
    },
}

impl SessionTimelineProjectionUpdateEvent {
    const fn session_id(&self) -> &SessionId {
        match self {
            Self::Event(event) => &event.session_id,
            Self::Invalidated { session_id, .. } | Self::ResetRequired { session_id, .. } => {
                session_id
            }
        }
    }
}

/// Timeline watch payload emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionTimelineEvent {
    /// Persisted timeline event with an assigned cursor.
    Event(AgentTimelineEvent),
    /// Timeline events after `after_cursor` were logically removed from the visible session.
    Truncated {
        /// Session that was truncated.
        session_id: SessionId,
        /// Last visible cursor that remains after truncation.
        after_cursor: Option<TimelineCursor>,
    },
}

impl SessionTimelineEvent {
    const fn session_id(&self) -> &SessionId {
        match self {
            Self::Event(event) => &event.session_id,
            Self::Truncated { session_id, .. } => session_id,
        }
    }
}

/// Live stream of persisted timeline updates for one durable session.
#[derive(Debug)]
pub struct SessionTimelineSubscription {
    session_id: SessionId,
    initial_sequence: u64,
    inner: BroadcastStream<SessionTimelineUpdate>,
}

/// Live stream used to keep one effective timeline projection current.
#[derive(Debug)]
pub struct SessionTimelineProjectionSubscription {
    session_id: SessionId,
    initial_sequence: u64,
    last_sequence: u64,
    inner: BroadcastStream<SessionTimelineProjectionUpdate>,
}

impl SessionTimelineProjectionSubscription {
    /// Sequence observed when this subscription was opened.
    #[must_use]
    pub const fn initial_sequence(&self) -> u64 {
        self.initial_sequence
    }
}

impl Stream for SessionTimelineProjectionSubscription {
    type Item = SessionTimelineProjectionUpdate;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(context) {
                Poll::Ready(Some(Ok(update))) => {
                    self.last_sequence = update.sequence;
                    if update.event.session_id() == &self.session_id {
                        return Poll::Ready(Some(update));
                    }
                }
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(missed_updates)))) => {
                    return Poll::Ready(Some(SessionTimelineProjectionUpdate {
                        sequence: self.last_sequence,
                        event: SessionTimelineProjectionUpdateEvent::ResetRequired {
                            session_id: self.session_id.clone(),
                            missed_updates,
                        },
                    }));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl SessionTimelineSubscription {
    /// Sequence observed when this subscription was opened.
    #[must_use]
    pub const fn initial_sequence(&self) -> u64 {
        self.initial_sequence
    }
}

impl Stream for SessionTimelineSubscription {
    type Item = SessionTimelineUpdate;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(context) {
                Poll::Ready(Some(Ok(update))) if update.event.session_id() == &self.session_id => {
                    return Poll::Ready(Some(update));
                }
                Poll::Ready(Some(Ok(_) | Err(BroadcastStreamRecvError::Lagged(_)))) => {}
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionTimelineBroadcaster {
    sequence: Arc<AtomicU64>,
    sender: broadcast::Sender<SessionTimelineUpdate>,
    projection_sender: broadcast::Sender<SessionTimelineProjectionUpdate>,
}

impl SessionTimelineBroadcaster {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(SESSION_TIMELINE_CHANNEL_CAPACITY);
        let (projection_sender, _) = broadcast::channel(SESSION_TIMELINE_CHANNEL_CAPACITY);
        Self {
            sequence: Arc::new(AtomicU64::new(0)),
            sender,
            projection_sender,
        }
    }

    pub(crate) fn subscribe_projection(
        &self,
        session_id: SessionId,
    ) -> SessionTimelineProjectionSubscription {
        let inner = BroadcastStream::new(self.projection_sender.subscribe());
        let initial_sequence = self.sequence.load(Ordering::Relaxed);
        SessionTimelineProjectionSubscription {
            session_id,
            initial_sequence,
            last_sequence: initial_sequence,
            inner,
        }
    }

    pub(crate) fn subscribe(&self, session_id: SessionId) -> SessionTimelineSubscription {
        SessionTimelineSubscription {
            session_id,
            initial_sequence: self.sequence.load(Ordering::Relaxed),
            inner: BroadcastStream::new(self.sender.subscribe()),
        }
    }

    pub(crate) fn publish(&self, event: AgentTimelineEvent) {
        let sequence = self.next_sequence();
        drop(self.sender.send(SessionTimelineUpdate {
            sequence,
            event: SessionTimelineEvent::Event(event.clone()),
        }));
        drop(
            self.projection_sender
                .send(SessionTimelineProjectionUpdate {
                    sequence,
                    event: SessionTimelineProjectionUpdateEvent::Event(event),
                }),
        );
    }

    pub(crate) fn publish_truncation(
        &self,
        session_id: SessionId,
        after_cursor: Option<TimelineCursor>,
    ) {
        self.publish_update(SessionTimelineEvent::Truncated {
            session_id,
            after_cursor,
        });
    }

    pub(crate) fn publish_projection_invalidation(
        &self,
        session_id: SessionId,
        projection_revision: Revision,
    ) {
        let sequence = self.next_sequence();
        drop(
            self.projection_sender
                .send(SessionTimelineProjectionUpdate {
                    sequence,
                    event: SessionTimelineProjectionUpdateEvent::Invalidated {
                        session_id,
                        projection_revision,
                    },
                }),
        );
    }

    fn publish_update(&self, event: SessionTimelineEvent) {
        let sequence = self.next_sequence();
        drop(self.sender.send(SessionTimelineUpdate { sequence, event }));
    }

    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn publish_all(&self, events: impl IntoIterator<Item = AgentTimelineEvent>) {
        for event in events {
            self.publish(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;

    #[tokio::test]
    async fn effective_subscription_surfaces_sequence_gap_as_reset() {
        let broadcaster = SessionTimelineBroadcaster::new();
        let id = SessionId::new("projection-gap");
        let mut subscription = broadcaster.subscribe_projection(id.clone());

        for revision in 1..=(SESSION_TIMELINE_CHANNEL_CAPACITY as u64 + 1) {
            broadcaster.publish_projection_invalidation(id.clone(), Revision::new(revision));
        }

        let update = subscription.next().await.unwrap();
        assert!(matches!(
            update.event,
            SessionTimelineProjectionUpdateEvent::ResetRequired {
                session_id,
                missed_updates: 1,
            } if session_id == id
        ));
        assert_eq!(
            broadcaster.subscribe_projection(id).initial_sequence(),
            SESSION_TIMELINE_CHANNEL_CAPACITY as u64 + 1
        );
    }
}
