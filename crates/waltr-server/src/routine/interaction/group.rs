use super::Error;
use crate::resource_catalog::routine::{RoutineWorkGroup, RoutineWorkGroupBusy};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

struct State {
    generation: u64,
    active: Option<(u64, CancellationToken)>,
    latest: Option<CancellationToken>,
}
pub(super) struct Group {
    policy: RoutineWorkGroupBusy,
    state: Mutex<State>,
    gate: Arc<Mutex<()>>,
}
pub(super) struct Lease {
    group: Arc<Group>,
    generation: u64,
    pub(super) token: CancellationToken,
    invalidated: CancellationToken,
}
impl Lease {
    pub(super) fn fence(&self) -> super::stream::Fence {
        super::stream::Fence {
            token: self.invalidated.clone(),
            gate: self.group.gate.clone(),
        }
    }
}
impl Group {
    pub(super) fn new(spec: &RoutineWorkGroup) -> Arc<Self> {
        debug_assert_ne!(spec.on_busy, RoutineWorkGroupBusy::Queue);
        Arc::new(Self {
            policy: spec.on_busy,
            state: Mutex::new(State {
                generation: 0,
                active: None,
                latest: None,
            }),
            gate: Arc::new(Mutex::new(())),
        })
    }
    pub(super) async fn enter(
        self: &Arc<Self>,
        parent: &CancellationToken,
    ) -> Result<Lease, Error> {
        let token = parent.child_token();
        let invalidated = CancellationToken::new();
        let generation = {
            let mut state = self.state.lock().expect("group lock");
            if state.active.is_some() && self.policy == RoutineWorkGroupBusy::Reject {
                return Err(Error::new(
                    "work_group_busy",
                    "work group rejects overlapping work",
                ));
            }
            state.generation += 1;
            let generation = state.generation;
            if self.policy == RoutineWorkGroupBusy::Interrupt {
                let _publication = self.gate.lock().expect("publication gate");
                if let Some(previous) = state.latest.replace(invalidated.clone()) {
                    previous.cancel();
                }
                if let Some((_, old)) = state.active.take() {
                    old.cancel();
                }
            }
            state.active = Some((generation, token.clone()));
            generation
        };
        Ok(Lease {
            group: self.clone(),
            generation,
            token,
            invalidated,
        })
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.group.state.lock().expect("group lock");
        if state
            .active
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            state.active = None;
        }
    }
}
