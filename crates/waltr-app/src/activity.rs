//! Text-free, process-owned accounting for admitted client operations.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ERROR_DURATION: Duration = Duration::from_secs(2);

#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Working,
    Error,
}

#[derive(Default)]
struct State {
    generation: u64,
    active: usize,
    failed: bool,
    error_until: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct ActivityTracker(Arc<Inner>);
struct Inner {
    state: Mutex<State>,
    failure: tokio::sync::watch::Sender<Option<AudioFailure>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioFailure {
    pub diagnostic_id: u64,
    pub category: &'static str,
}
impl Default for ActivityTracker {
    fn default() -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            failure: tokio::sync::watch::channel(None).0,
        }))
    }
}

impl ActivityTracker {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn bind(&self, generation: u64) {
        let mut state = self.lock();
        if state.generation == generation {
            return;
        }
        state.generation = generation;
        self.0.failure.send_replace(None);
    }
    pub(crate) fn failures(&self) -> tokio::sync::watch::Receiver<Option<AudioFailure>> {
        self.0.failure.subscribe()
    }
    pub(crate) fn complete(
        &self,
        generation: u64,
        completion: crate::native_audio_telemetry::Completion,
    ) {
        let state = self.lock();
        if state.generation != generation {
            return;
        }
        match completion {
            crate::native_audio_telemetry::Completion::Failed(failure) => {
                self.0.failure.send_replace(Some(failure));
            }
            crate::native_audio_telemetry::Completion::Succeeded => {
                self.0.failure.send_replace(None);
            }
            crate::native_audio_telemetry::Completion::Unchanged => {}
        }
    }
    pub(crate) fn dismiss(&self, generation: u64, diagnostic_id: u64) {
        let state = self.lock();
        if state.generation == generation
            && self
                .0
                .failure
                .borrow()
                .is_some_and(|failure| failure.diagnostic_id == diagnostic_id)
        {
            self.0.failure.send_replace(None);
        }
    }

    pub(crate) fn begin(&self) -> ActivityLease {
        let mut state = self.lock();
        state.active = state
            .active
            .checked_add(1)
            .expect("activity count overflow");
        state.error_until = None;
        drop(state);
        ActivityLease {
            tracker: self.clone(),
            failed: false,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(crate) fn phase(&self, now: Instant) -> Phase {
        let state = self.lock();
        if state.active > 0 {
            Phase::Working
        } else if state.error_until.is_some_and(|until| now < until) {
            Phase::Error
        } else {
            Phase::Idle
        }
    }

    fn finish(&self, failed: bool, now: Instant) {
        let mut state = self.lock();
        state.failed |= failed;
        state.active -= 1;
        if state.active == 0 && state.failed {
            state.error_until = Some(now + ERROR_DURATION);
            state.failed = false;
        }
    }
}

pub(crate) struct ActivityLease {
    tracker: ActivityTracker,
    failed: bool,
}
impl ActivityLease {
    pub(crate) const fn fail(&mut self) {
        self.failed = true;
    }
}
impl Drop for ActivityLease {
    fn drop(&mut self) {
        self.tracker.finish(self.failed, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_failure_same_generation_rebind_preserves_failure_and_dismissal() {
        use crate::native_audio_telemetry::Completion;
        let tracker = ActivityTracker::default();
        let mut receiver = tracker.failures();
        tracker.bind(0);
        assert!(!receiver.has_changed().unwrap());
        assert_eq!(*receiver.borrow_and_update(), None);
        tracker.complete(
            0,
            Completion::Failed(AudioFailure {
                diagnostic_id: 42,
                category: "audio_failed",
            }),
        );
        let failure = *receiver.borrow_and_update();
        tracker.bind(0);
        assert_eq!(*tracker.failures().borrow(), failure);
        assert!(!receiver.has_changed().unwrap());
        tracker.dismiss(0, 42);
        assert!(receiver.has_changed().unwrap());
        assert_eq!(*receiver.borrow_and_update(), None);
    }

    #[test]
    fn audio_failure_new_generation_rejects_old_completions_and_lease_cleanup() {
        use crate::native_audio_telemetry::Completion;
        let tracker = ActivityTracker::default();
        tracker.bind(7);
        let mut old = tracker.begin();
        tracker.complete(
            7,
            Completion::Failed(AudioFailure {
                diagnostic_id: 41,
                category: "audio_failed",
            }),
        );
        tracker.bind(8);
        assert_eq!(*tracker.failures().borrow(), None);
        let failure = AudioFailure {
            diagnostic_id: 42,
            category: "remote_failed",
        };
        tracker.complete(8, Completion::Failed(failure));
        old.fail();
        drop(old);
        tracker.complete(7, Completion::Succeeded);
        tracker.dismiss(7, 42);
        tracker.bind(8);
        assert_eq!(*tracker.failures().borrow(), Some(failure));
    }

    #[test]
    fn audio_failure_is_persistent_dismissible_and_generation_scoped() {
        use crate::native_audio_telemetry::Completion;
        let tracker = ActivityTracker::default();
        tracker.bind(7);
        let receiver = tracker.failures();
        let failure = AudioFailure {
            diagnostic_id: 42,
            category: "audio_failed",
        };
        tracker.complete(7, Completion::Failed(failure));
        assert_eq!(*receiver.borrow(), Some(failure));
        tracker.complete(7, Completion::Unchanged);
        tracker.dismiss(6, 42);
        tracker.dismiss(7, 41);
        assert_eq!(*receiver.borrow(), Some(failure));
        tracker.dismiss(7, 42);
        assert_eq!(*receiver.borrow(), None);
        tracker.complete(7, Completion::Failed(failure));
        tracker.complete(7, Completion::Succeeded);
        assert_eq!(*receiver.borrow(), None);
        tracker.complete(7, Completion::Failed(failure));
        tracker.bind(8);
        tracker.complete(7, Completion::Failed(failure));
        assert_eq!(*receiver.borrow(), None);
    }

    #[test]
    fn overlap_and_old_completion_cannot_clear_new_work() {
        let tracker = ActivityTracker::default();
        let old = tracker.begin();
        let new = tracker.begin();
        drop(old);
        assert_eq!(tracker.lock().active, 1);
        assert_eq!(tracker.phase(Instant::now()), Phase::Working);
        drop(new);
        assert_eq!(tracker.phase(Instant::now()), Phase::Idle);
    }

    #[test]
    fn failure_waits_for_cleanup_of_all_work_and_expires_on_clock() {
        let tracker = ActivityTracker::default();
        let now = Instant::now();
        // Exercise the same transition as lease Drop with an injected clock.
        tracker.lock().active = 2;
        tracker.finish(true, now);
        assert_eq!(tracker.phase(now), Phase::Working);
        tracker.finish(false, now + Duration::from_secs(5));
        assert_eq!(tracker.phase(now + Duration::from_secs(6)), Phase::Error);
        assert_eq!(tracker.phase(now + Duration::from_secs(7)), Phase::Idle);
        let cancel = tracker.begin();
        assert_eq!(tracker.phase(now), Phase::Working);
        drop(cancel);
        assert_eq!(tracker.phase(Instant::now()), Phase::Idle);
    }

    #[test]
    fn normal_drop_is_cancellation_not_failure() {
        let tracker = ActivityTracker::default();
        drop(tracker.begin());
        assert_eq!(tracker.phase(Instant::now()), Phase::Idle);
        let mut failed = tracker.begin();
        failed.fail();
        drop(failed);
        assert_eq!(tracker.phase(Instant::now()), Phase::Error);
        drop(tracker.begin());
        assert_eq!(tracker.phase(Instant::now()), Phase::Idle);
    }
}
