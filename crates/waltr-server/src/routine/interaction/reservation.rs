use super::Error;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct Waiting {
    id: u64,
    resources: BTreeSet<String>,
    ready: oneshot::Sender<()>,
}
#[derive(Default)]
struct State {
    next: u64,
    active: BTreeMap<u64, BTreeSet<String>>,
    waiting: VecDeque<Waiting>,
}
/// Queue resources are acquired atomically by a whole live component, never by its members.
/// Conflicting components are FIFO; disjoint components can progress independently.
pub(super) struct Reservations {
    limits: BTreeMap<String, usize>,
    state: Mutex<State>,
}
pub(super) struct Reservation {
    manager: Arc<Reservations>,
    id: u64,
}
impl Reservations {
    pub(super) fn new(limits: BTreeMap<String, usize>) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: Mutex::new(State::default()),
        })
    }
    pub(super) async fn enter(
        self: &Arc<Self>,
        resources: BTreeSet<String>,
        cancel: &CancellationToken,
    ) -> Result<Reservation, Error> {
        if cancel.is_cancelled() {
            return Err(Error::cancelled());
        }
        let (id, wait) = {
            let mut state = self.state.lock().expect("component reservations");
            state.waiting.retain(|w| !w.ready.is_closed());
            let busy = state.active.values().any(|r| !r.is_disjoint(&resources))
                || state
                    .waiting
                    .iter()
                    .any(|w| !w.resources.is_disjoint(&resources));
            if busy
                && resources.iter().any(|r| {
                    state
                        .waiting
                        .iter()
                        .filter(|w| w.resources.contains(r))
                        .count()
                        >= self.limits[r]
                })
            {
                return Err(Error::new("resource_exhausted", "work group queue is full"));
            }
            state.next += 1;
            let id = state.next;
            if busy {
                let (ready, wait) = oneshot::channel();
                state.waiting.push_back(Waiting {
                    id,
                    resources,
                    ready,
                });
                (id, Some(wait))
            } else {
                state.active.insert(id, resources);
                (id, None)
            }
        };
        let lease = Reservation {
            manager: self.clone(),
            id,
        };
        if let Some(wait) = wait {
            tokio::select! { biased;
                () = cancel.cancelled() => return Err(Error::cancelled()),
                result = wait => result.map_err(|_| Error::cancelled())?,
            }
        }
        Ok(lease)
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.manager.state.lock().expect("component reservations");
        state.active.remove(&self.id);
        state
            .waiting
            .retain(|w| w.id != self.id && !w.ready.is_closed());
        let mut blocked = BTreeSet::new();
        let mut index = 0;
        while index < state.waiting.len() {
            let waiting = &state.waiting[index];
            if !waiting.resources.is_disjoint(&blocked)
                || state
                    .active
                    .values()
                    .any(|r| !r.is_disjoint(&waiting.resources))
            {
                blocked.extend(waiting.resources.clone());
                index += 1;
            } else {
                let waiting = state.waiting.remove(index).expect("queued component");
                if waiting.ready.send(()).is_ok() {
                    state.active.insert(waiting.id, waiting.resources);
                }
            }
        }
    }
}
