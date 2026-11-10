use crate::{Error, HostContext, HostDispatcher, InvocationId, Limits, StructuredValue, backend};
use futures_util::FutureExt;
use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// An owned, executor-safe asynchronous operation.
pub type ScriptFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Stable backend and API revisions.
#[derive(Clone, Debug)]
pub struct EngineInfo {
    /// JavaScript implementation revision.
    pub javascript: &'static str,
    /// WASM interpreter revision.
    pub interpreter: &'static str,
    /// Host bridge ABI revision.
    pub api_version: u32,
    /// Maximum concurrent reservations, including cleanup.
    pub admission_limit: usize,
}

/// Trusted terminal result, published only after the worker thread is joined.
#[derive(Debug)]
pub struct ExecutionReport {
    /// Accepted script output or a bounded error category.
    pub result: Result<StructuredValue, Error>,
    /// Interpreter fuel consumed before teardown.
    pub fuel: u64,
    /// Elapsed wall-clock time including host cleanup.
    pub elapsed: Duration,
    /// Whether actual host resource cleanup completed successfully.
    pub cleanup_complete: bool,
}
impl ExecutionReport {
    pub(crate) fn new(
        result: Result<StructuredValue, Error>,
        fuel: u64,
        start: Instant,
        clean: bool,
    ) -> Self {
        Self {
            result,
            fuel,
            elapsed: start.elapsed(),
            cleanup_complete: clean,
        }
    }
}

/// Independent cancellation authority. Dropping a handle does not cancel execution.
pub trait ScriptCancellation: Send + Sync {
    /// Request cancellation; repeated requests and requests after termination are harmless.
    fn cancel(&self);
}

/// One fresh realm's owner. Drop requests cancellation; explicit join proves teardown.
pub trait ScriptExecution: Send {
    /// Obtain a cloneable control handle before consuming the owner through `join`.
    ///
    /// The handle survives `join`; its drop does not request cancellation or prove teardown.
    fn cancellation(&self) -> Arc<dyn ScriptCancellation>;
    /// Signal cancellation and wake blocked host/callback waits.
    fn cancel(&self);
    /// Await terminal notification and joined teardown without blocking the executor.
    fn join(self: Box<Self>) -> ScriptFuture<ExecutionReport>;
}

/// Object-safe, implementation-independent engine boundary.
pub trait ScriptEngine: Send + Sync {
    /// Compile source without executing module bodies, getters, or host effects.
    ///
    /// # Errors
    /// Returns configuration, admission, syntax, or interpreter budget errors.
    fn validate(&self, source: &str, limits: &Limits) -> Result<(), Error>;
    /// Reserve global capacity before spawning a fresh execution thread.
    ///
    /// # Errors
    /// Rejects invalid input, source syntax, exhausted admission, or thread failure.
    fn start(
        &self,
        source: String,
        input: StructuredValue,
        limits: Limits,
        host: Arc<dyn HostDispatcher>,
    ) -> Result<Box<dyn ScriptExecution>, Error>;
    /// Report pinned revisions and engine-wide admission limits.
    fn info(&self) -> EngineInfo;
    /// Stop admission, cancel all owned executions, and join the reaper.
    fn shutdown(&self) -> ScriptFuture<Result<(), Error>>;
}

struct Job {
    thread: thread::JoinHandle<ExecutionReport>,
    complete: oneshot::Sender<ExecutionReport>,
    shared: Arc<Shared>,
    id: InvocationId,
}
fn reap(receive: &mpsc::Receiver<Job>) {
    let mut pending = Vec::<Job>::new();
    let mut closed = false;
    loop {
        if closed {
            if pending.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        } else {
            let next = if pending.is_empty() {
                receive
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            } else {
                receive.recv_timeout(Duration::from_millis(1))
            };
            match next {
                Ok(job) => pending.push(job),
                Err(mpsc::RecvTimeoutError::Disconnected) => closed = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let mut index = 0;
        while index < pending.len() {
            if !pending[index].thread.is_finished() {
                index += 1;
                continue;
            }
            let job = pending.swap_remove(index);
            let mut report = job.thread.join().unwrap_or(ExecutionReport {
                result: Err(Error::Panic),
                fuel: 0,
                elapsed: Duration::ZERO,
                cleanup_complete: false,
            });
            if let Ok(mut tokens) = job.shared.cancellations.lock() {
                tokens.remove(&job.id);
            }
            if report.cleanup_complete {
                job.shared.used.fetch_sub(1, Ordering::AcqRel);
            } else {
                job.shared.failed.store(true, Ordering::Release);
                report.result = Err(Error::Cleanup);
            }
            job.shared.active.fetch_sub(1, Ordering::AcqRel);
            job.shared.notify.notify_waiters();
            let _sent = job.complete.send(report);
        }
    }
}
struct Gate {
    send: Option<mpsc::Sender<Job>>,
    closed: bool,
}
struct Shared {
    gate: Mutex<Gate>,
    reaper: Mutex<Option<thread::JoinHandle<()>>>,
    cancellations: Mutex<BTreeMap<InvocationId, CancellationToken>>,
    used: AtomicUsize,
    active: AtomicUsize,
    failed: AtomicBool,
    reaper_joined: AtomicBool,
    notify: Notify,
    limit: usize,
}
impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineState")
            .field("limit", &self.limit)
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

/// The sole production engine: QuickJS-NG WASM interpreted by Wasmi, without JIT.
#[derive(Clone, Debug)]
pub struct Engine {
    shared: Arc<Shared>,
}
impl Engine {
    /// Construct an engine-wide admission gate and one blocking thread reaper.
    ///
    /// # Errors
    /// Rejects admission outside 1..=128 or failure to spawn the reaper.
    pub fn new(admission_limit: usize) -> Result<Self, Error> {
        if admission_limit == 0 || admission_limit > 128 {
            return Err(Error::Invalid);
        }
        let (send, receive) = mpsc::channel::<Job>();
        let reaper = thread::Builder::new()
            .name("waltr-script-reaper".into())
            .spawn(move || reap(&receive))
            .map_err(|_| Error::Shutdown)?;
        Ok(Self {
            shared: Arc::new(Shared {
                gate: Mutex::new(Gate {
                    send: Some(send),
                    closed: false,
                }),
                reaper: Mutex::new(Some(reaper)),
                cancellations: Mutex::new(BTreeMap::new()),
                used: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                failed: AtomicBool::new(false),
                reaper_joined: AtomicBool::new(false),
                notify: Notify::new(),
                limit: admission_limit,
            }),
        })
    }
    fn reserve(&self) -> Result<(), Error> {
        self.shared
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.shared.limit).then_some(n + 1)
            })
            .map_err(|_| Error::Busy)?;
        Ok(())
    }
}

impl ScriptEngine for Engine {
    fn validate(&self, source: &str, limits: &Limits) -> Result<(), Error> {
        limits.validate()?;
        if source.len() > limits.source_bytes {
            return Err(Error::Limit);
        }
        let gate = self.shared.gate.lock().map_err(|_| Error::Shutdown)?;
        if gate.closed {
            return Err(Error::Shutdown);
        }
        self.reserve()?;
        self.shared.active.fetch_add(1, Ordering::AcqRel);
        drop(gate);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            backend::validate(source, limits.clone())
        }));
        self.shared.used.fetch_sub(1, Ordering::AcqRel);
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
        self.shared.notify.notify_waiters();
        result.map_err(|_| Error::Panic)?
    }
    #[allow(clippy::too_many_lines)] // Admission and thread ownership transfer remain in one function.
    fn start(
        &self,
        source: String,
        input: StructuredValue,
        limits: Limits,
        host: Arc<dyn HostDispatcher>,
    ) -> Result<Box<dyn ScriptExecution>, Error> {
        limits.validate()?;
        if source.len() > limits.source_bytes {
            return Err(Error::Limit);
        }
        input.encode(&limits)?;
        // Syntax rejection precedes execution admission and any host effect.
        self.validate(&source, &limits)?;
        let gate = self.shared.gate.lock().map_err(|_| Error::Shutdown)?;
        if gate.closed {
            return Err(Error::Shutdown);
        }
        self.reserve()?;
        let context = HostContext::new(limits.clone())?;
        let id = context.invocation();
        let cancel = CancellationToken::new();
        self.shared
            .cancellations
            .lock()
            .map_err(|_| Error::Shutdown)?
            .insert(id, cancel.clone());
        let worker_cancel = cancel.clone();
        let worker = thread::Builder::new()
            .name("waltr-script".into())
            .spawn(move || {
                let start = Instant::now();
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return ExecutionReport::new(Err(Error::Shutdown), 0, start, false);
                };
                runtime.block_on(async move {
                    let report = AssertUnwindSafe(backend::run(
                        source,
                        input,
                        host.clone(),
                        context.clone(),
                        worker_cancel,
                        start,
                    ))
                    .catch_unwind()
                    .await;
                    let mut report = report.unwrap_or_else(|_| {
                        ExecutionReport::new(Err(Error::Panic), 0, start, true)
                    });
                    // Registry is closed before host cleanup; no late task can mint identities.
                    let registry_closed = context.close().is_ok();
                    let cleanup = AssertUnwindSafe(async {
                        tokio::time::timeout(
                            limits.deadline.min(Duration::from_secs(10)),
                            host.cleanup(id),
                        )
                        .await
                        .map_err(|_| Error::Cleanup)?
                    })
                    .catch_unwind()
                    .await;
                    report.cleanup_complete = registry_closed && matches!(cleanup, Ok(Ok(())));
                    if !report.cleanup_complete {
                        report.result = Err(Error::Cleanup);
                    }
                    report.elapsed = start.elapsed();
                    report
                })
            });
        let Ok(worker) = worker else {
            self.shared.used.fetch_sub(1, Ordering::AcqRel);
            self.shared
                .cancellations
                .lock()
                .map_err(|_| Error::Shutdown)?
                .remove(&id);
            return Err(Error::Shutdown);
        };
        let (complete, receive) = oneshot::channel();
        self.shared.active.fetch_add(1, Ordering::AcqRel);
        let job = Job {
            thread: worker,
            complete,
            shared: self.shared.clone(),
            id,
        };
        if gate
            .send
            .as_ref()
            .ok_or(Error::Shutdown)?
            .send(job)
            .is_err()
        {
            cancel.cancel();
            self.shared.failed.store(true, Ordering::Release);
            return Err(Error::Cleanup);
        }
        drop(gate);
        Ok(Box::new(Execution {
            cancel,
            receive: Some(receive),
        }))
    }
    fn info(&self) -> EngineInfo {
        EngineInfo {
            javascript: "QuickJS-NG 0.16.2",
            interpreter: "Wasmi 0.46.0",
            api_version: 1,
            admission_limit: self.shared.limit,
        }
    }
    fn shutdown(&self) -> ScriptFuture<Result<(), Error>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            {
                let mut gate = shared.gate.lock().map_err(|_| Error::Shutdown)?;
                gate.closed = true;
                gate.send.take();
                drop(gate);
                for token in shared
                    .cancellations
                    .lock()
                    .map_err(|_| Error::Cleanup)?
                    .values()
                {
                    token.cancel();
                }
            }
            loop {
                let notified = shared.notify.notified();
                if shared.active.load(Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
            let reaper = shared.reaper.lock().map_err(|_| Error::Cleanup)?.take();
            if let Some(reaper) = reaper {
                let state = shared.clone();
                tokio::task::spawn_blocking(move || {
                    if reaper.join().is_err() {
                        state.failed.store(true, Ordering::Release);
                    }
                    state.reaper_joined.store(true, Ordering::Release);
                    state.notify.notify_waiters();
                })
                .await
                .map_err(|_| Error::Cleanup)?;
            }
            loop {
                let notified = shared.notify.notified();
                if shared.reaper_joined.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
            if shared.failed.load(Ordering::Acquire) {
                Err(Error::Cleanup)
            } else {
                Ok(())
            }
        })
    }
}

struct Execution {
    cancel: CancellationToken,
    receive: Option<oneshot::Receiver<ExecutionReport>>,
}
struct Cancellation(CancellationToken);
impl ScriptCancellation for Cancellation {
    fn cancel(&self) {
        self.0.cancel();
    }
}
impl ScriptExecution for Execution {
    fn cancellation(&self) -> Arc<dyn ScriptCancellation> {
        Arc::new(Cancellation(self.cancel.clone()))
    }
    fn cancel(&self) {
        self.cancel.cancel();
    }
    fn join(mut self: Box<Self>) -> ScriptFuture<ExecutionReport> {
        let receive = self.receive.take();
        let guard = self.cancel.clone().drop_guard();
        Box::pin(async move {
            // Keep cancellation ownership if the join future itself is dropped.
            let report = match receive {
                Some(receive) => receive.await.unwrap_or(ExecutionReport {
                    result: Err(Error::Cleanup),
                    fuel: 0,
                    elapsed: Duration::ZERO,
                    cleanup_complete: false,
                }),
                None => ExecutionReport {
                    result: Err(Error::Cleanup),
                    fuel: 0,
                    elapsed: Duration::ZERO,
                    cleanup_complete: false,
                },
            };
            drop(guard);
            report
        })
    }
}
impl Drop for Execution {
    fn drop(&mut self) {
        if self.receive.is_some() {
            self.cancel.cancel();
        }
    }
}
