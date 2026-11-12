//! Directly constructed unified execution service; no production route registration.
use super::{
    definition::{API_VERSION, Definition, Trigger, identifier},
    history::{Admission, Caller, Operation, OperationStatus, Origin, Ownership, Revision, Status},
    ports::{
        Clock, HostFactory, InvocationStore, OperationPolicy, PortError, PreparedHost, Reservation,
        SelectedEntrypoint,
    },
};
use futures_util::FutureExt;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;
use waltr_script::{
    CleanupFuture, HostContext, HostDispatcher, HostFuture, HostRequest, InvocationId, Limits,
    ScriptEngine, StructuredValue,
};

#[derive(Clone, Debug)]
/// Verified terminal observation, or explicit inability to verify it.
pub struct Outcome {
    /// Immutable invocation identity.
    pub id: String,
    /// None means durable terminalization could not be verified, not success.
    pub status: Option<Status>,
    /// Typed complete output, only after durable success.
    pub result: Option<Value>,
    /// Engine cleanup and required owner finalization both verified.
    pub cleanup_complete: bool,
}
struct Completion {
    outcome: Mutex<Option<Outcome>>,
    notify: Notify,
}
struct Entry {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    reusable: Arc<AtomicBool>,
    completion: Arc<Completion>,
}
struct Gate {
    maintenance: bool,
    shutdown: bool,
    entries: BTreeMap<String, Entry>,
    retained: Vec<Box<dyn Reservation>>,
    failed: bool,
    quarantined: usize,
}
struct Shared {
    engine: Arc<dyn ScriptEngine>,
    store: Arc<dyn InvocationStore>,
    factory: Arc<dyn HostFactory>,
    clock: Arc<dyn Clock>,
    limit: usize,
    gate: Mutex<Gate>,
    shutdown_result: Mutex<Option<Result<(), PortError>>>,
    shutdown_notify: Notify,
}
impl Drop for Shared {
    fn drop(&mut self) {
        let gate = self
            .gate
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Unknown cleanup must never release a pause through Reservation::drop.
        // At most `limit` reservations remain quarantined for process lifetime.
        for reservation in gate.retained.drain(..) {
            std::mem::forget(reservation);
        }
    }
}
#[derive(Clone)]
/// Bounded service owner, constructed directly with explicit consumer ports.
pub struct InvocationService {
    shared: Arc<Shared>,
}
/// Detached handles do not cancel on Drop. Leased/tool handles do; cancellation
/// remains service-owned until engine join, host cleanup and durable terminalization.
pub struct InvocationHandle {
    /// Immutable invocation identity, for observation and explicit cancellation.
    pub id: String,
    ownership: Ownership,
    cancel: CancellationToken,
    completion: Arc<Completion>,
}
/// Child ownership exists before its potentially cancelled admission acknowledgment.
pub struct ChildAdmission {
    /// Registered observation/cancellation owner, including failed preflight.
    pub owner: InvocationHandle,
    /// Admission acknowledgment; dropping it does not discard the owner.
    pub admitted: super::ports::PortFuture<InvocationHandle>,
}
/// Complete child selection and input, still subject to normal service admission checks.
pub struct ChildInvocation {
    /// Immutable host-selected definition.
    pub definition: Arc<Definition>,
    /// Exact declared entrypoint.
    pub entrypoint: String,
    /// Inherited authenticated caller.
    pub caller: Caller,
    /// Complete bounded input, never authority.
    pub input: Value,
}
impl std::fmt::Debug for ChildInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildInvocation").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for ChildAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildAdmission").finish_non_exhaustive()
    }
}
struct PendingOwner(Option<CancellationToken>);
impl PendingOwner {
    fn disarm(mut self) {
        self.0 = None;
    }
}
impl Drop for PendingOwner {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}
impl InvocationHandle {
    /// Request cancellation; joining remains service-owned.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    /// Observe completion without consuming or detaching execution ownership.
    pub async fn join(&self) -> Outcome {
        loop {
            let notified = self.completion.notify.notified();
            if let Some(outcome) = self
                .completion
                .outcome
                .lock()
                .expect("completion mutex")
                .clone()
            {
                return outcome;
            }
            notified.await;
        }
    }
}
impl Drop for InvocationHandle {
    fn drop(&mut self) {
        if self.ownership != Ownership::Detached {
            self.cancel.cancel();
        }
    }
}

impl InvocationService {
    /// Readiness excludes maintenance, shutdown, and unverifiable retained cleanup.
    pub fn is_ready(&self) -> bool {
        self.shared.gate.lock().is_ok_and(|gate| {
            !gate.maintenance && !gate.shutdown && !gate.failed && gate.quarantined == 0
        })
    }
    pub(crate) fn code_definition(&self, envelope: &str) -> Result<Arc<Definition>, PortError> {
        Definition::parse(envelope, self.shared.engine.as_ref()).map_err(|_| PortError::Invalid)
    }

    pub(crate) fn invoke_code(
        &self,
        request: ChildInvocation,
        factory: Arc<dyn HostFactory>,
        reserved: String,
    ) -> Result<ChildAdmission, PortError> {
        self.invoke_scoped(request, None, Some(factory), Some(reserved))
            .map(|(admitted, owner)| ChildAdmission {
                owner,
                admitted: Box::pin(admitted),
            })
    }
    /// Construct an isolated service with at most 128 active/quarantined owners.
    pub fn new(
        engine: Arc<dyn ScriptEngine>,
        store: Arc<dyn InvocationStore>,
        factory: Arc<dyn HostFactory>,
        clock: Arc<dyn Clock>,
        limit: usize,
    ) -> Result<Self, PortError> {
        if limit == 0 || limit > 128 {
            return Err(PortError::Invalid);
        }
        Ok(Self {
            shared: Arc::new(Shared {
                engine,
                store,
                factory,
                clock,
                limit,
                shutdown_result: Mutex::new(None),
                shutdown_notify: Notify::new(),
                gate: Mutex::new(Gate {
                    maintenance: false,
                    shutdown: false,
                    entries: BTreeMap::new(),
                    retained: Vec::new(),
                    failed: false,
                    quarantined: 0,
                }),
            }),
        })
    }
    /// Atomically reject new invokes from every origin; does not cancel admitted work.
    pub fn set_maintenance(&self, enabled: bool) -> Result<(), PortError> {
        self.shared
            .gate
            .lock()
            .map_err(|_| PortError::Unavailable)?
            .maintenance = enabled;
        Ok(())
    }
    /// The bounded owner is registered synchronously before returning a future.
    /// Dropping the admission acknowledgment cannot cancel committed detached work.
    pub fn invoke(
        &self,
        definition: Arc<Definition>,
        entrypoint: String,
        caller: Caller,
        input: Value,
    ) -> Result<impl Future<Output = Result<InvocationHandle, PortError>> + Send + 'static, PortError>
    {
        self.invoke_scoped(
            ChildInvocation {
                definition,
                entrypoint,
                caller,
                input,
            },
            None,
            None,
            None,
        )
        .map(|(admitted, _observation)| admitted)
    }

    /// Host-only bounded child admission; normal entry adapters cannot provide a scope.
    pub fn invoke_child(
        &self,
        request: ChildInvocation,
        scope: super::children::ChildScope,
    ) -> Result<ChildAdmission, PortError> {
        self.invoke_child_bound(request, scope, None)
    }

    pub(crate) fn invoke_child_bound(
        &self,
        request: ChildInvocation,
        scope: super::children::ChildScope,
        factory: Option<Arc<dyn HostFactory>>,
    ) -> Result<ChildAdmission, PortError> {
        // The parent is awaiting this call inside the same engine. No spare slot can exist.
        if self.shared.engine.info().admission_limit <= 1 {
            return Err(PortError::Unavailable);
        }
        if request.caller.principal != scope.principal
            || request.caller.ownership != Ownership::Tool
            || !request
                .definition
                .envelope()
                .spec
                .capabilities
                .is_subset(&scope.capabilities)
            || tokio::time::Instant::now() >= scope.deadline
        {
            return Err(PortError::Denied);
        }
        self.invoke_scoped(request, Some(scope), factory, None)
            .map(|(admitted, owner)| ChildAdmission {
                owner,
                admitted: Box::pin(admitted),
            })
    }

    fn invoke_scoped(
        &self,
        request: ChildInvocation,
        scope: Option<super::children::ChildScope>,
        factory: Option<Arc<dyn HostFactory>>,
        reserved_id: Option<String>,
    ) -> Result<
        (
            impl Future<Output = Result<InvocationHandle, PortError>> + Send + 'static,
            InvocationHandle,
        ),
        PortError,
    > {
        let ChildInvocation {
            definition,
            entrypoint,
            caller,
            input,
        } = request;
        validate_caller(&caller)?;
        let descriptor = definition
            .envelope()
            .spec
            .entrypoints
            .get(&entrypoint)
            .ok_or(PortError::Invalid)?;
        if !compatible(caller.origin, descriptor.trigger) {
            return Err(PortError::Invalid);
        }
        let selected = SelectedEntrypoint {
            name: entrypoint.clone(),
            descriptor: descriptor.clone(),
        };
        let info = self.shared.engine.info();
        if definition.engine()
            != format!(
                "{};{};abi={}",
                info.javascript, info.interpreter, info.api_version
            )
        {
            return Err(PortError::Invalid);
        }
        let input = definition
            .envelope()
            .spec
            .parameters
            .accepts(&input, definition.limits())
            .map_err(|_| PortError::Invalid)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| PortError::Unavailable)?;
        let mut gate = self
            .shared
            .gate
            .lock()
            .map_err(|_| PortError::Unavailable)?;
        let mut failed = false;
        let mut quarantined = 0;
        gate.entries
            .retain(|id, entry| match (&mut entry.task).now_or_never() {
                None => true,
                Some(result) => {
                    let clean = result.is_ok() && entry.reusable.load(Ordering::Acquire);
                    failed |= !clean;
                    quarantined += usize::from(!clean);
                    if result.is_err() {
                        *entry.completion.outcome.lock().expect("completion mutex") =
                            Some(Outcome {
                                id: id.clone(),
                                status: None,
                                result: None,
                                cleanup_complete: false,
                            });
                        entry.completion.notify.notify_waiters();
                    }
                    false
                }
            });
        gate.failed |= failed;
        gate.quarantined += quarantined;
        if gate.maintenance || gate.shutdown {
            return Err(PortError::Denied);
        }
        if gate.entries.len() + gate.quarantined >= self.shared.limit {
            return Err(PortError::Unavailable);
        }
        let id = reserved_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if uuid::Uuid::parse_str(&id).is_err() || gate.entries.contains_key(&id) {
            return Err(PortError::Conflict);
        }
        let cancel = CancellationToken::new();
        let pending_owner =
            PendingOwner((caller.ownership != Ownership::Detached).then(|| cancel.clone()));
        let completion = Arc::new(Completion {
            outcome: Mutex::new(None),
            notify: Notify::new(),
        });
        let reusable = Arc::new(AtomicBool::new(false));
        let (send, receive) = oneshot::channel();
        let shared = self.shared.clone();
        let task_id = id.clone();
        let observation_id = id.clone();
        let task_cancel = cancel.clone();
        let task_reusable = reusable.clone();
        let observation = InvocationHandle {
            id: id.clone(),
            ownership: Ownership::Detached,
            cancel: cancel.clone(),
            completion: completion.clone(),
        };
        let entry_completion = completion.clone();
        use tracing::Instrument as _;
        let trace = super::telemetry::Operation::new(
            tracing::info_span!(target: "waltr_server::routine::telemetry", "routine.invocation", routine_name = super::telemetry::routine_name(&definition.envelope().name), invocation_id = super::telemetry::invocation_id(&id), outcome = tracing::field::Empty, failed_stage = tracing::field::Empty, error_category = tracing::field::Empty),
        );
        let task_span = trace.span.clone();
        let task = runtime.spawn(
            async move {
                let work = run(
                    shared,
                    task_id.clone(),
                    definition,
                    entrypoint,
                    caller,
                    selected,
                    input,
                    task_cancel,
                    completion.clone(),
                    send,
                    task_reusable.clone(),
                    scope,
                    factory,
                );
                if AssertUnwindSafe(work).catch_unwind().await.is_err() {
                    super::telemetry::failure("invocation", "panic");
                    *completion.outcome.lock().expect("completion mutex") = Some(Outcome {
                        id: task_id,
                        status: None,
                        result: None,
                        cleanup_complete: false,
                    });
                    completion.notify.notify_waiters();
                }
                let mut outcome = completion.outcome.lock().expect("completion mutex");
                if outcome.is_none() {
                    *outcome = Some(Outcome {
                        id: observation_id,
                        status: None,
                        result: None,
                        cleanup_complete: task_reusable.load(Ordering::Acquire),
                    });
                    completion.notify.notify_waiters();
                }
                trace.outcome(match outcome.as_ref().and_then(|outcome| outcome.status) {
                    Some(Status::Succeeded) => "success",
                    Some(Status::Interrupted) => "cancelled",
                    Some(Status::Uncertain) => "uncertain",
                    _ => "failure",
                });
            }
            .instrument(task_span),
        );
        gate.entries.insert(
            id,
            Entry {
                cancel,
                task,
                reusable,
                completion: entry_completion,
            },
        );
        drop(gate);
        Ok((
            async move {
                let result = receive.await.map_err(|_| PortError::Unavailable)?;
                pending_owner.disarm();
                result
            },
            observation,
        ))
    }
    /// Trusted request-local transport composition over the same engine, gate and SQL owner.
    pub(crate) fn invoke_bound(
        &self,
        request: ChildInvocation,
        factory: Arc<dyn HostFactory>,
    ) -> Result<ChildAdmission, PortError> {
        self.invoke_scoped(request, None, Some(factory), None)
            .map(|(admitted, owner)| ChildAdmission {
                owner,
                admitted: Box::pin(admitted),
            })
    }
    /// Request cancellation by invocation identity.
    pub fn cancel(&self, id: &str) -> Result<(), PortError> {
        let gate = self
            .shared
            .gate
            .lock()
            .map_err(|_| PortError::Unavailable)?;
        let entry = gate.entries.get(id).ok_or(PortError::Invalid)?;
        entry.cancel.cancel();
        Ok(())
    }
    /// Stops all origins atomically against invoke, then joins every owner. Engine
    /// shutdown is composition-owned because an engine may have other consumers.
    pub async fn shutdown(&self) -> Result<(), PortError> {
        {
            let mut gate = self
                .shared
                .gate
                .lock()
                .map_err(|_| PortError::Unavailable)?;
            if !gate.shutdown {
                gate.shutdown = true;
                for entry in gate.entries.values() {
                    entry.cancel.cancel();
                }
                let entries = std::mem::take(&mut gate.entries);
                let shared = self.shared.clone();
                tokio::spawn(async move {
                    let mut clean = true;
                    for (id, entry) in entries {
                        let joined = entry.task.await;
                        clean &= joined.is_ok() && entry.reusable.load(Ordering::Acquire);
                        if joined.is_err() {
                            *entry.completion.outcome.lock().expect("completion mutex") =
                                Some(Outcome {
                                    id,
                                    status: None,
                                    result: None,
                                    cleanup_complete: false,
                                });
                            entry.completion.notify.notify_waiters();
                        }
                    }
                    clean &= shared
                        .gate
                        .lock()
                        .map(|gate| {
                            gate.retained.is_empty() && !gate.failed && gate.quarantined == 0
                        })
                        .unwrap_or(false);
                    *shared.shutdown_result.lock().expect("shutdown result") = Some(if clean {
                        Ok(())
                    } else {
                        Err(PortError::Uncertain)
                    });
                    shared.shutdown_notify.notify_waiters();
                });
            }
        }
        loop {
            let notified = self.shared.shutdown_notify.notified();
            if let Some(result) = *self
                .shared
                .shutdown_result
                .lock()
                .map_err(|_| PortError::Unavailable)?
            {
                return result;
            }
            notified.await;
        }
    }
}
impl std::fmt::Debug for InvocationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvocationService")
            .field("limit", &self.shared.limit)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for InvocationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvocationHandle")
            .field("ownership", &self.ownership)
            .finish_non_exhaustive()
    }
}
fn validate_caller(caller: &Caller) -> Result<(), PortError> {
    if !identifier(&caller.principal) || caller.correlation.len() > 128 {
        return Err(PortError::Invalid);
    }
    let valid = match caller.origin {
        Origin::Manual
        | Origin::Event
        | Origin::Schedule
        | Origin::Lifecycle
        | Origin::Progress => caller.ownership == Ownership::Detached,
        Origin::Interactive => caller.ownership == Ownership::Leased,
        Origin::Tool | Origin::Inline => caller.ownership == Ownership::Tool,
        Origin::Mcp => caller.ownership != Ownership::Detached,
    };
    if valid {
        Ok(())
    } else {
        Err(PortError::Invalid)
    }
}

fn compatible(origin: Origin, trigger: Trigger) -> bool {
    matches!(
        (origin, trigger),
        (Origin::Manual, Trigger::Manual)
            | (Origin::Event, Trigger::Event)
            | (Origin::Schedule, Trigger::Schedule)
            | (Origin::Lifecycle, Trigger::Lifecycle)
            | (Origin::Progress, Trigger::Progress)
            | (Origin::Interactive, Trigger::Interactive)
            | (Origin::Tool, Trigger::Tool)
            | (Origin::Inline, Trigger::Inline)
            | (Origin::Mcp, Trigger::Mcp)
    )
}

#[allow(clippy::too_many_arguments)]
async fn run(
    shared: Arc<Shared>,
    id: String,
    definition: Arc<Definition>,
    entrypoint: String,
    caller: Caller,
    selected: SelectedEntrypoint,
    input: StructuredValue,
    cancel: CancellationToken,
    completion: Arc<Completion>,
    send: oneshot::Sender<Result<InvocationHandle, PortError>>,
    reusable: Arc<AtomicBool>,
    scope: Option<super::children::ChildScope>,
    factory: Option<Arc<dyn HostFactory>>,
) {
    let started_ms = shared.clock.now_ms();
    let binding = super::history::InvocationBinding::new(id.clone(), caller.principal.clone())
        .at_start(started_ms);
    let snapshot = scope.as_ref().map(|scope| scope.snapshot.provenance());
    let factory = factory.as_ref().unwrap_or(&shared.factory);
    let preflight_future = match scope {
        Some(scope) => {
            factory.preflight_child(binding, definition.clone(), selected, caller.clone(), scope)
        }
        None => factory.preflight_input(
            binding,
            definition.clone(),
            selected,
            caller.clone(),
            input.as_json().clone(),
        ),
    };
    let preflight = tokio::select! {
        () = cancel.cancelled() => Err(PortError::Denied),
        result = tokio::time::timeout(definition.limits().deadline, preflight_future) => result.unwrap_or(Err(PortError::Unavailable)),
    };
    let prepared = match preflight {
        Ok(host) => host,
        Err(error) => {
            super::telemetry::failure(
                "preflight",
                if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "admission"
                },
            );
            reusable.store(true, Ordering::Release);
            drop(send.send(Err(error)));
            return;
        }
    };
    let stopped = {
        let gate = shared.gate.lock().expect("service gate");
        gate.maintenance || gate.shutdown
    };
    if cancel.is_cancelled() || stopped {
        super::telemetry::failure("prepare", "cancelled");
        let clean = prepared.reservation.abort().await.is_ok();
        reusable.store(clean, Ordering::Release);
        if !clean {
            shared
                .gate
                .lock()
                .expect("service gate")
                .retained
                .push(prepared.reservation);
        }
        drop(send.send(Err(PortError::Denied)));
        return;
    }
    if prepared.snapshot.as_ref().is_some_and(|s| !identifier(s))
        || snapshot
            .as_ref()
            .is_some_and(|snapshot| prepared.snapshot.as_ref() != Some(snapshot))
    {
        super::telemetry::failure("prepare", "identity");
        let clean = prepared.reservation.abort().await.is_ok();
        reusable.store(clean, Ordering::Release);
        if !clean {
            shared
                .gate
                .lock()
                .expect("service gate")
                .retained
                .push(prepared.reservation);
        }
        drop(send.send(Err(PortError::Invalid)));
        return;
    }
    // Barrier acquisition is owned by this registered service task, never by a
    // droppable guest/preflight future. Once started, join it and its release.
    let preparation = AssertUnwindSafe(prepared.reservation.prepare())
        .catch_unwind()
        .await
        .unwrap_or(Err(PortError::Uncertain));
    let stopped = {
        let gate = shared.gate.lock().expect("service gate");
        gate.maintenance || gate.shutdown
    };
    if preparation.is_err() || cancel.is_cancelled() || stopped {
        super::telemetry::failure(
            "prepare",
            if cancel.is_cancelled() || stopped {
                "cancelled"
            } else {
                "preparation"
            },
        );
        let clean = AssertUnwindSafe(prepared.reservation.abort())
            .catch_unwind()
            .await
            .is_ok_and(|result| result.is_ok());
        reusable.store(clean, Ordering::Release);
        if !clean {
            shared
                .gate
                .lock()
                .expect("service gate")
                .retained
                .push(prepared.reservation);
        }
        drop(send.send(Err(if clean {
            preparation.err().unwrap_or(PortError::Denied)
        } else {
            PortError::Uncertain
        })));
        return;
    }
    let admission = Admission {
        id: id.clone(),
        revision: Revision {
            id: definition.revision().into(),
            api_version: API_VERSION.into(),
            engine: definition.engine().into(),
            envelope: definition.canonical().into(),
            name: definition.envelope().name.clone(),
        },
        caller: caller.clone(),
        entrypoint,
        snapshot: prepared.snapshot.clone(),
        admitted_ms: started_ms,
    };
    // Once commit has been attempted, cancellation must not drop its future.
    let source_session = prepared.reservation.source_session();
    let public_definition = prepared.reservation.public_definition();
    let admission = match AssertUnwindSafe(async {
        shared
            .store
            .admit_with_projection(admission, source_session, public_definition)
            .await
    })
    .catch_unwind()
    .await
    {
        Ok(result) => result,
        Err(_) => {
            // Ambiguous history admission still cannot abandon an acquired pause.
            super::telemetry::failure("admission", "panic");
            drop(
                AssertUnwindSafe(prepared.reservation.abort())
                    .catch_unwind()
                    .await,
            );
            shared
                .gate
                .lock()
                .expect("service gate")
                .retained
                .push(prepared.reservation);
            drop(send.send(Err(PortError::Uncertain)));
            return;
        }
    };
    if let Err(error) = admission {
        super::telemetry::failure("admission", "persistence");
        // A failed commit acknowledgment does not prove absence. Never start a
        // guest after ambiguous admission; reconcile its durable observation.
        let recover = async {
            if prepared.reservation.abort().await.is_err() {
                return false;
            }
            match shared.store.get(id.clone()).await {
                Ok(None) => true,
                Ok(Some(_)) => {
                    let finalized = prepared.reservation.finalize(true).await.is_ok();
                    finalized
                        && shared
                            .store
                            .terminal(id.clone(), Status::Interrupted, None, shared.clock.now_ms())
                            .await
                            .is_ok()
                }
                Err(_) => false,
            }
        };
        let safe = AssertUnwindSafe(recover)
            .catch_unwind()
            .await
            .unwrap_or(false);
        if safe {
            reusable.store(true, Ordering::Release);
        } else {
            shared
                .gate
                .lock()
                .expect("service gate")
                .retained
                .push(prepared.reservation);
        }
        drop(send.send(Err(error)));
        return;
    }
    let handle = InvocationHandle {
        id: id.clone(),
        ownership: caller.ownership,
        cancel: cancel.clone(),
        completion: completion.clone(),
    };
    drop(send.send(Ok(handle)));
    // Catch adapter/engine panics while retaining the admitted reservation.
    let execution = execute(&shared, &id, &definition, input, &cancel, &prepared);
    let (outcome, release) = match AssertUnwindSafe(execution).catch_unwind().await {
        Ok(pair) => pair,
        Err(_) => {
            super::telemetry::failure("execution", "panic");
            (
                Outcome {
                    id: id.clone(),
                    status: None,
                    result: None,
                    cleanup_complete: false,
                },
                false,
            )
        }
    };
    if release {
        reusable.store(true, Ordering::Release);
    } else {
        shared
            .gate
            .lock()
            .expect("service gate")
            .retained
            .push(prepared.reservation);
    }
    *completion.outcome.lock().expect("completion mutex") = Some(outcome);
    completion.notify.notify_waiters();
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    shared: &Arc<Shared>,
    id: &str,
    definition: &Definition,
    input: StructuredValue,
    cancel: &CancellationToken,
    prepared: &PreparedHost,
) -> (Outcome, bool) {
    let failed = Arc::new(AtomicBool::new(false));
    let mut status = Status::Failed;
    let mut result = None;
    let mut cleanup = true;
    let mut entered_engine = false;
    if shared
        .store
        .running(id.into(), shared.clock.now_ms())
        .await
        .is_ok()
    {
        if cancel.is_cancelled() {
            super::telemetry::failure("engine_start", "cancelled");
            status = Status::Interrupted;
        } else {
            let host = Arc::new(JournalHost {
                trace: tracing::Span::current(),
                id: id.into(),
                store: shared.store.clone(),
                clock: shared.clock.clone(),
                inner: prepared.dispatcher.clone(),
                policy: prepared.policy.clone(),
                failed: failed.clone(),
                cancel: cancel.clone(),
            });
            match shared.engine.start(
                definition.envelope().spec.source.clone(),
                input,
                definition.limits().clone(),
                host,
            ) {
                Ok(owner) => {
                    entered_engine = true;
                    let control = owner.cancellation();
                    let mut joined = owner.join();
                    let report = tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            control.cancel();
                            joined.await
                        }
                        report = &mut joined => report,
                    };
                    cleanup = report.cleanup_complete;
                    if let Err(error) = &report.result {
                        super::telemetry::failure(
                            "engine",
                            super::telemetry::script_category(*error),
                        );
                    }
                    if cancel.is_cancelled() {
                        status = Status::Interrupted;
                    } else if let Ok(value) = report.result {
                        let limits = Limits {
                            value_bytes: definition.envelope().spec.budgets.result_bytes,
                            ..definition.limits().clone()
                        };
                        if definition
                            .envelope()
                            .spec
                            .result
                            .accepts(value.as_json(), &limits)
                            .is_ok()
                        {
                            status = Status::Succeeded;
                            result = Some(value.into_json());
                        } else {
                            super::telemetry::failure("result_validation", "value");
                        }
                    }
                }
                Err(error) => {
                    super::telemetry::failure(
                        "engine_start",
                        super::telemetry::script_category(error),
                    );
                    status = Status::Failed;
                }
            }
        }
    } else {
        super::telemetry::failure("running", "persistence");
    }
    if !entered_engine {
        cleanup = prepared.reservation.abort().await.is_ok();
    }
    if cleanup && status == Status::Succeeded && prepared.reservation.validate_success().is_err() {
        super::telemetry::failure("result_validation", "value");
        status = Status::Failed;
        result = None;
    }
    if !cleanup || failed.load(Ordering::Acquire) {
        super::telemetry::failure("cleanup", "cleanup");
        status = Status::Uncertain;
        result = None;
    }
    let finalized = if cleanup {
        prepared
            .reservation
            .finalize(status != Status::Succeeded)
            .await
            .is_ok()
    } else {
        false
    };
    if !finalized {
        super::telemetry::failure("finalize", "cleanup");
        status = Status::Uncertain;
        result = None;
    }
    let durable = shared
        .store
        .terminal(id.into(), status, result.clone(), shared.clock.now_ms())
        .await
        .ok();
    if durable.is_none() {
        super::telemetry::failure("terminal", "persistence");
    }
    let outcome = Outcome {
        id: id.into(),
        status: durable,
        result: if durable == Some(Status::Succeeded) {
            result
        } else {
            None
        },
        cleanup_complete: cleanup && finalized,
    };
    (outcome, cleanup && finalized && durable.is_some())
}

struct JournalHost {
    trace: tracing::Span,
    id: String,
    store: Arc<dyn InvocationStore>,
    clock: Arc<dyn Clock>,
    inner: Arc<dyn HostDispatcher>,
    policy: Arc<dyn OperationPolicy>,
    failed: Arc<AtomicBool>,
    cancel: CancellationToken,
}
impl HostDispatcher for JournalHost {
    fn dispatch(&self, request: HostRequest, context: HostContext) -> HostFuture {
        let store = self.store.clone();
        let clock = self.clock.clone();
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let failed = self.failed.clone();
        let invocation = self.id.clone();
        let cancel = self.cancel.clone();
        use tracing::Instrument as _;
        let trace = self.trace.with_subscriber(|(_, dispatcher)| {
            tracing::dispatcher::with_default(dispatcher, || super::telemetry::Operation::new(tracing::info_span!(target: "waltr_server::routine::telemetry", parent: &self.trace, "routine.host_call", invocation_id = super::telemetry::invocation_id(&self.id), host_method = super::telemetry::host_method(&request.operation), outcome = tracing::field::Empty, failed_stage = tracing::field::Empty, error_category = tracing::field::Empty)))
        }).unwrap_or_else(|| super::telemetry::Operation::new(tracing::Span::none()));
        let span = trace.span.clone();
        Box::pin(async move {
            let mut stage = "admission";
            let mut dispatch_error = None;
            let work = AssertUnwindSafe(async {
            if cancel.is_cancelled() {
                return Err(waltr_script::Error::Cancelled);
            }
            if failed.load(Ordering::Acquire) {
                return Err(waltr_script::Error::Cleanup);
            }
            let projection = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(waltr_script::Error::Cancelled),
                projection = async { policy.before(&request, &context).await } => projection.map_err(|_| waltr_script::Error::Denied)?,
            };
            if bounded_projection(&projection).is_err() {
                failed.store(true, Ordering::Release);
                cancel.cancel();
                return Err(waltr_script::Error::Value);
            }
            if failed.load(Ordering::Acquire) || cancel.is_cancelled() {
                return Err(waltr_script::Error::Cancelled);
            }
            let operation = Operation {
                version: 1,
                cursor: 0,
                id: uuid::Uuid::new_v4().to_string(),
                invocation,
                name: request.operation.clone(),
                status: OperationStatus::Dispatched,
                arguments: projection,
                result: None,
                updated_ms: clock.now_ms(),
            };
            let id = operation.id.clone();
            stage = "dispatch_intent";
            if store.dispatch_intent(operation).await.is_err() {
                failed.store(true, Ordering::Release);
                cancel.cancel();
                return Err(waltr_script::Error::Cleanup);
            }
            if failed.load(Ordering::Acquire) || cancel.is_cancelled() {
                return Err(waltr_script::Error::Cancelled);
            }
            let name = request.operation.clone();
            stage = "dispatch";
            let reply = tokio::select! {
                biased;
                () = cancel.cancelled() => Err(waltr_script::Error::Cancelled),
                reply = async { inner.dispatch(request, context).await } => reply,
            };
            dispatch_error = reply.as_ref().err().copied();
            let (status, result) = match &reply {
                Ok(value) => match policy.project_result(&name, value) {
                    Ok(projection)
                        if projection
                            .as_ref()
                            .is_none_or(|p| bounded_projection(p).is_ok()) =>
                    {
                        (OperationStatus::Completed, projection)
                    }
                    _ => {
                        failed.store(true, Ordering::Release);
                        cancel.cancel();
                        (OperationStatus::Uncertain, None)
                    }
                },
                Err(_) => (OperationStatus::Uncertain, None),
            };
            if dispatch_error.is_none() { stage = "acknowledgment"; }
            if store
                .operation_done(id, status, result, clock.now_ms())
                .await
                .is_err()
                || status == OperationStatus::Uncertain
            {
                failed.store(true, Ordering::Release);
                cancel.cancel();
                return Err(waltr_script::Error::Cleanup);
            }
            reply
            }).catch_unwind().await;
            match work {
                Ok(reply) => {
                    if let Err(error) = &reply {
                        super::telemetry::failure(stage, super::telemetry::script_category(dispatch_error.unwrap_or(*error)));
                    }
                    trace.outcome(match &reply {
                        Ok(_) => "success",
                        Err(waltr_script::Error::Cancelled) => "cancelled",
                        Err(waltr_script::Error::Cleanup) => "uncertain",
                        Err(_) => "failure",
                    });
                    reply
                }
                Err(panic) => {
                    super::telemetry::failure(stage, "panic");
                    trace.outcome("failure");
                    std::panic::resume_unwind(panic)
                }
            }
        }.instrument(span))
    }
    fn cleanup(&self, invocation: InvocationId) -> CleanupFuture {
        self.inner.cleanup(invocation)
    }
}
pub(crate) fn bounded_projection(value: &Value) -> Result<(), PortError> {
    let limits = Limits {
        value_bytes: 4096,
        string_bytes: 1024,
        value_depth: 8,
        value_nodes: 256,
        ..Limits::default()
    };
    StructuredValue::new(value.clone(), &limits)
        .map(|_| ())
        .map_err(|_| PortError::Invalid)
}

#[cfg(test)]
mod tests {
    include!("service/tests.rs");
    mod diagnostics {
        include!("service/diagnostics_tests.rs");
    }
}
