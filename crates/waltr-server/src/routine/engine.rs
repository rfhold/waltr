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

use waltr_agent::{CompletedSessionCapture, SessionId};
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
    host::{CoordinatorRoutineHost, RoutineHost},
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
    pub(super) fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("destination tracker poisoned");
            assert!(!state.closed, "destination tracker is closed");
            state.active += 1;
        }
        let guard = DestinationTaskGuard {
            inner: Arc::clone(&self.inner),
        };
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
        Self::start_inner(
            source.subscribe(),
            catalog,
            Arc::new(CoordinatorRoutineHost::new(sessions)),
            Some(store),
        )
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
            let children = {
                let _lifecycle = lifecycle.write().await;
                invocation_tasks.close_and_wait().await;
                destination_tasks.close_and_wait().await;
                active_children.lock().await.drain().collect::<Vec<_>>()
            };
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

    /// Subscribe to durable lifecycle commit wakeups. SQL remains authoritative.
    pub(crate) fn lifecycle_wakeups(&self) -> broadcast::Receiver<()> {
        self.manual.wakeups.subscribe()
    }

    /// Stop admission, interrupt active child runs, and join all workers.
    pub async fn shutdown(self) {
        self.cancellation.cancel();
        if let Err(error) = self.task.await {
            tracing::error!(%error, error_class = "engine_join", "routine engine failed to join");
        }
    }
}

impl ManualRoutineHandle {
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
        let invocation = self.prepare(request).await?;
        let invocation_id = format!("rinv_{}", uuid::Uuid::new_v4().simple());
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
        store.admit(admission).await.map_err(|_| {
            InvocationError::new(
                "routine_persistence",
                "routine admission persistence failed",
            )
        })?;
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
                    &invocation,
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
        let invocation = self.prepare(request).await?;
        execute_invocation(
            &invocation,
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
        let capture = if routine.definition().spec.source_session.is_some() {
            Some(tokio::select! {
                biased;
                () = self.cancellation.cancelled() => return Err(InvocationError::new(
                    "shutdown",
                    "routine source capture canceled during shutdown",
                )),
                capture = self.host.capture_latest_completed(&request.session_id) => capture
                    .map_err(|error| {
                        let class = match &error {
                            waltr_agent::SessionCaptureError::FailedPrecondition(_) => {
                                "source_precondition"
                            }
                            waltr_agent::SessionCaptureError::ResourceExhausted(_) => {
                                "source_exhausted"
                            }
                            waltr_agent::SessionCaptureError::Store(
                                waltr_agent::SessionStoreError::NotFound { .. },
                            ) => "source_not_found",
                            waltr_agent::SessionCaptureError::Store(_) => "source_store",
                        };
                        InvocationError::new(class, error.to_string())
                    })?,
            })
        } else {
            None
        };
        let cursor = capture
            .as_ref()
            .map(|capture| capture.through_cursor.get());
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
            let source_id = super::expression::substitute_scalar(
                &source_spec.from,
                &provisional,
                &BTreeMap::new(),
                None,
                None,
            )?;
            let source_id = source_id
                .as_str()
                .or_else(|| source_id.get("id").and_then(Value::as_str));
            let snapshot = source_spec.snapshot.as_ref().ok_or_else(|| {
                InvocationError::new(
                    "source_capture",
                    "paused source execution is not supported",
                )
            })?;
            let cursor = super::expression::substitute_scalar(
                &snapshot.through_cursor,
                &provisional,
                &BTreeMap::new(),
                None,
                None,
            )?;
            if source_id != Some(capture.source_id.as_str())
                || cursor.as_u64() != Some(capture.through_cursor.get())
            {
                return Err(InvocationError::new(
                    "source_capture",
                    "sourceSession expressions do not identify the admitted capture",
                ));
            }
        }
        let source_values = capture.as_ref().map_or(Value::Null, |capture| json!({
            "session": { "id": capture.source_id.as_str() },
            "snapshot": { "cursor": capture.through_cursor.get() },
        }));
        let context = RoutineInvocationContext {
            event: None,
            trigger: trigger_values,
            params,
            source: capture,
            source_values,
        };
        Ok(AdmittedInvocation {
            routine,
            trigger: trigger_name,
            context,
            identity: format!("manual_{}", uuid::Uuid::new_v4()),
            correlation_id: request.request_id,
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
    let exposed = trigger
        .inputs
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
            let admitted = AdmittedInvocation {
                routine: routine.clone(),
                trigger: trigger.name.clone(),
                context,
                identity: identity.clone(),
                correlation_id: event.id.clone(),
            };
            let recorder = if let Some(store) = store {
                let invocation_id = format!("rinv_{}", uuid::Uuid::new_v4().simple());
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
                    &admitted,
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
    invocation: &AdmittedInvocation,
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
    pub(super) routine: MaterializedRoutine,
    pub(super) trigger: String,
    pub(super) context: RoutineInvocationContext,
    pub(super) identity: String,
    pub(super) correlation_id: String,
}

fn invocation_identity(routine: &str, etag: &str, trigger: &str, event_id: &str) -> String {
    let mut digest = Sha256::new();
    for value in [routine, etag, trigger, event_id] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    format!("routine_{}", hex_digest(&digest.finalize()))
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
