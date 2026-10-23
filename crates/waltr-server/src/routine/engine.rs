use std::fmt::Write as _;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use waltr_agent::{
    CompletedSessionCapture, SessionCompletionAdmissionFinalizer,
    SessionCompletionCoordinationFuture, SessionCompletionCoordinationHandler,
    SessionCompletionCoordinationPreparation, SessionCompletionFinalizationFuture,
    SessionCoordinationReason, SessionId, SessionPauseLease, SessionRunCompletionSnapshot,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    persistence::routine_invocation::{
        RoutineAdmission, RoutineFailure, RoutineInvocationStore, RoutineTaskDescriptor,
        RoutineTriggerKind, SOURCE_SESSION_METADATA_KEY,
    },
    resource_catalog::{AgentDefinitionState, MaterializedRoutine},
    session::SessionHost,
};

use super::{InvocationError, ManualRoutineInvocation, RoutineInvocationResult, RoutineValue};
use super::{
    event::{EventEnvelope, EventSource},
    executor::execute_invocation,
    expression::conditions_match,
    host::{CoordinatorRoutineHost, RoutineHost, RoutinePauseGuard},
    lifecycle::{LifecycleRecorder, now_ms},
};

pub(super) const TERMINAL_CACHE_CAPACITY: usize = 4096;
const TERMINAL_CACHE_TTL: Duration = Duration::from_hours(1);
const SHUTDOWN_WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub(super) struct DestinationTaskTracker {
    inner: Arc<DestinationTaskTrackerInner>,
}

#[derive(Default)]
struct DestinationTaskTrackerInner {
    state: Mutex<DestinationTaskState>,
    empty: tokio::sync::Notify,
}

#[derive(Default)]
struct DestinationTaskState {
    closed: bool,
    active: usize,
}

struct DestinationTaskGuard {
    inner: Arc<DestinationTaskTrackerInner>,
}

impl DestinationTaskTracker {
    fn reserve(&self) -> Option<DestinationTaskGuard> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("destination tracker poisoned");
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(DestinationTaskGuard {
            inner: Arc::clone(&self.inner),
        })
    }

    pub(super) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let guard = self.reserve().expect("destination tracker is closed");
        tokio::spawn(async move {
            let _guard = guard;
            future.await
        })
    }

    pub(super) async fn close_and_wait(&self) {
        loop {
            let empty = self.inner.empty.notified();
            {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .expect("destination tracker poisoned");
                state.closed = true;
                if state.active == 0 {
                    return;
                }
            }
            empty.await;
        }
    }
}

impl Drop for DestinationTaskGuard {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("destination tracker poisoned");
        state.active -= 1;
        if state.closed && state.active == 0 {
            self.inner.empty.notify_one();
        }
    }
}

/// Liveness of the event admission worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineHealth {
    /// Startup reconciliation is still running.
    Reconciling,
    /// Admission is active.
    Running,
    /// Admission ended unexpectedly.
    Failed(String),
}

/// Owned Routine background runtime.
pub struct RoutineEngine {
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    health: watch::Receiver<EngineHealth>,
    manual: ManualRoutineHandle,
    automatic: Arc<AutomaticRoutineAdmission>,
    completion_host: Option<Arc<dyn SessionHost>>,
}

/// Cloneable process-local admission surface for session-scoped manual Routines.
#[derive(Clone)]
pub struct ManualRoutineHandle {
    catalog: AgentDefinitionState,
    host: Arc<dyn RoutineHost>,
    active_children: Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
    child_admission_closed: Arc<AtomicBool>,
    destination_tasks: DestinationTaskTracker,
    invocation_tasks: DestinationTaskTracker,
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    cancellation: CancellationToken,
    ready: Arc<AtomicBool>,
    store: Option<RoutineInvocationStore>,
    wakeups: broadcast::Sender<()>,
    failure_control: EngineFailureControl,
}

#[derive(Clone)]
struct EngineFailureControl {
    cancellation: CancellationToken,
    ready: Arc<AtomicBool>,
    child_admission_closed: Arc<AtomicBool>,
    health: watch::Sender<EngineHealth>,
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    #[cfg(test)]
    fail_closed_pending: Arc<AtomicBool>,
    #[cfg(test)]
    event_admission_waiters: Arc<AtomicUsize>,
}

struct AutomaticRoutineAdmission {
    manual: ManualRoutineHandle,
}

struct WeakAutomaticRoutineAdmission {
    inner: std::sync::Weak<AutomaticRoutineAdmission>,
}

struct AutomaticRoutineFinalizer {
    configuration: Arc<waltr::grpc::waltr::config::v1::Configuration>,
    manual: ManualRoutineHandle,
    snapshot: SessionRunCompletionSnapshot,
    routine: MaterializedRoutine,
    trigger_name: String,
    context: RoutineInvocationContext,
}

impl EngineFailureControl {
    async fn fail_closed(&self) {
        #[cfg(test)]
        self.fail_closed_pending.store(true, Ordering::SeqCst);
        let _lifecycle = self.lifecycle.write().await;
        self.ready.store(false, Ordering::SeqCst);
        self.child_admission_closed.store(true, Ordering::SeqCst);
        drop(self.health.send(EngineHealth::Failed(
            "routine lifecycle persistence recovery failed".to_owned(),
        )));
        self.cancellation.cancel();
    }
}

impl std::fmt::Debug for ManualRoutineHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManualRoutineHandle")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(super) struct RoutineInvocationContext {
    pub(super) event: Option<EventEnvelope>,
    pub(super) trigger: Value,
    pub(super) params: BTreeMap<String, Value>,
    pub(super) source: Option<CompletedSessionCapture>,
    pub(super) source_values: Value,
}

impl std::fmt::Debug for RoutineEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoutineEngine")
            .finish_non_exhaustive()
    }
}

impl RoutineEngine {
    /// Subscribe and start admitting future events against current immutable catalog snapshots.
    #[must_use]
    pub(crate) fn start(
        source: &impl EventSource,
        catalog: AgentDefinitionState,
        sessions: Arc<dyn SessionHost>,
        store: RoutineInvocationStore,
    ) -> Self {
        let completion_host = Arc::clone(&sessions);
        let mut engine = Self::start_inner(
            source.subscribe(),
            catalog,
            Arc::new(CoordinatorRoutineHost::new(sessions)),
            Some(store),
        );
        let handler: Arc<dyn SessionCompletionCoordinationHandler> =
            Arc::new(WeakAutomaticRoutineAdmission {
                inner: Arc::downgrade(&engine.automatic),
            });
        completion_host.set_completion_admission_handler(Some(handler));
        engine.completion_host = Some(completion_host);
        engine
    }

    #[allow(
        clippy::too_many_lines,
        reason = "engine startup keeps owned lifecycle state together"
    )]
    #[cfg(test)]
    pub(super) fn start_with_host(
        events: broadcast::Receiver<EventEnvelope>,
        catalog: AgentDefinitionState,
        host: Arc<dyn RoutineHost>,
    ) -> Self {
        Self::start_inner(events, catalog, host, None)
    }

    #[cfg(test)]
    pub(super) fn start_with_host_and_store(
        events: broadcast::Receiver<EventEnvelope>,
        catalog: AgentDefinitionState,
        host: Arc<dyn RoutineHost>,
        store: RoutineInvocationStore,
    ) -> Self {
        Self::start_inner(events, catalog, host, Some(store))
    }

    fn start_inner(
        mut events: broadcast::Receiver<EventEnvelope>,
        catalog: AgentDefinitionState,
        host: Arc<dyn RoutineHost>,
        store: Option<RoutineInvocationStore>,
    ) -> Self {
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let active_children = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        let child_admission_closed = Arc::new(AtomicBool::new(false));
        let destination_tasks = DestinationTaskTracker::default();
        let invocation_tasks = DestinationTaskTracker::default();
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let ready = Arc::new(AtomicBool::new(store.is_none()));
        let (wakeups, _) = broadcast::channel(32);
        let cache = Arc::new(Mutex::new(InvocationCache::default()));
        let (health_tx, health) = watch::channel(if store.is_some() {
            EngineHealth::Reconciling
        } else {
            EngineHealth::Running
        });
        let failure_control = EngineFailureControl {
            cancellation: task_cancellation.clone(),
            ready: Arc::clone(&ready),
            child_admission_closed: Arc::clone(&child_admission_closed),
            health: health_tx.clone(),
            lifecycle: Arc::clone(&lifecycle),
            #[cfg(test)]
            fail_closed_pending: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            event_admission_waiters: Arc::new(AtomicUsize::new(0)),
        };
        let manual = ManualRoutineHandle {
            catalog: catalog.clone(),
            host: Arc::clone(&host),
            active_children: Arc::clone(&active_children),
            child_admission_closed: Arc::clone(&child_admission_closed),
            destination_tasks: destination_tasks.clone(),
            invocation_tasks: invocation_tasks.clone(),
            lifecycle: Arc::clone(&lifecycle),
            cancellation: task_cancellation.clone(),
            ready: Arc::clone(&ready),
            store: store.clone(),
            wakeups: wakeups.clone(),
            failure_control: failure_control.clone(),
        };
        let automatic = Arc::new(AutomaticRoutineAdmission {
            manual: manual.clone(),
        });
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            if let Some(store) = &store {
                let failure = RoutineFailure {
                    code: "server_restart".to_owned(),
                    message: "routine interrupted by server restart".to_owned(),
                };
                match store.reconcile_unfinished(failure, now_ms()).await {
                    Ok(events) => {
                        if !events.is_empty() {
                            drop(wakeups.send(()));
                        }
                        ready.store(true, Ordering::SeqCst);
                        drop(health_tx.send(EngineHealth::Running));
                    }
                    Err(_) => {
                        drop(health_tx.send(EngineHealth::Failed(
                            "routine restart reconciliation failed".to_owned(),
                        )));
                        return;
                    }
                }
            }
            loop {
                tokio::select! {
                    biased;
                    () = task_cancellation.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(event) => admit_event(
                            &event,
                            &catalog,
                            &host,
                            &cache,
                            &active_children,
                            &child_admission_closed,
                            &destination_tasks,
                            &task_cancellation,
                            store.as_ref(),
                            &wakeups,
                            &failure_control,
                            &mut workers,
                        ).await,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::error!(skipped, "routine engine lagged; events were dropped");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            let message = "routine event source closed".to_owned();
                            tracing::error!(error_class = "event_source_closed", "{message}");
                            drop(health_tx.send(EngineHealth::Failed(message)));
                            task_cancellation.cancel();
                            break;
                        }
                    },
                    result = workers.join_next(), if !workers.is_empty() => {
                        if let Some(Err(error)) = result {
                            tracing::error!(%error, error_class = "worker_panic", "routine invocation worker exited unexpectedly");
                            drop(health_tx.send(EngineHealth::Failed(
                                "routine invocation worker exited unexpectedly".to_owned(),
                            )));
                        }
                    }
                }
            }

            // Cancellation wins first. Child IDs enter the owned set before create/submit, so a
            // racing or partially completed host operation can only leave an owned child behind.
            child_admission_closed.store(true, Ordering::SeqCst);
            if tokio::time::timeout(SHUTDOWN_WORKER_JOIN_TIMEOUT, async {
                while let Some(result) = workers.join_next().await {
                    if let Err(error) = result {
                        tracing::error!(%error, error_class = "worker_panic", "routine invocation worker exited during shutdown");
                    }
                }
            })
            .await
            .is_err()
            {
                tracing::error!(error_class = "worker_join_timeout", "routine workers did not stop before shutdown deadline");
                workers.abort_all();
                while workers.join_next().await.is_some() {}
            }
            // Fence new admissions before waiting; workers may need the lifecycle write gate
            // themselves to fail closed after a recording error.
            {
                let _lifecycle = lifecycle.write().await;
                ready.store(false, Ordering::SeqCst);
            }
            invocation_tasks.close_and_wait().await;
            destination_tasks.close_and_wait().await;
            let children = active_children.lock().await.drain().collect::<Vec<_>>();
            for child in &children {
                match tokio::time::timeout(SHUTDOWN_INTERRUPT_TIMEOUT, host.interrupt(child)).await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(session_id = %child.as_str(), %error, "failed to interrupt routine child during shutdown");
                    }
                    Err(_) => {
                        tracing::error!(session_id = %child.as_str(), error_class = "interrupt_timeout", "routine child interrupt exceeded shutdown deadline");
                    }
                }
            }
            if let Some(store) = &store {
                let failure = RoutineFailure {
                    code: "server_shutdown".to_owned(),
                    message: "routine interrupted by server shutdown".to_owned(),
                };
                if let Ok(events) = store.reconcile_unfinished(failure, now_ms()).await
                    && !events.is_empty()
                {
                    drop(wakeups.send(()));
                }
            }
        });
        Self {
            cancellation,
            task,
            health,
            manual,
            automatic,
            completion_host: None,
        }
    }

    /// Observe admission-worker liveness.
    #[must_use]
    pub fn health(&self) -> watch::Receiver<EngineHealth> {
        self.health.clone()
    }

    /// Return a cloneable manual invocation surface tied to this engine lifecycle.
    #[must_use]
    pub fn manual_handle(&self) -> ManualRoutineHandle {
        self.manual.clone()
    }

    #[cfg(test)]
    pub(super) fn completion_handler_for_test(
        &self,
    ) -> Arc<dyn SessionCompletionCoordinationHandler> {
        Arc::new(WeakAutomaticRoutineAdmission {
            inner: Arc::downgrade(&self.automatic),
        })
    }

    /// Subscribe to durable lifecycle commit wakeups. SQL remains authoritative.
    pub(crate) fn lifecycle_wakeups(&self) -> broadcast::Receiver<()> {
        self.manual.wakeups.subscribe()
    }

    /// Stop admission, interrupt active child runs, and join all workers.
    pub async fn shutdown(self) {
        self.cancellation.cancel();
        if let Some(host) = &self.completion_host {
            host.set_completion_admission_handler(None);
        }
        if let Err(error) = self.task.await {
            tracing::error!(%error, error_class = "engine_join", "routine engine failed to join");
        }
    }
}

impl SessionCompletionCoordinationHandler for WeakAutomaticRoutineAdmission {
    fn prepare(
        &self,
        snapshot: SessionRunCompletionSnapshot,
    ) -> SessionCompletionCoordinationFuture {
        let inner = self.inner.upgrade();
        Box::pin(async move {
            let Some(inner) = inner else {
                return Ok(SessionCompletionCoordinationPreparation::Continue);
            };
            inner.prepare(snapshot).await
        })
    }
}

impl AutomaticRoutineAdmission {
    async fn prepare(
        &self,
        snapshot: SessionRunCompletionSnapshot,
    ) -> Result<SessionCompletionCoordinationPreparation, String> {
        let manual = &self.manual;
        let _lifecycle = manual.lifecycle.read().await;
        if manual.cancellation.is_cancelled()
            || manual.child_admission_closed.load(Ordering::SeqCst)
            || !manual.ready.load(Ordering::SeqCst)
        {
            return Ok(SessionCompletionCoordinationPreparation::Continue);
        }
        let catalog = manual.catalog.snapshot();
        let Some((routine, trigger_name, context)) =
            select_automatic_invocation(&catalog, &snapshot)?
        else {
            return Ok(SessionCompletionCoordinationPreparation::Continue);
        };
        Ok(SessionCompletionCoordinationPreparation::Claim {
            reason: routine_pause_reason(routine.name(), "run-completed")?,
            finalizer: Box::new(AutomaticRoutineFinalizer {
                manual: manual.clone(),
                configuration: Arc::new(catalog.configuration().clone()),
                snapshot,
                routine,
                trigger_name,
                context,
            }),
        })
    }
}

impl SessionCompletionAdmissionFinalizer for AutomaticRoutineFinalizer {
    fn finalize(self: Box<Self>, lease: SessionPauseLease) -> SessionCompletionFinalizationFuture {
        Box::pin(async move {
            let Self {
                manual,
                snapshot,
                routine,
                trigger_name,
                mut context,
                configuration,
            } = *self;
            let _lifecycle = manual.lifecycle.read().await;
            let pause = super::host::RoutinePause::from_lease(lease);
            let capture = manual
                .host
                .capture_paused_effective(&pause)
                .await
                .map_err(|error| error.to_string())?;
            let lease = pause.into_lease();
            context.source_values = json!({
                "session": { "id": capture.source_id.as_str() },
                "pause": { "cursor": capture.through_cursor.get() },
            });
            context.source = Some(capture.clone());
            if manual.cancellation.is_cancelled()
                || manual.child_admission_closed.load(Ordering::SeqCst)
            {
                return Err("routine engine is shutting down".to_owned());
            }
            let store = manual
                .store
                .clone()
                .ok_or_else(|| "durable routine store is unavailable".to_owned())?;
            let identity = invocation_identity(
                routine.name(),
                routine.source_etag(),
                &trigger_name,
                &snapshot.run_id,
            );
            let invocation_id = format!("rinv_{}", uuid::Uuid::new_v4().simple());
            let admission = RoutineAdmission {
                invocation_id: invocation_id.clone(),
                execution_id: identity.clone(),
                routine_name: routine.name().to_owned(),
                routine_revision: routine.source_etag().to_owned(),
                trigger_name: trigger_name.clone(),
                trigger_kind: RoutineTriggerKind::Event,
                correlation_id: snapshot.run_id.clone(),
                metadata: BTreeMap::from([(
                    SOURCE_SESSION_METADATA_KEY.to_owned(),
                    snapshot.session_id.as_str().to_owned(),
                )]),
                source_cursor: Some(capture.through_cursor.get().to_string()),
                declared_tasks: task_descriptors(&routine),
                admitted_at_ms: now_ms(),
            };
            let reservation = manual
                .invocation_tasks
                .reserve()
                .ok_or_else(|| "routine engine is shutting down".to_owned())?;
            store
                .admit(admission)
                .await
                .map_err(|_| "routine admission persistence failed".to_owned())?;
            drop(manual.wakeups.send(()));

            let recorder =
                LifecycleRecorder::new(store, invocation_id.clone(), manual.wakeups.clone());
            let host = Arc::clone(&manual.host);
            let active_children = Arc::clone(&manual.active_children);
            let child_admission_closed = Arc::clone(&manual.child_admission_closed);
            let destination_tasks = manual.destination_tasks.clone();
            let cancellation = manual.cancellation.clone();
            let failure_control = manual.failure_control.clone();
            let mut invocation = AdmittedInvocation {
                configuration,
                invocation_id: Some(invocation_id),
                routine,
                trigger: trigger_name,
                context,
                identity,
                correlation_id: snapshot.run_id,
                pause: None,
            };
            let worker = Box::pin(async move {
                let _reservation = reservation;
                invocation.pause = Some(RoutinePauseGuard::new(
                    super::host::RoutinePause::from_lease(lease),
                    Arc::clone(&host),
                ));
                drop(
                    execute_and_finalize(
                        &mut invocation,
                        &host,
                        &active_children,
                        &child_admission_closed,
                        &destination_tasks,
                        &cancellation,
                        Some(&recorder),
                        &failure_control,
                    )
                    .await,
                );
            }) as waltr_agent::SessionCompletionWorkerFuture;
            Ok(worker)
        })
    }
}

pub(super) fn select_automatic_invocation(
    catalog: &crate::resource_catalog::AgentDefinitionSnapshot,
    snapshot: &SessionRunCompletionSnapshot,
) -> Result<Option<(MaterializedRoutine, String, RoutineInvocationContext)>, String> {
    if snapshot.purpose.as_deref() == Some("routine-task") {
        return Ok(None);
    }
    let context_limit = catalog
        .configuration()
        .providers
        .get(&snapshot.provider_key)
        .and_then(|provider| provider.models.get(&snapshot.model_id))
        .and_then(|model| model.limits)
        .and_then(|limits| limits.context)
        .map(u64::from);
    let percent = snapshot.context_tokens.and_then(|tokens| {
        context_limit.filter(|limit| *limit != 0).map(|limit| {
            u64::try_from((u128::from(tokens) * 100) / u128::from(limit)).unwrap_or(u64::MAX)
        })
    });
    let trigger_values = json!({
        "session": {
            "id": snapshot.session_id.as_str(),
            "latestCompletedCursor": snapshot.terminal_cursor.get(),
        },
        "run": { "id": snapshot.run_id },
        "terminalCursor": snapshot.terminal_cursor.get(),
        "context": {
            "tokens": snapshot.context_tokens,
            "percent": percent,
        },
    });

    for routine in catalog.routines() {
        for trigger in &routine.definition().spec.triggers {
            let Some(lifecycle) = &trigger.session_lifecycle else {
                continue;
            };
            if lifecycle.event
                != crate::resource_catalog::routine::RoutineSessionLifecycleEvent::RunCompleted
            {
                continue;
            }
            let params = lifecycle_params(routine.definition(), trigger, &trigger_values)?;
            let context = RoutineInvocationContext {
                event: None,
                trigger: trigger_values.clone(),
                params,
                source: None,
                source_values: Value::Null,
            };
            if conditions_match(&trigger.when, &context, &BTreeMap::new()) {
                return Ok(Some((routine.clone(), trigger.name.clone(), context)));
            }
        }
    }
    Ok(None)
}

fn lifecycle_params(
    definition: &crate::resource_catalog::routine::RoutineDefinition,
    trigger: &crate::resource_catalog::routine::RoutineTrigger,
    trigger_values: &Value,
) -> Result<BTreeMap<String, Value>, String> {
    let mut params = definition
        .spec
        .params
        .iter()
        .filter_map(|param| {
            param.default.as_ref().map(|value| {
                let value = match value {
                    crate::resource_catalog::routine::RoutineScalar::String(value) => {
                        Value::String(value.clone())
                    }
                    crate::resource_catalog::routine::RoutineScalar::Boolean(value) => {
                        Value::Bool(*value)
                    }
                    crate::resource_catalog::routine::RoutineScalar::Integer(value) => {
                        Value::Number((*value).into())
                    }
                };
                (param.name.clone(), value)
            })
        })
        .collect::<BTreeMap<_, _>>();
    let provisional = RoutineInvocationContext {
        event: None,
        trigger: trigger_values.clone(),
        params: params.clone(),
        source: None,
        source_values: Value::Null,
    };
    for assignment in &trigger.params {
        let mut value = super::expression::substitute_scalar(
            &assignment.value,
            &provisional,
            &BTreeMap::new(),
            None,
            None,
        )
        .map_err(|error| error.message)?;
        if definition
            .spec
            .params
            .iter()
            .find(|param| param.name == assignment.name)
            .is_some_and(|param| {
                param.r#type == crate::resource_catalog::routine::RoutineResultType::Boolean
            })
        {
            value = match value.as_str() {
                Some("true") => Value::Bool(true),
                Some("false") => Value::Bool(false),
                _ => value,
            };
        }
        params.insert(assignment.name.clone(), value);
    }
    Ok(params)
}

pub(crate) struct InteractionOwnership {
    pub store: RoutineInvocationStore,
    pub wakeups: broadcast::Sender<()>,
    pub cancellation: CancellationToken,
    failure_control: EngineFailureControl,
}

impl InteractionOwnership {
    pub(crate) async fn persistence_failed(&self) {
        self.failure_control.fail_closed().await;
    }
}

impl ManualRoutineHandle {
    /// Reserve engine ownership before any asynchronous durable admission can be abandoned.
    pub(crate) async fn own_interaction<F, Fut>(&self, make: F) -> Result<(), InvocationError>
    where
        F: FnOnce(InteractionOwnership) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut health = self.failure_control.health.subscribe();
        tokio::select! { biased;
            () = self.cancellation.cancelled() => return Err(InvocationError::new("shutdown", "routine engine is shutting down")),
            result = health.wait_for(|value| !matches!(value, EngineHealth::Reconciling)) => {
                if result.is_err() { return Err(InvocationError::new("shutdown", "routine engine health closed")); }
            }
        }
        let _lifecycle = self.lifecycle.read().await;
        if self.cancellation.is_cancelled() || self.child_admission_closed.load(Ordering::SeqCst) {
            return Err(InvocationError::new(
                "shutdown",
                "routine engine is shutting down",
            ));
        }
        if !self.ready.load(Ordering::SeqCst) {
            return Err(InvocationError::new(
                "reconciling",
                "routine engine is not ready",
            ));
        }
        let store = self.store.clone().ok_or_else(|| {
            InvocationError::new(
                "routine_persistence",
                "durable routine store is unavailable",
            )
        })?;
        let ownership = InteractionOwnership {
            store,
            wakeups: self.wakeups.clone(),
            cancellation: self.cancellation.clone(),
            failure_control: self.failure_control.clone(),
        };
        self.invocation_tasks.spawn(tracing::Instrument::instrument(
            make(ownership),
            tracing::Span::current(),
        ));
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn lifecycle_gate_for_test(&self) -> Arc<tokio::sync::RwLock<()>> {
        Arc::clone(&self.lifecycle)
    }

    #[cfg(test)]
    pub(super) fn fail_closed_pending_for_test(&self) -> bool {
        self.failure_control
            .fail_closed_pending
            .load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn event_admission_waiters_for_test(&self) -> usize {
        self.failure_control
            .event_admission_waiters
            .load(Ordering::SeqCst)
    }

    /// Validate and durably admit a detached manual invocation.
    pub async fn start(&self, request: ManualRoutineInvocation) -> Result<String, InvocationError> {
        let _lifecycle = self.lifecycle.read().await;
        if !self.ready.load(Ordering::SeqCst) {
            return Err(InvocationError::new(
                "reconciling",
                "routine engine is reconciling durable state",
            ));
        }
        let store = self.store.clone().ok_or_else(|| {
            InvocationError::new(
                "routine_persistence",
                "durable routine store is unavailable",
            )
        })?;
        let mut invocation = self.prepare(request).await?;
        let invocation_id = format!("rinv_{}", uuid::Uuid::new_v4().simple());
        invocation.invocation_id = Some(invocation_id.clone());
        let admission = RoutineAdmission {
            invocation_id: invocation_id.clone(),
            execution_id: invocation.identity.clone(),
            routine_name: invocation.routine.name().to_owned(),
            routine_revision: invocation.routine.source_etag().to_owned(),
            trigger_name: invocation.trigger.clone(),
            trigger_kind: RoutineTriggerKind::Manual,
            correlation_id: invocation.correlation_id.clone(),
            metadata: invocation
                .context
                .trigger
                .get("session")
                .and_then(|session| session.get("id"))
                .and_then(Value::as_str)
                .map(|source| {
                    BTreeMap::from([(SOURCE_SESSION_METADATA_KEY.to_owned(), source.to_owned())])
                })
                .unwrap_or_default(),
            source_cursor: invocation
                .context
                .source
                .as_ref()
                .map(|capture| capture.through_cursor.get().to_string()),
            declared_tasks: task_descriptors(&invocation.routine),
            admitted_at_ms: now_ms(),
        };
        if store.admit(admission).await.is_err() {
            invocation.release_pause().await;
            return Err(InvocationError::new(
                "routine_persistence",
                "routine admission persistence failed",
            ));
        }
        drop(self.wakeups.send(()));
        let recorder = LifecycleRecorder::new(store, invocation_id.clone(), self.wakeups.clone());
        let host = Arc::clone(&self.host);
        let active_children = Arc::clone(&self.active_children);
        let child_admission_closed = Arc::clone(&self.child_admission_closed);
        let destination_tasks = self.destination_tasks.clone();
        let cancellation = self.cancellation.clone();
        let failure_control = self.failure_control.clone();
        self.invocation_tasks.spawn(async move {
            drop(
                execute_and_finalize(
                    &mut invocation,
                    &host,
                    &active_children,
                    &child_admission_closed,
                    &destination_tasks,
                    &cancellation,
                    Some(&recorder),
                    &failure_control,
                )
                .await,
            );
        });
        Ok(invocation_id)
    }

    /// Test-only seam for executor and host-boundary regression coverage.
    #[cfg(test)]
    #[allow(
        clippy::too_many_lines,
        reason = "manual admission pins and validates all immutable invocation inputs before execution"
    )]
    pub(super) async fn invoke_for_test(
        &self,
        request: ManualRoutineInvocation,
    ) -> Result<Vec<RoutineInvocationResult>, InvocationError> {
        let _lifecycle = self.lifecycle.read().await;
        let mut invocation = self.prepare(request).await?;
        execute_invocation(
            &mut invocation,
            &self.host,
            &self.active_children,
            &self.child_admission_closed,
            &self.destination_tasks,
            &self.cancellation,
            None,
        )
        .await
    }

    async fn prepare(
        &self,
        request: ManualRoutineInvocation,
    ) -> Result<AdmittedInvocation, InvocationError> {
        if self.cancellation.is_cancelled() || self.child_admission_closed.load(Ordering::SeqCst) {
            return Err(InvocationError::new(
                "shutdown",
                "routine engine is shutting down",
            ));
        }
        let snapshot = self.catalog.snapshot();
        let routine = snapshot
            .routine(&request.routine_name)
            .cloned()
            .ok_or_else(|| {
                InvocationError::new("routine_not_found", "manual routine was not found")
            })?;
        let trigger = routine
            .definition()
            .spec
            .triggers
            .iter()
            .find(|trigger| trigger.name == request.trigger_name && trigger.manual.is_some())
            .ok_or_else(|| {
                InvocationError::new("trigger_not_found", "manual routine trigger was not found")
            })?;
        let trigger_name = trigger.name.clone();
        let manual = trigger.manual.as_ref().ok_or_else(|| {
            InvocationError::new("trigger_not_found", "manual routine trigger was not found")
        })?;
        let params = validate_manual_inputs(routine.definition(), manual, &request.inputs)?;
        let provisional = RoutineInvocationContext {
            event: None,
            trigger: json!({"session": {"id": request.session_id.as_str()}}),
            params: params.clone(),
            source: None,
            source_values: Value::Null,
        };
        let source_id = if let Some(source) = &routine.definition().spec.source_session {
            let value = super::expression::substitute_scalar(
                &source.from,
                &provisional,
                &BTreeMap::new(),
                None,
                None,
            )?;
            let id = value
                .as_str()
                .or_else(|| value.get("id").and_then(Value::as_str))
                .map(SessionId::new)
                .ok_or_else(|| {
                    InvocationError::new("source_capture", "sourceSession.from is not a session")
                })?;
            if id != request.session_id {
                return Err(InvocationError::new(
                    "source_capture",
                    "sourceSession.from does not identify the invocation session",
                ));
            }
            Some(id)
        } else {
            None
        };
        let mut pause = if routine
            .definition()
            .spec
            .source_session
            .as_ref()
            .is_some_and(|source| source.pause.is_some())
        {
            let acquired = tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return Err(InvocationError::new(
                    "shutdown", "routine pause acquisition canceled during shutdown"
                )),
                acquired = self.host.acquire_pause(
                    source_id.as_ref().expect("source id"),
                    routine_pause_reason(routine.name(), "manual")
                        .map_err(|error| InvocationError::new("pause_reason", error))?,
                ) => acquired,
            }
            .map_err(pause_admission_error)?;
            Some(RoutinePauseGuard::new(acquired, Arc::clone(&self.host)))
        } else {
            None
        };
        let capture_result = if let Some(guard) = &pause {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(InvocationError::new(
                    "shutdown", "routine source capture canceled during shutdown"
                )),
                capture = self.host.capture_paused_effective(guard.pause()) => {
                    capture
                        .map(Some)
                        .map_err(pause_admission_error)
                },
            }
        } else if let Some(source_id) = &source_id {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(InvocationError::new(
                    "shutdown", "routine source capture canceled during shutdown"
                )),
                capture = self.host.capture_latest_completed(source_id) => {
                    capture.map(Some).map_err(capture_admission_error)
                },
            }
        } else {
            Ok(None)
        };
        let capture = match capture_result {
            Ok(capture) => capture,
            Err(error) => {
                if let Some(guard) = &mut pause {
                    if let Err(release) = guard.release().await {
                        tracing::warn!(%release, "failed to release Routine pause after capture failure");
                    }
                }
                return Err(error);
            }
        };
        let cursor = capture.as_ref().map(|capture| capture.through_cursor.get());
        let trigger_values = json!({
            "session": {
                "id": request.session_id.as_str(),
                "latestCompletedCursor": cursor,
            }
        });
        let provisional = RoutineInvocationContext {
            event: None,
            trigger: trigger_values.clone(),
            params: params.clone(),
            source: None,
            source_values: Value::Null,
        };
        if let (Some(source_spec), Some(capture)) =
            (&routine.definition().spec.source_session, &capture)
        {
            let cursor_matches = source_spec.snapshot.as_ref().is_none_or(|snapshot| {
                super::expression::substitute_scalar(
                    &snapshot.through_cursor,
                    &provisional,
                    &BTreeMap::new(),
                    None,
                    None,
                )
                .is_ok_and(|cursor| cursor.as_u64() == Some(capture.through_cursor.get()))
            });
            if source_id.as_ref() != Some(&capture.source_id) || !cursor_matches {
                return Err(InvocationError::new(
                    "source_capture",
                    "sourceSession expressions do not identify the admitted capture",
                ));
            }
        }
        let source_values = capture.as_ref().map_or(Value::Null, |capture| {
            if pause.is_some() {
                json!({
                    "session": { "id": capture.source_id.as_str() },
                    "pause": { "cursor": capture.through_cursor.get() },
                })
            } else {
                json!({
                    "session": { "id": capture.source_id.as_str() },
                    "snapshot": { "cursor": capture.through_cursor.get() },
                })
            }
        });
        let context = RoutineInvocationContext {
            event: None,
            trigger: trigger_values,
            params,
            source: capture,
            source_values,
        };
        Ok(AdmittedInvocation {
            configuration: Arc::new(snapshot.configuration().clone()),
            invocation_id: None,
            routine,
            trigger: trigger_name,
            context,
            identity: format!("manual_{}", uuid::Uuid::new_v4()),
            correlation_id: request.request_id,
            pause,
        })
    }
}

fn task_descriptors(routine: &MaterializedRoutine) -> Vec<RoutineTaskDescriptor> {
    routine
        .definition()
        .spec
        .tasks
        .iter()
        .map(|task| RoutineTaskDescriptor {
            name: task.name.clone(),
            task_type: if task.task_spec.session_template.is_some() {
                "session_template"
            } else if task
                .task_spec
                .action
                .as_ref()
                .is_some_and(|action| action.session_create.is_some())
            {
                "session_create"
            } else if task
                .task_spec
                .action
                .as_ref()
                .is_some_and(|action| action.session_mask.is_some())
            {
                "session_mask"
            } else if task
                .task_spec
                .action
                .as_ref()
                .is_some_and(|action| action.session_continue.is_some())
            {
                "session_continue"
            } else {
                "session_metadata"
            }
            .to_owned(),
        })
        .collect()
}

fn validate_manual_inputs(
    definition: &crate::resource_catalog::routine::RoutineDefinition,
    trigger: &crate::resource_catalog::routine::RoutineManualTrigger,
    inputs: &BTreeMap<String, RoutineValue>,
) -> Result<BTreeMap<String, Value>, InvocationError> {
    validate_scalar_inputs(definition, &trigger.inputs, inputs)
}

pub(crate) fn validate_scalar_inputs(
    definition: &crate::resource_catalog::routine::RoutineDefinition,
    declarations: &[crate::resource_catalog::routine::RoutineManualInput],
    inputs: &BTreeMap<String, RoutineValue>,
) -> Result<BTreeMap<String, Value>, InvocationError> {
    let exposed = declarations
        .iter()
        .map(|input| input.name.as_str())
        .collect::<HashSet<_>>();
    if let Some(name) = inputs.keys().find(|name| !exposed.contains(name.as_str())) {
        return Err(InvocationError::new(
            "input",
            format!("input `{name}` is not declared by the trigger"),
        ));
    }
    let mut values = BTreeMap::new();
    for param in &definition.spec.params {
        if !exposed.contains(param.name.as_str()) {
            continue;
        }
        let value = inputs.get(&param.name).cloned().or_else(|| {
            param.default.as_ref().map(|value| match value {
                crate::resource_catalog::routine::RoutineScalar::String(value) => {
                    RoutineValue::String(value.clone())
                }
                crate::resource_catalog::routine::RoutineScalar::Boolean(value) => {
                    RoutineValue::Boolean(*value)
                }
                crate::resource_catalog::routine::RoutineScalar::Integer(_) => {
                    unreachable!("Routine parameters admit only string and boolean defaults")
                }
            })
        });
        let Some(value) = value else {
            if param.required {
                return Err(InvocationError::new(
                    "input",
                    format!("required input `{}` is missing", param.name),
                ));
            }
            continue;
        };
        let valid_type = matches!(
            (&param.r#type, &value),
            (
                crate::resource_catalog::routine::RoutineResultType::String,
                RoutineValue::String(_)
            ) | (
                crate::resource_catalog::routine::RoutineResultType::Boolean,
                RoutineValue::Boolean(_)
            )
        );
        if !valid_type {
            return Err(InvocationError::new(
                "input",
                format!("input `{}` has the wrong type", param.name),
            ));
        }
        if let (RoutineValue::String(value), Some(validation)) = (&value, &param.validation) {
            if validation
                .min_length
                .is_some_and(|minimum| value.chars().count() < minimum)
            {
                return Err(InvocationError::new(
                    "input",
                    format!("input `{}` is shorter than minLength", param.name),
                ));
            }
            if let Some(pattern) = &validation.pattern
                && !crate::resource_catalog::routine::pattern_matches(pattern, value)
            {
                return Err(InvocationError::new(
                    "input",
                    format!("input `{}` does not match pattern", param.name),
                ));
            }
        }
        if let RoutineValue::String(value) = &value
            && value.len() > super::expression::MAX_EXPANDED_STRING_BYTES
        {
            return Err(InvocationError::new(
                "input_exhausted",
                format!("input `{}` exceeds the scalar size limit", param.name),
            ));
        }
        values.insert(param.name.clone(), value.into_json());
    }
    Ok(values)
}

#[allow(
    clippy::too_many_arguments,
    reason = "admission receives bounded shared engine state"
)]
#[allow(
    clippy::too_many_lines,
    reason = "event admission keeps pinned invocation construction and worker logging together"
)]
async fn admit_event(
    event: &EventEnvelope,
    catalog: &AgentDefinitionState,
    host: &Arc<dyn RoutineHost>,
    cache: &Arc<Mutex<InvocationCache>>,
    active_children: &Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
    child_admission_closed: &Arc<AtomicBool>,
    destination_tasks: &DestinationTaskTracker,
    cancellation: &CancellationToken,
    store: Option<&RoutineInvocationStore>,
    wakeups: &broadcast::Sender<()>,
    failure_control: &EngineFailureControl,
    workers: &mut tokio::task::JoinSet<()>,
) {
    let snapshot = catalog.snapshot();
    for routine in snapshot.routines() {
        for trigger in &routine.definition().spec.triggers {
            let Some(event_trigger) = &trigger.event else {
                continue;
            };
            let context = RoutineInvocationContext {
                event: Some(event.clone()),
                trigger: Value::Null,
                params: BTreeMap::new(),
                source: None,
                source_values: Value::Null,
            };
            if event_trigger.r#type != event.r#type
                || !conditions_match(&trigger.when, &context, &BTreeMap::new())
            {
                continue;
            }
            let identity = invocation_identity(
                routine.name(),
                routine.source_etag(),
                &trigger.name,
                &event.id,
            );
            if !cache
                .lock()
                .expect("invocation cache poisoned")
                .admit(&identity)
            {
                tracing::debug!(
                    routine = routine.name(),
                    trigger = trigger.name,
                    event_id = event.id,
                    invocation_id = identity,
                    status = "duplicate",
                    "routine invocation suppressed"
                );
                continue;
            }
            #[cfg(test)]
            failure_control
                .event_admission_waiters
                .fetch_add(1, Ordering::SeqCst);
            let _admission = failure_control.lifecycle.read().await;
            #[cfg(test)]
            failure_control
                .event_admission_waiters
                .fetch_sub(1, Ordering::SeqCst);
            if !failure_control.ready.load(Ordering::SeqCst)
                || failure_control.cancellation.is_cancelled()
                || failure_control
                    .child_admission_closed
                    .load(Ordering::SeqCst)
            {
                cache
                    .lock()
                    .expect("invocation cache poisoned")
                    .finish(&identity);
                continue;
            }
            let mut admitted = AdmittedInvocation {
                configuration: Arc::new(snapshot.configuration().clone()),
                invocation_id: None,
                routine: routine.clone(),
                trigger: trigger.name.clone(),
                context,
                identity: identity.clone(),
                correlation_id: event.id.clone(),
                pause: None,
            };
            let recorder = if let Some(store) = store {
                let invocation_id = format!("rinv_{}", uuid::Uuid::new_v4().simple());
                admitted.invocation_id = Some(invocation_id.clone());
                let admission = RoutineAdmission {
                    invocation_id: invocation_id.clone(),
                    execution_id: identity.clone(),
                    routine_name: admitted.routine.name().to_owned(),
                    routine_revision: admitted.routine.source_etag().to_owned(),
                    trigger_name: admitted.trigger.clone(),
                    trigger_kind: RoutineTriggerKind::Event,
                    correlation_id: admitted.correlation_id.clone(),
                    metadata: event
                        .source_session_id
                        .as_ref()
                        .map(|source| {
                            BTreeMap::from([(
                                SOURCE_SESSION_METADATA_KEY.to_owned(),
                                source.clone(),
                            )])
                        })
                        .unwrap_or_default(),
                    source_cursor: None,
                    declared_tasks: task_descriptors(&admitted.routine),
                    admitted_at_ms: now_ms(),
                };
                if store.admit(admission).await.is_err() {
                    tracing::error!(
                        routine = admitted.routine.name(),
                        trigger = admitted.trigger,
                        error_class = "routine_persistence",
                        "routine event admission persistence failed"
                    );
                    cache
                        .lock()
                        .expect("invocation cache poisoned")
                        .finish(&identity);
                    continue;
                }
                drop(wakeups.send(()));
                Some(LifecycleRecorder::new(
                    store.clone(),
                    invocation_id,
                    wakeups.clone(),
                ))
            } else {
                None
            };
            let host = Arc::clone(host);
            let cache = Arc::clone(cache);
            let active_children = Arc::clone(active_children);
            let child_admission_closed = Arc::clone(child_admission_closed);
            let destination_tasks = destination_tasks.clone();
            let cancellation = cancellation.clone();
            let failure_control = failure_control.clone();
            workers.spawn(async move {
                let started = Instant::now();
                let result = execute_and_finalize(
                    &mut admitted,
                    &host,
                    &active_children,
                    &child_admission_closed,
                    &destination_tasks,
                    &cancellation,
                    recorder.as_ref(),
                    &failure_control,
                )
                .await;
                cache
                    .lock()
                    .expect("invocation cache poisoned")
                    .finish(&identity);
                match result {
                    Ok(_) => tracing::info!(
                        routine = admitted.routine.name(),
                        trigger = admitted.trigger,
                        correlation_id = admitted.correlation_id,
                        invocation_id = admitted.identity,
                        status = "succeeded",
                        duration_ms = started.elapsed().as_millis(),
                        "routine invocation completed"
                    ),
                    Err(error) => tracing::warn!(
                        routine = admitted.routine.name(),
                        trigger = admitted.trigger,
                        correlation_id = admitted.correlation_id,
                        invocation_id = admitted.identity,
                        status = "failed",
                        error_class = error.class,
                        duration_ms = started.elapsed().as_millis(),
                        message = %error.message,
                        "routine invocation failed"
                    ),
                }
            });
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared worker wrapper owns the canonical executor boundaries"
)]
async fn execute_and_finalize(
    invocation: &mut AdmittedInvocation,
    host: &Arc<dyn RoutineHost>,
    active_children: &Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
    child_admission_closed: &Arc<AtomicBool>,
    destination_tasks: &DestinationTaskTracker,
    cancellation: &CancellationToken,
    recorder: Option<&LifecycleRecorder>,
    failure_control: &EngineFailureControl,
) -> Result<Vec<RoutineInvocationResult>, InvocationError> {
    let result = execute_invocation(
        invocation,
        host,
        active_children,
        child_admission_closed,
        destination_tasks,
        cancellation,
        recorder,
    )
    .await;
    if result
        .as_ref()
        .is_err_and(|error| error.class == "routine_persistence")
        && let Some(recorder) = recorder
        && recorder
            .interrupted(
                "persistence_failure",
                "routine interrupted after lifecycle persistence failure",
            )
            .await
            .is_err()
    {
        failure_control.fail_closed().await;
    }
    result
}

pub(super) struct AdmittedInvocation {
    pub(super) configuration: Arc<waltr::grpc::waltr::config::v1::Configuration>,
    pub(super) invocation_id: Option<String>,
    pub(super) routine: MaterializedRoutine,
    pub(super) trigger: String,
    pub(super) context: RoutineInvocationContext,
    pub(super) identity: String,
    pub(super) correlation_id: String,
    pub(super) pause: Option<RoutinePauseGuard>,
}

impl AdmittedInvocation {
    async fn release_pause(&mut self) {
        if let Some(guard) = &mut self.pause
            && let Err(error) = guard.release().await
        {
            tracing::warn!(%error, "failed to release Routine pause");
        }
    }
}

fn capture_admission_error(error: waltr_agent::SessionCaptureError) -> InvocationError {
    let class = match &error {
        waltr_agent::SessionCaptureError::FailedPrecondition(_) => "source_precondition",
        waltr_agent::SessionCaptureError::ResourceExhausted(_) => "source_exhausted",
        waltr_agent::SessionCaptureError::Store(waltr_agent::SessionStoreError::NotFound {
            ..
        }) => "source_not_found",
        waltr_agent::SessionCaptureError::Store(_) => "source_store",
    };
    InvocationError::new(class, error.to_string())
}

fn pause_admission_error(error: waltr_agent::SessionPauseError) -> InvocationError {
    let class = match error {
        waltr_agent::SessionPauseError::Active { .. }
        | waltr_agent::SessionPauseError::Paused { .. }
        | waltr_agent::SessionPauseError::LifecycleBusy { .. } => "source_precondition",
        waltr_agent::SessionPauseError::Unavailable { .. } => "shutdown",
        waltr_agent::SessionPauseError::CompletedBoundaryRequired { .. } => "source_precondition",
        waltr_agent::SessionPauseError::Capture(
            waltr_agent::SessionCaptureError::ResourceExhausted(_),
        ) => "source_exhausted",
        waltr_agent::SessionPauseError::Capture(waltr_agent::SessionCaptureError::Store(
            waltr_agent::SessionStoreError::NotFound { .. },
        )) => "source_not_found",
        _ => "source_capture",
    };
    InvocationError::new(class, error.to_string())
}

fn invocation_identity(routine: &str, etag: &str, trigger: &str, event_id: &str) -> String {
    let mut digest = Sha256::new();
    for value in [routine, etag, trigger, event_id] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    format!("routine_{}", hex_digest(&digest.finalize()))
}

pub(super) fn routine_pause_reason(
    routine_name: &str,
    trigger: &str,
) -> Result<SessionCoordinationReason, String> {
    let code = if routine_name == "compact-and-continue" {
        "context-compaction"
    } else {
        "host-operation"
    };
    SessionCoordinationReason::new(
        code,
        Some(routine_name.to_owned()),
        Some(trigger.to_owned()),
    )
    .map_err(|error| error.to_string())
}

pub(super) fn short_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex_digest(&digest[..16])
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        },
    )
}

#[derive(Default)]
// Terminal identities are intentionally process-local and bounded. A duplicate arriving after
// TTL/capacity eviction can run again; running identities are never evicted.
pub(super) struct InvocationCache {
    entries: HashMap<String, Option<Instant>>,
    terminal_order: VecDeque<(String, Instant)>,
}

impl InvocationCache {
    pub(super) fn admit(&mut self, identity: &str) -> bool {
        self.expire();
        if self.entries.contains_key(identity) {
            return false;
        }
        self.entries.insert(identity.to_owned(), None);
        true
    }

    pub(super) fn finish(&mut self, identity: &str) {
        let now = Instant::now();
        self.entries.insert(identity.to_owned(), Some(now));
        self.terminal_order.push_back((identity.to_owned(), now));
        self.expire();
    }

    fn expire(&mut self) {
        while let Some((identity, completed)) = self.terminal_order.front() {
            let over_capacity = self.terminal_order.len() > TERMINAL_CACHE_CAPACITY;
            if !over_capacity && completed.elapsed() < TERMINAL_CACHE_TTL {
                break;
            }
            let identity = identity.clone();
            let completed = *completed;
            self.terminal_order.pop_front();
            if self.entries.get(&identity) == Some(&Some(completed)) {
                self.entries.remove(&identity);
            }
        }
    }
}
