//! Bounded shortcut signals. Overflow invalidates the backend generation immediately;
//! hosts use `Receiver::failure_token` as their operation generation cancellation parent.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod wayland;

/// Secret-free backend failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Invalid portable key or backend mapping.
    Invalid,
    /// Must construct and destroy on the macOS main thread.
    MainThread,
    /// Another process-wide desktop registrar already owns the dispatcher.
    Busy,
    /// Queue overflow; a release may have been lost, so all work must cancel.
    Overflow,
    /// Unsupported portal, denied permission, closed session, or native failure.
    Backend,
}
/// One normalized physical edge. Repeated presses are suppressed by Router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Config binding ID.
    pub id: String,
    /// True for press, false for release.
    pub pressed: bool,
}
/// A physical edge stamped at backend ingress, never at receiver dequeue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochEvent {
    /// Opaque ingress revision; releases retain the revision of their physical press.
    pub epoch: u64,
    /// Normalized physical edge.
    pub event: Event,
}
#[derive(Debug, Default)]
struct EpochState {
    owner: u64,
    revision: u64,
    exhausted: bool,
}
#[derive(Debug, Default)]
struct EpochInner {
    state: Mutex<EpochState>,
    published: AtomicU64,
}
/// Host-owned ingress fence. A fresh fence denies admission until explicitly enabled.
/// Odd revisions admit presses; even revisions reject them. Revisions never wrap.
#[derive(Clone, Debug, Default)]
pub struct IngressEpoch(Arc<EpochInner>);
impl IngressEpoch {
    /// Replace the host lease and invalidate every previously queued edge before mutation.
    pub fn bind(&self, owner: u64) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.owner = owner;
        if let Some(next) = state.revision.checked_add(2) {
            state.revision = next & !1;
        } else {
            state.revision = u64::MAX - 1;
            state.exhausted = true;
        }
        self.0.published.store(state.revision, Ordering::Release);
    }
    /// Publish eligibility only for the exact lease. Disable before revocation; enable after setup.
    /// Changing eligibility invalidates all queued edges, including same-lease access transitions.
    pub fn set_enabled(&self, owner: u64, enabled: bool) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.exhausted || state.owner != owner || state.revision % 2 == u64::from(enabled) {
            return;
        }
        if let Some(next) = state.revision.checked_add(1) {
            state.revision = next;
        } else {
            state.revision = u64::MAX - 1;
            state.exhausted = true;
        }
        self.0.published.store(state.revision, Ordering::Release);
    }
    /// Execute admission under the same lock as lease/eligibility replacement.
    /// Ingress itself only loads the published revision and never blocks a native callback.
    pub fn admit<T>(&self, epoch: u64, effect: impl FnOnce() -> T) -> Option<T> {
        let state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = (state.revision == epoch && epoch % 2 == 1).then(effect);
        drop(state);
        result
    }
}

/// Effective registration after backend/user decisions, without command/audio values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    /// Stable config ID.
    pub id: String,
    /// Actual user-visible trigger, not the requested hint.
    pub trigger: String,
}
#[derive(Debug)]
struct State {
    effective: BTreeMap<String, String>,
    down: Vec<(String, u64)>,
    failure: Option<Error>,
}
/// Injectable bounded signal router shared by native backends and synthetic tests.
#[derive(Clone, Debug)]
pub struct Router {
    state: Arc<Mutex<State>>,
    tx: mpsc::Sender<EpochEvent>,
    epoch: IngressEpoch,
    failed: CancellationToken,
}
/// Single host-owned event lane; no tasks or runtimes are spawned.
#[derive(Debug)]
pub struct Receiver {
    router: Router,
    rx: mpsc::Receiver<EpochEvent>,
}
impl Router {
    /// Create a fixed 64-edge queue and independent terminal lane.
    #[must_use]
    pub fn channel() -> (Self, Receiver) {
        let epoch = IngressEpoch::default();
        epoch.set_enabled(0, true);
        Self::channel_with_epoch(epoch)
    }
    /// Create the same bounded edge lane with a host-owned ingress fence.
    #[must_use]
    pub fn channel_with_epoch(epoch: IngressEpoch) -> (Self, Receiver) {
        let (tx, rx) = mpsc::channel(64);
        let router = Self {
            state: Arc::new(Mutex::new(State {
                effective: BTreeMap::new(),
                down: Vec::new(),
                failure: None,
            })),
            tx,
            epoch,
            failed: CancellationToken::new(),
        };
        (router.clone(), Receiver { router, rx })
    }
    /// Replace actual accepted bindings; enforce finite bounded state.
    /// # Errors
    /// Rejects duplicate, oversized, or excessive entries and invalidates active work.
    pub fn effective(&self, bindings: Vec<Effective>) -> Result<(), Error> {
        if bindings.len() > 64
            || bindings
                .iter()
                .any(|b| b.id.is_empty() || b.id.len() > 256 || b.trigger.len() > 256)
        {
            self.fail(Error::Invalid);
            return Err(Error::Invalid);
        }
        let count = bindings.len();
        let effective: BTreeMap<_, _> = bindings.into_iter().map(|b| (b.id, b.trigger)).collect();
        if effective.len() != count {
            self.fail(Error::Invalid);
            return Err(Error::Invalid);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A rebind while down cannot safely preserve the physical release association.
        if !state.down.is_empty() {
            drop(state);
            self.fail(Error::Backend);
            return Err(Error::Backend);
        }
        state.effective = effective;
        drop(state);
        Ok(())
    }
    /// Route a signal only for the exact session and registered ID. Never block callbacks.
    pub fn edge(&self, expected_session: &str, session: &str, id: &str, pressed: bool) {
        // Linearize ingress before filtering or touching the bounded physical-down list.
        let ingress = self.epoch.0.published.load(Ordering::Acquire);
        if expected_session != session {
            return;
        }
        let Ok(mut state) = self.state.try_lock() else {
            self.failed.cancel();
            return;
        };
        if state.failure.is_some() || !state.effective.contains_key(id) {
            return;
        }
        // Retain the press stamp through key-up, even if the host lease changes while held.
        let epoch = if pressed {
            if state.down.iter().any(|(down, _)| down == id) {
                return;
            }
            state.down.push((id.into(), ingress));
            ingress
        } else {
            let Some(index) = state.down.iter().position(|(down, _)| down == id) else {
                return;
            };
            state.down.swap_remove(index).1
        };
        if self
            .tx
            .try_send(EpochEvent {
                epoch,
                event: Event {
                    id: id.into(),
                    pressed,
                },
            })
            .is_err()
        {
            state.failure = Some(Error::Overflow);
            drop(state);
            self.failed.cancel();
        }
    }
    /// Independent generation token for backend drivers and active holds.
    #[must_use]
    pub fn failure_token(&self) -> CancellationToken {
        self.failed.clone()
    }
    /// Terminal failure bypasses the finite event queue and cancels the generation.
    pub fn fail(&self, error: Error) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure = Some(error);
        self.failed.cancel();
    }
}
impl Receiver {
    /// Parent token for active holds. It cancels even if the event queue is full.
    #[must_use]
    pub fn failure_token(&self) -> CancellationToken {
        self.router.failed.clone()
    }
    /// Current finite effective bindings; never exposes local commands or text.
    #[must_use]
    pub fn effective(&self) -> Vec<Effective> {
        self.router
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .effective
            .iter()
            .map(|(id, trigger)| Effective {
                id: id.clone(),
                trigger: trigger.clone(),
            })
            .collect()
    }
    /// Independent static terminal error.
    #[must_use]
    pub fn failure(&self) -> Option<Error> {
        self.router
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure
    }
    /// Await one edge or terminal failure. Failure takes priority over stale queued events.
    /// # Errors
    /// Returns a terminal backend error.
    pub async fn next(&mut self) -> Result<Event, Error> {
        self.next_epoch().await.map(|edge| edge.event)
    }
    /// Await an ingress-stamped edge. The host must fence admission and match release epochs.
    /// # Errors
    /// Returns terminal failure before any queued event, just like `next`.
    pub async fn next_epoch(&mut self) -> Result<EpochEvent, Error> {
        tokio::select! { biased;
            () = self.router.failed.cancelled() => Err(self.failure().unwrap_or(Error::Backend)),
            event = self.rx.recv() => event.ok_or(Error::Backend),
        }
    }
}
/// Normalize strict config chords to XDG preferred-trigger syntax.
/// # Errors
/// Rejects invalid portable syntax. Portal override remains authoritative.
pub fn portal_trigger(key: &str) -> Result<String, Error> {
    crate::config::validate_chord(key).map_err(|_| Error::Invalid)?;
    Ok(key
        .split('+')
        .map(|part| match part {
            "Ctrl" => "CTRL",
            "Alt" => "ALT",
            "Shift" => "SHIFT",
            "Super" => "LOGO",
            "Enter" => "Return",
            "Escape" => "Escape",
            "Space" => "space",
            "Backspace" => "BackSpace",
            "PageUp" => "Prior",
            "PageDown" => "Next",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("+"))
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
