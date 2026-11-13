use super::{Error, Guard, capture};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

struct State {
    release: CancellationToken,
    count: u8,
    outstanding: u8,
    sealed: bool,
}

/// Process-local admission, not a server workflow/session policy.
pub(crate) struct RepeatedCaptures {
    state: Mutex<State>,
    requests: tokio::sync::mpsc::Sender<CancellationToken>,
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<CancellationToken>>,
    sealed: CancellationToken,
    monitor: Mutex<Option<Monitor>>,
}
struct Monitor {
    release: CancellationToken,
    stop: Arc<std::sync::atomic::AtomicU8>,
    started: Arc<Mutex<Option<std::time::Instant>>>,
    failure: Arc<Mutex<Option<Result<(), capture::Error>>>>,
    requested: std::time::Instant,
    duration: Option<std::time::Duration>,
}
impl std::fmt::Debug for RepeatedCaptures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepeatedCaptures").finish_non_exhaustive()
    }
}
impl RepeatedCaptures {
    pub(crate) fn new(release: CancellationToken) -> Self {
        let (requests, receiver) = tokio::sync::mpsc::channel(15);
        Self {
            state: Mutex::new(State {
                release,
                count: 1,
                outstanding: 1,
                sealed: false,
            }),
            requests,
            receiver: tokio::sync::Mutex::new(receiver),
            sealed: CancellationToken::new(),
            monitor: Mutex::new(None),
        }
    }
    pub(crate) fn watch(
        &self,
        capture: &capture::CaptureWorker,
        release: CancellationToken,
        duration: Option<std::time::Duration>,
    ) {
        *self
            .monitor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Monitor {
            release,
            stop: capture.stop_lane(),
            started: capture.started_lane(),
            failure: capture.failure_lane(),
            requested: std::time::Instant::now(),
            duration,
        });
    }
    pub(crate) fn poll(&self) -> Result<(), Error> {
        let monitor_owner = self
            .monitor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(monitor) = monitor_owner.as_ref() {
            let failure = *monitor
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(Err(error)) = failure {
                return Err(Error::Capture(error));
            }
            let expired = monitor.duration.is_some_and(|duration| {
                monitor
                    .started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .unwrap_or(monitor.requested)
                    .elapsed()
                    >= duration
            });
            if monitor.release.is_cancelled() || expired {
                let _result = monitor.stop.compare_exchange(
                    0,
                    1,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                );
            }
        }
        drop(monitor_owner);
        Ok(())
    }
    pub(crate) fn press(&self) -> Result<CancellationToken, Error> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.sealed || state.count >= 16 || !state.release.is_cancelled() {
            return Err(Error::Busy);
        }
        let release = CancellationToken::new();
        self.requests
            .try_send(release.clone())
            .map_err(|_| Error::Busy)?;
        state.release = release.clone();
        state.count += 1;
        state.outstanding += 1;
        drop(state);
        Ok(release)
    }
    /// False means a physical press overlapped server sealing; surface it as a failure.
    pub(crate) fn seal(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.sealed = true;
        self.sealed.cancel();
        state.outstanding == 0
    }
    pub(crate) fn closed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outstanding = state.outstanding.saturating_sub(1);
    }
    pub(crate) async fn next(&self) -> Option<CancellationToken> {
        let mut receiver = self.receiver.lock().await;
        tokio::select! { biased;
            () = self.sealed.cancelled() => None,
            release = receiver.recv() => release,
        }
    }
    pub(crate) fn pending(&self) -> Option<CancellationToken> {
        self.receiver.try_lock().ok()?.try_recv().ok()
    }
}

/// Each future borrows the outer worker, so cancellation cannot detach native cleanup.
#[allow(
    clippy::future_not_send,
    reason = "the effect guard belongs to the native LocalSet"
)]
pub(super) async fn frame(
    capture: &mut capture::CaptureWorker,
    release: &CancellationToken,
    duration: Option<std::time::Duration>,
    guard: &Guard<'_>,
    lane: &RepeatedCaptures,
) -> Result<Option<Vec<u8>>, Error> {
    loop {
        guard.check().map_err(|_| Error::Cancelled)?;
        if lane.sealed.is_cancelled() {
            return Err(Error::Interaction);
        }
        let expired = duration.is_some_and(|duration| {
            capture
                .started_lane()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some_and(|start| start.elapsed() >= duration)
        });
        if release.is_cancelled() || expired {
            capture.stop();
        }
        tokio::select! { biased;
            frame = capture.frames.recv() => return Ok(frame),
            () = tokio::time::sleep(std::time::Duration::from_millis(2)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_bounded_and_each_press_requires_its_own_release() {
        let release = CancellationToken::new();
        let lane = RepeatedCaptures::new(release.clone());
        assert!(lane.press().is_err());
        release.cancel();
        for _ in 1..16 {
            let release = lane.press().unwrap();
            assert!(!release.is_cancelled());
            assert!(lane.press().is_err());
            release.cancel();
        }
        assert!(
            lane.press().is_err(),
            "16 total captures, including initial capture"
        );
        assert!(
            !lane.seal(),
            "unprocessed captures cannot silently disappear at sealing"
        );
        assert!(lane.press().is_err());
    }
    #[test]
    fn seal_after_last_close_is_atomic_with_new_capture_admission() {
        let release = CancellationToken::new();
        let lane = RepeatedCaptures::new(release.clone());
        release.cancel();
        lane.closed();
        assert!(lane.seal());
        assert!(lane.press().is_err());
    }
}
