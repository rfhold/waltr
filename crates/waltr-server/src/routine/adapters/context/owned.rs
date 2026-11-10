//! Bounded host-runtime ownership of atomic context calls and their joined cleanup.

use crate::routine::ports::{PortError, PortFuture};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};

#[derive(Default)]
struct State {
    closed: bool,
    calls: Vec<JoinHandle<()>>,
    cleanup: Option<JoinHandle<()>>,
    result: Option<Result<(), PortError>>,
    required_uncertain: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn required_lease_cleanup_survives_dropped_waiters_and_retains_release_failure() {
        for fail_release in [false, true] {
            let owner = ContextCalls::new();
            let (send, receive) = tokio::sync::oneshot::channel();
            let finished = Arc::new(AtomicBool::new(false));
            let observed = finished.clone();
            drop(
                owner
                    .run_required(Box::pin(async move {
                        receive.await.map_err(|_| PortError::Uncertain)?;
                        observed.store(true, Ordering::SeqCst);
                        Ok(Value::Null)
                    }))
                    .unwrap(),
            );
            let released = Arc::new(AtomicBool::new(false));
            let observed = released.clone();
            drop(owner.cleanup(Box::pin(async move {
                observed.store(true, Ordering::SeqCst);
                if fail_release {
                    Err(PortError::Unavailable)
                } else {
                    Ok(())
                }
            })));
            assert!(!released.load(Ordering::SeqCst));
            send.send(()).unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                owner.cleanup(Box::pin(async { panic!("cleanup must not run twice") })),
            )
            .await
            .unwrap();
            assert_eq!(
                result,
                if fail_release {
                    Err(PortError::Uncertain)
                } else {
                    Ok(())
                }
            );
            assert!(finished.load(Ordering::SeqCst));
            assert!(released.load(Ordering::SeqCst));
            assert!(matches!(
                owner.run_required(Box::pin(async { Ok(Value::Null) })),
                Err(PortError::Denied)
            ));
        }
    }

    #[tokio::test]
    async fn dropped_required_result_cannot_hide_uncertain_owned_work() {
        let owner = ContextCalls::new();
        drop(
            owner
                .run_required(Box::pin(async { Err(PortError::Uncertain) }))
                .unwrap(),
        );
        assert_eq!(
            owner.cleanup(Box::pin(async { Ok(()) })).await,
            Err(PortError::Uncertain)
        );
    }
}

#[derive(Clone)]
pub(super) struct ContextCalls {
    runtime: Handle,
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

impl ContextCalls {
    pub(super) fn run_required(
        &self,
        call: PortFuture<Value>,
    ) -> Result<PortFuture<Value>, PortError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().map_err(|_| PortError::Unavailable)?;
        if state.closed || state.calls.len() >= 256 {
            return Err(PortError::Denied);
        }
        let owner = self.state.clone();
        state.calls.push(self.runtime.spawn(async move {
            let result = call.await;
            if matches!(result, Err(PortError::Uncertain))
                && let Ok(mut state) = owner.lock()
            {
                state.required_uncertain = true;
            }
            drop(send.send(result));
        }));
        Ok(Box::pin(async move {
            receive.await.map_err(|_| PortError::Uncertain)?
        }))
    }
    pub(super) fn new() -> Self {
        Self {
            runtime: Handle::current(),
            state: Arc::new(Mutex::new(State::default())),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(super) fn run(&self, call: PortFuture<Value>) -> PortFuture<Value> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let registered = self
            .state
            .lock()
            .map_err(|_| PortError::Unavailable)
            .and_then(|mut state| {
                if state.closed || state.calls.len() >= 256 {
                    return Err(PortError::Denied);
                }
                let gate = self.state.clone();
                state.calls.push(self.runtime.spawn(async move {
                    let allowed = gate.lock().is_ok_and(|state| !state.closed);
                    let result = if allowed {
                        call.await
                    } else {
                        Err(PortError::Denied)
                    };
                    drop(send.send(result));
                }));
                Ok(())
            });
        Box::pin(async move {
            registered?;
            receive.await.map_err(|_| PortError::Uncertain)?
        })
    }

    pub(super) fn cleanup(&self, release: PortFuture<()>) -> PortFuture<()> {
        let this = self.clone();
        let started = self
            .state
            .lock()
            .map_err(|_| PortError::Unavailable)
            .map(|mut state| {
                if state.cleanup.is_none() {
                    state.closed = true;
                    let calls = std::mem::take(&mut state.calls);
                    let owner = this.clone();
                    state.cleanup = Some(self.runtime.spawn(async move {
                        let mut result = Ok(());
                        for call in calls {
                            if call.await.is_err() {
                                result = Err(PortError::Uncertain);
                            }
                        }
                        if release.await.is_err() {
                            result = Err(PortError::Uncertain);
                        }
                        if let Ok(mut state) = owner.state.lock() {
                            if state.required_uncertain {
                                result = Err(PortError::Uncertain);
                            }
                            state.result = Some(result);
                        }
                        owner.changed.notify_waiters();
                    }));
                }
            });
        Box::pin(async move {
            started?;
            loop {
                let notified = this.changed.notified();
                if let Some(result) = this
                    .state
                    .lock()
                    .map_err(|_| PortError::Unavailable)?
                    .result
                {
                    return result;
                }
                notified.await;
            }
        })
    }
}
