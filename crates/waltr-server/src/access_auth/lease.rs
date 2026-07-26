use std::{
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
    time::{Duration, SystemTime},
};

use tokio::time::{Instant, Sleep};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use super::{AccessAuthStore, BrowserSessionTokenDigest};

const REVALIDATION_INTERVAL: Duration = Duration::from_secs(1);
const REVALIDATION_LEAD_TIME: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(super) enum PrincipalLease {
    Unbounded,
    Bearer {
        expires_at: SystemTime,
    },
    Browser {
        token_digest: BrowserSessionTokenDigest,
        issuer: String,
        client_id: String,
        expires_at: SystemTime,
        store: AccessAuthStore,
        revocation_signal: Arc<RevocationSignal>,
    },
}

impl fmt::Debug for PrincipalLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unbounded => formatter.write_str("Unbounded"),
            Self::Bearer { expires_at } => formatter
                .debug_struct("Bearer")
                .field("expires_at", expires_at)
                .finish(),
            Self::Browser {
                issuer,
                client_id,
                expires_at,
                ..
            } => formatter
                .debug_struct("Browser")
                .field("issuer", issuer)
                .field("client_id", client_id)
                .field("expires_at", expires_at)
                .finish(),
        }
    }
}

impl PrincipalLease {
    pub(super) const fn bearer_expires_at(&self) -> Option<SystemTime> {
        match self {
            Self::Bearer { expires_at } => Some(*expires_at),
            Self::Unbounded | Self::Browser { .. } => None,
        }
    }

    pub(super) fn monitor(&self) -> Option<LeaseMonitor> {
        match self {
            Self::Unbounded => None,
            Self::Bearer { expires_at } => Some(LeaseMonitor::bearer(*expires_at)),
            Self::Browser {
                token_digest,
                expires_at,
                store,
                revocation_signal,
                ..
            } => Some(LeaseMonitor::browser(
                *expires_at,
                token_digest.clone(),
                Arc::new(store.clone()),
                Arc::clone(revocation_signal),
            )),
        }
    }
}

#[derive(Debug)]
pub(super) struct RevocationSignal {
    token: CancellationToken,
}

impl RevocationSignal {
    pub(super) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    fn cancel(&self) {
        self.token.cancel();
    }

    #[cfg(test)]
    pub(super) fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

#[derive(Debug, Default)]
pub(super) struct RevocationRegistry {
    signals: Mutex<HashMap<BrowserSessionTokenDigest, Weak<RevocationSignal>>>,
}

impl RevocationRegistry {
    pub(super) fn signal(&self, token_digest: &BrowserSessionTokenDigest) -> Arc<RevocationSignal> {
        let mut signals = self
            .signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(signal) = signals.get(token_digest).and_then(Weak::upgrade) {
            return signal;
        }
        signals.retain(|_, signal| signal.strong_count() > 0);
        let signal = Arc::new(RevocationSignal::new());
        signals.insert(token_digest.clone(), Arc::downgrade(&signal));
        signal
    }

    pub(super) fn cancel(&self, token_digest: &BrowserSessionTokenDigest) {
        let signal = self
            .signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(token_digest)
            .and_then(Weak::upgrade);
        if let Some(signal) = signal {
            signal.cancel();
        }
    }

    #[cfg(test)]
    pub(super) fn live_signal(
        &self,
        token_digest: &BrowserSessionTokenDigest,
    ) -> Option<Arc<RevocationSignal>> {
        self.signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(token_digest)
            .and_then(Weak::upgrade)
    }
}

pub(super) type RevalidationFuture = Pin<Box<dyn Future<Output = Result<bool, ()>> + Send>>;

pub(super) trait BrowserSessionRevalidator: fmt::Debug + Send + Sync {
    fn is_active(&self, token_digest: BrowserSessionTokenDigest) -> RevalidationFuture;
}

impl BrowserSessionRevalidator for AccessAuthStore {
    fn is_active(&self, token_digest: BrowserSessionTokenDigest) -> RevalidationFuture {
        let store = self.clone();
        Box::pin(async move {
            store
                .is_browser_session_active(&token_digest, SystemTime::now())
                .await
                .map_err(|_| ())
        })
    }
}

pub(super) enum LeaseMonitor {
    Bearer {
        deadline: Instant,
        expiry: Pin<Box<Sleep>>,
    },
    Browser {
        token_digest: BrowserSessionTokenDigest,
        revalidator: Arc<dyn BrowserSessionRevalidator>,
        expiry_deadline: Instant,
        expiry: Pin<Box<Sleep>>,
        cancellation: Pin<Box<WaitForCancellationFutureOwned>>,
        _revocation_signal: Arc<RevocationSignal>,
        freshness_deadline: Instant,
        freshness: Pin<Box<Sleep>>,
        next_query_deadline: Instant,
        next_query: Pin<Box<Sleep>>,
        pending_revalidation: Option<RevalidationFuture>,
    },
}

impl fmt::Debug for LeaseMonitor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => formatter.write_str("LeaseMonitor::Bearer"),
            Self::Browser { .. } => formatter.write_str("LeaseMonitor::Browser"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LeaseEnd {
    Unauthenticated,
    Unavailable,
}

impl LeaseMonitor {
    pub(super) fn bearer(expires_at: SystemTime) -> Self {
        let deadline = monotonic_deadline(expires_at);
        Self::Bearer {
            deadline,
            expiry: Box::pin(tokio::time::sleep_until(deadline)),
        }
    }

    pub(super) fn browser(
        expires_at: SystemTime,
        token_digest: BrowserSessionTokenDigest,
        revalidator: Arc<dyn BrowserSessionRevalidator>,
        revocation_signal: Arc<RevocationSignal>,
    ) -> Self {
        let now = Instant::now();
        let expiry_deadline = monotonic_deadline_at(expires_at, now, SystemTime::now());
        let freshness_deadline = now + REVALIDATION_INTERVAL;
        let next_query_deadline = now + REVALIDATION_LEAD_TIME;
        Self::Browser {
            token_digest,
            revalidator,
            expiry_deadline,
            expiry: Box::pin(tokio::time::sleep_until(expiry_deadline)),
            cancellation: Box::pin(revocation_signal.token.clone().cancelled_owned()),
            _revocation_signal: revocation_signal,
            freshness_deadline,
            freshness: Box::pin(tokio::time::sleep_until(freshness_deadline)),
            next_query_deadline,
            next_query: Box::pin(tokio::time::sleep_until(next_query_deadline)),
            pending_revalidation: None,
        }
    }

    pub(super) fn poll(&mut self, context: &mut Context<'_>) -> Poll<LeaseEnd> {
        match self {
            Self::Bearer { deadline, expiry } => {
                if Instant::now() >= *deadline {
                    return Poll::Ready(LeaseEnd::Unauthenticated);
                }
                expiry
                    .as_mut()
                    .poll(context)
                    .map(|()| LeaseEnd::Unauthenticated)
            }
            Self::Browser {
                token_digest,
                revalidator,
                expiry_deadline,
                expiry,
                cancellation,
                _revocation_signal: _,
                freshness_deadline,
                freshness,
                next_query_deadline,
                next_query,
                pending_revalidation,
            } => {
                let now = Instant::now();
                if now >= *expiry_deadline {
                    return Poll::Ready(LeaseEnd::Unauthenticated);
                }
                if cancellation.as_mut().poll(context).is_ready() {
                    return Poll::Ready(LeaseEnd::Unauthenticated);
                }
                if expiry.as_mut().poll(context).is_ready() {
                    return Poll::Ready(LeaseEnd::Unauthenticated);
                }

                if now >= *freshness_deadline || freshness.as_mut().poll(context).is_ready() {
                    return Poll::Ready(LeaseEnd::Unavailable);
                }

                if let Some(pending) = pending_revalidation {
                    match pending.as_mut().poll(context) {
                        Poll::Ready(Ok(true)) => {
                            *pending_revalidation = None;
                            *freshness_deadline = next_query_deadline
                                .checked_add(REVALIDATION_INTERVAL)
                                .expect("revalidation deadline should be representable");
                            freshness.as_mut().reset(*freshness_deadline);
                            *next_query_deadline = next_query_deadline
                                .checked_add(REVALIDATION_LEAD_TIME)
                                .expect("revalidation schedule should be representable");
                            next_query.as_mut().reset(*next_query_deadline);
                        }
                        Poll::Ready(Ok(false)) => {
                            return Poll::Ready(LeaseEnd::Unauthenticated);
                        }
                        Poll::Ready(Err(())) => return Poll::Ready(LeaseEnd::Unavailable),
                        Poll::Pending => return Poll::Pending,
                    }
                }

                if now >= *next_query_deadline || next_query.as_mut().poll(context).is_ready() {
                    *pending_revalidation = Some(revalidator.is_active(token_digest.clone()));
                    return self.poll(context);
                }
                Poll::Pending
            }
        }
    }
}

fn monotonic_deadline(expires_at: SystemTime) -> Instant {
    monotonic_deadline_at(expires_at, Instant::now(), SystemTime::now())
}

fn monotonic_deadline_at(
    expires_at: SystemTime,
    monotonic_now: Instant,
    system_now: SystemTime,
) -> Instant {
    monotonic_now
        .checked_add(
            expires_at
                .duration_since(system_now)
                .unwrap_or(Duration::ZERO),
        )
        .unwrap_or(monotonic_now)
}

#[cfg(test)]
#[allow(
    clippy::duration_suboptimal_units,
    clippy::unchecked_time_subtraction,
    reason = "lease tests exercise exact refresh boundaries"
)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use futures_util::{future::poll_fn, task::noop_waker_ref};
    use tokio::sync::oneshot;

    use super::*;

    #[derive(Debug)]
    struct ControlledRevalidator {
        active: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        gates: Mutex<VecDeque<oneshot::Receiver<()>>>,
    }

    impl BrowserSessionRevalidator for ControlledRevalidator {
        fn is_active(&self, _token_digest: BrowserSessionTokenDigest) -> RevalidationFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gates.lock().unwrap().pop_front().unwrap();
            let active = Arc::clone(&self.active);
            Box::pin(async move {
                gate.await.map_err(|_| ())?;
                Ok(active.load(Ordering::SeqCst))
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_query_after_revocation_fails_closed_at_fixed_deadline() {
        assert_delayed_change_ends_at_fixed_deadline().await;
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_query_after_configuration_change_fails_closed_at_fixed_deadline() {
        assert_delayed_change_ends_at_fixed_deadline().await;
    }

    async fn assert_delayed_change_ends_at_fixed_deadline() {
        let active = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let (first_tx, first_rx) = oneshot::channel();
        let (_delayed_tx, delayed_rx) = oneshot::channel();
        let revalidator = Arc::new(ControlledRevalidator {
            active: Arc::clone(&active),
            calls: Arc::clone(&calls),
            gates: Mutex::new(VecDeque::from([first_rx, delayed_rx])),
        });
        let mut monitor = LeaseMonitor::browser(
            SystemTime::now() + Duration::from_secs(60),
            BrowserSessionTokenDigest::from_token("cross-instance"),
            revalidator,
            Arc::new(RevocationSignal::new()),
        );

        let task = tokio::spawn(async move { poll_fn(|context| monitor.poll(context)).await });
        tokio::task::yield_now().await;
        tokio::time::advance(REVALIDATION_LEAD_TIME).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        first_tx.send(()).unwrap();
        tokio::task::yield_now().await;

        active.store(false, Ordering::SeqCst);
        tokio::time::advance(REVALIDATION_LEAD_TIME).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        tokio::time::advance(REVALIDATION_LEAD_TIME - Duration::from_nanos(1)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_nanos(2)).await;
        assert_eq!(task.await.unwrap(), LeaseEnd::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn active_result_keeps_query_schedule_anchored_to_fixed_deadlines() {
        let active = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let (first_tx, first_rx) = oneshot::channel();
        let (_second_tx, second_rx) = oneshot::channel();
        let mut monitor = LeaseMonitor::browser(
            SystemTime::now() + Duration::from_secs(60),
            BrowserSessionTokenDigest::from_token("fixed-cadence"),
            Arc::new(ControlledRevalidator {
                active,
                calls: Arc::clone(&calls),
                gates: Mutex::new(VecDeque::from([first_rx, second_rx])),
            }),
            Arc::new(RevocationSignal::new()),
        );

        assert_eq!(poll_monitor(&mut monitor), Poll::Pending);
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(poll_monitor(&mut monitor), Poll::Pending);
        tokio::time::advance(Duration::from_millis(400)).await;
        first_tx.send(()).unwrap();
        assert_eq!(poll_monitor(&mut monitor), Poll::Pending);

        tokio::time::advance(Duration::from_millis(99)).await;
        assert_eq!(poll_monitor(&mut monitor), Poll::Pending);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(poll_monitor(&mut monitor), Poll::Pending);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn poll_monitor(monitor: &mut LeaseMonitor) -> Poll<LeaseEnd> {
        let mut context = Context::from_waker(noop_waker_ref());
        monitor.poll(&mut context)
    }
}
