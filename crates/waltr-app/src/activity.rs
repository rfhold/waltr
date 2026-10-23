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
    active: usize,
    failed: bool,
    error_until: Option<Instant>,
}

#[derive(Clone, Default)]
pub(crate) struct ActivityTracker(Arc<Mutex<State>>);

impl ActivityTracker {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
