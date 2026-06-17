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

use super::SessionSummary;

const SESSION_LIST_CHANNEL_CAPACITY: usize = 128;

/// A session list update emitted by [`AgentSessions`](super::AgentSessions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListUpdate {
    /// Monotonic sequence for detecting missed updates.
    pub sequence: u64,
    /// Complete current summary for the changed session.
    pub session: SessionSummary,
}

/// Live stream of session list updates.
#[derive(Debug)]
pub struct SessionListSubscription {
    inner: BroadcastStream<SessionListUpdate>,
}

impl Stream for SessionListSubscription {
    type Item = SessionListUpdate;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(context) {
                Poll::Ready(Some(Ok(update))) => return Poll::Ready(Some(update)),
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => continue,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Debug, Clone)]
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
            inner: BroadcastStream::new(self.sender.subscribe()),
        }
    }

    pub(crate) fn publish(&self, session: SessionSummary) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        drop(self.sender.send(SessionListUpdate { sequence, session }));
    }
}
