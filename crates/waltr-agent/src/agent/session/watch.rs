//! Session watch support.

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

use super::{SessionId, SessionSummary};

const SESSION_LIST_CHANNEL_CAPACITY: usize = 128;
const SESSION_TIMELINE_CHANNEL_CAPACITY: usize = 1024;

/// Kind of session list change emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionListEvent {
    /// A session was created or updated.
    Upsert(SessionSummary),
    /// A session was deleted.
    Deleted(SessionId),
}

/// A session list update emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionListUpdate {
    /// Monotonic sequence for detecting missed updates.
    pub sequence: u64,
    /// Session list change event.
    pub event: SessionListEvent,
}

/// Live stream of session list updates.
#[derive(Debug)]
pub struct SessionListSubscription {
    initial_sequence: u64,
    inner: BroadcastStream<SessionListUpdate>,
}

impl SessionListSubscription {
    /// Sequence observed when this subscription was opened.
    #[must_use]
    pub const fn initial_sequence(&self) -> u64 {
        self.initial_sequence
    }
}

impl Stream for SessionListSubscription {
    type Item = SessionListUpdate;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(context) {
                Poll::Ready(Some(Ok(update))) => return Poll::Ready(Some(update)),
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {}
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct SessionListBroadcaster {
    sequence: Arc<AtomicU64>,
    sender: broadcast::Sender<SessionListUpdate>,
}

/// A timeline update emitted by [`SessionCoordinator`](super::SessionCoordinator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimelineUpdate {
    /// Monotonic sequence for detecting missed live updates.
    pub sequence: u64,
    /// Timeline update payload.
    pub event: SessionTimelineEvent,
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

/// Live stream of persisted timeline updates for one session.
#[derive(Debug)]
pub struct SessionTimelineSubscription {
    session_id: SessionId,
    initial_sequence: u64,
    inner: BroadcastStream<SessionTimelineUpdate>,
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
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct SessionTimelineBroadcaster {
    sequence: Arc<AtomicU64>,
    sender: broadcast::Sender<SessionTimelineUpdate>,
}

impl SessionTimelineBroadcaster {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(SESSION_TIMELINE_CHANNEL_CAPACITY);
        Self {
            sequence: Arc::new(AtomicU64::new(0)),
            sender,
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
        self.publish_update(SessionTimelineEvent::Event(event));
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

    fn publish_update(&self, event: SessionTimelineEvent) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        drop(self.sender.send(SessionTimelineUpdate { sequence, event }));
    }

    pub(crate) fn publish_all(&self, events: impl IntoIterator<Item = AgentTimelineEvent>) {
        for event in events {
            self.publish(event);
        }
    }
}

impl SessionListBroadcaster {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(SESSION_LIST_CHANNEL_CAPACITY);
        Self {
            sequence: Arc::new(AtomicU64::new(0)),
            sender,
        }
    }

    pub(crate) fn subscribe(&self) -> SessionListSubscription {
        SessionListSubscription {
            initial_sequence: self.sequence.load(Ordering::Relaxed),
            inner: BroadcastStream::new(self.sender.subscribe()),
        }
    }

    pub(crate) fn publish(&self, event: SessionListEvent) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        drop(self.sender.send(SessionListUpdate { sequence, event }));
    }
}
