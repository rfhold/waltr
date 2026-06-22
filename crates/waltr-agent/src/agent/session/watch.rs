//! Session list watch support.

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

use super::{SessionId, SessionSummary};

const SESSION_LIST_CHANNEL_CAPACITY: usize = 128;

/// Kind of session list change emitted by [`AgentSessions`](super::AgentSessions).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionListEvent {
    /// A session was created or updated.
    Upsert(SessionSummary),
    /// A session was deleted.
    Deleted(SessionId),
}

/// A session list update emitted by [`AgentSessions`](super::AgentSessions).
#[derive(Debug, Clone, PartialEq, Eq)]
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
