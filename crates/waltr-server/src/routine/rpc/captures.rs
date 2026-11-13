//! Invocation-local admission for opt-in repeated input streams.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::routine::media::stream::Reader;
use crate::routine::ports::PortError;

const MAX_CAPTURES: usize = 16;

#[derive(Default)]
struct State {
    sealed: bool,
    admitted: usize,
    pending: BTreeMap<String, bool>,
    ready: VecDeque<(String, Reader)>,
}

/// The same lock linearizes capture admission and normal completion.
#[derive(Clone, Default)]
pub(super) struct Captures {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    consumer: Arc<tokio::sync::Mutex<()>>,
}

impl Captures {
    #[cfg(test)]
    pub(super) fn admit(&self, id: &str) -> Result<(), PortError> {
        self.admit_many(&[id])
    }

    pub(super) fn admit_many(&self, ids: &[&str]) -> Result<(), PortError> {
        let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
        if state.sealed
            || ids.iter().any(|id| state.pending.contains_key(*id))
            || ids
                .iter()
                .enumerate()
                .any(|(index, id)| ids[..index].contains(id))
        {
            return Err(PortError::Conflict);
        }
        if state.admitted + ids.len() > MAX_CAPTURES {
            return Err(PortError::Invalid);
        }
        for id in ids {
            state.pending.insert((*id).into(), false);
        }
        state.admitted += ids.len();
        Ok(())
    }

    pub(super) fn publish(&self, id: String, reader: Reader) -> Result<(), PortError> {
        let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
        if !state.pending.contains_key(&id) || state.ready.iter().any(|(key, _)| key == &id) {
            return Err(PortError::Conflict);
        }
        state.ready.push_back((id, reader));
        self.changed.notify_one();
        Ok(())
    }

    pub(super) fn close(&self, id: &str) -> Result<(), PortError> {
        let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
        let closed = state.pending.get_mut(id).ok_or(PortError::Invalid)?;
        if *closed {
            return Err(PortError::Conflict);
        }
        *closed = true;
        Ok(())
    }

    /// Acknowledge only after the closed capture has been consumed and submitted.
    pub(super) fn acknowledge(&self, id: &str) -> Result<(), PortError> {
        let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
        if state.pending.get(id) != Some(&true) || state.ready.iter().any(|(key, _)| key == id) {
            return Err(PortError::Conflict);
        }
        state.pending.remove(id);
        Ok(())
    }

    /// Successful sealing prevents later captures; admitted captures must resolve first.
    pub(super) fn seal(&self) -> Result<bool, PortError> {
        let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
        if !state.pending.is_empty() {
            return Ok(false);
        }
        state.sealed = true;
        self.changed.notify_waiters();
        Ok(true)
    }

    pub(super) fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.sealed = true;
        state.ready.clear();
        state.pending.clear();
        self.changed.notify_waiters();
    }

    pub(super) async fn next(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<(String, Reader)>, PortError> {
        let _consumer = self.consumer.try_lock().map_err(|_| PortError::Conflict)?;
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self.state.lock().map_err(|_| PortError::Uncertain)?;
                if cancel.is_cancelled() {
                    return Err(PortError::Unavailable);
                }
                if let Some(capture) = state.ready.pop_front() {
                    return Ok(Some(capture));
                }
                if state.sealed {
                    return Ok(None);
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return Err(PortError::Unavailable),
                () = changed => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_cannot_overtake_an_admitted_capture() {
        let captures = Captures::default();
        captures.admit("first").unwrap();
        assert!(!captures.seal().unwrap());
        assert!(captures.acknowledge("first").is_err());
        captures.close("first").unwrap();
        assert!(!captures.seal().unwrap());
        captures.acknowledge("first").unwrap();
        assert!(captures.seal().unwrap());
        assert!(captures.admit("late").is_err());
    }

    #[test]
    fn capture_admission_is_finite_and_close_is_explicit() {
        let captures = Captures::default();
        for index in 0..MAX_CAPTURES {
            let id = index.to_string();
            captures.admit(&id).unwrap();
            assert!(captures.admit(&id).is_err());
            captures.close(&id).unwrap();
            assert!(captures.close(&id).is_err());
            captures.acknowledge(&id).unwrap();
        }
        assert!(captures.admit("overflow").is_err());
    }

    #[tokio::test]
    async fn cancellation_and_sealing_wake_input_waiters() {
        let captures = Captures::default();
        let cancel = CancellationToken::new();
        let wait = captures.next(&cancel);
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        captures.seal().unwrap();
        assert!(wait.await.unwrap().is_none());
        cancel.cancel();
        assert!(captures.next(&cancel).await.is_err());

        let captures = Captures::default();
        let cancel = CancellationToken::new();
        let wait = captures.next(&cancel);
        tokio::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        cancel.cancel();
        assert!(wait.await.is_err());
    }
}
