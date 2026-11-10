//! Durable agent session coordinator.

#![allow(clippy::significant_drop_tightening)]

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::{
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, LlmError, PendingTimelineEvent, Result,
    SessionRecord, SessionJournalEvent, TimelineContent, TimelineItemId, TimelineItemText,
    TimelineQuery,
};
use futures_util::StreamExt as _;
use serde_json::Value;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// Admission or execution outcome for an exclusive, provider-free host effect.
#[derive(Debug)]
pub enum IdleHostEffectError {
    Busy,
    Stale,
    Canceled,
    Effect(LlmError),
}
impl std::fmt::Display for IdleHostEffectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "idle host effect: {self:?}")
    }
}
impl std::error::Error for IdleHostEffectError {}
impl From<LlmError> for IdleHostEffectError {
    fn from(error: LlmError) -> Self {
        Self::Effect(error)
    }
}

#[cfg(test)]
#[path = "idle_host_tests.rs"]
mod idle_host_tests;
use tracing::Instrument as _;

use super::run::SessionRunInferenceIdentity;
use super::{
    AgentRunLimits, AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntime, AgentSessionRuntimeFactory, CompletedSessionCapture, InferenceSettings,
    MetadataSetCondition, MetadataSetOutcome, ResumeInput, RunConflictPolicy, RunOutcomeSender,
    RunResult, SessionCaptureError, SessionCaptureLimits, SessionControl, SessionCoordinationPhase,
    SessionCoordinationReason, SessionCoordinationState, SessionCreateRequest, SessionId,
    SessionListSubscription, SessionMaskMutation, SessionMaskMutationOutcome, SessionPauseError,
    SessionPauseId, SessionPauseLease, SessionRunContext, SessionStatus, SessionStore,
    SessionSummary, SessionTimelineBroadcaster, SessionTimelineMode, SessionTimelinePage,
    SessionTimelineProjectionSubscription, SessionTimelineSubscription,
};
use crate::agent::{AgentRunBuilder, AgentRuntime, AgentSession};
use crate::resource::{SessionTemplateOption, SessionTemplateSearchRequest};

/// Durable session coordinator bound to a store.
#[derive(Clone)]
pub struct SessionCoordinator<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    store: S,
    control: SessionControl,
    timeline: SessionTimelineBroadcaster,
    runtime_factory: Option<AgentSessionRuntimeFactory>,
    active_runs: Arc<Mutex<RunCoordination>>,
    run_drain: Arc<RunDrain>,
    next_run_id: Arc<AtomicU64>,
    next_pause_id: Arc<AtomicU64>,
    event_sink: Option<Arc<dyn crate::SessionEventSink>>,
    completion_handler: Arc<StdMutex<Option<Arc<dyn super::SessionCompletionCoordinationHandler>>>>,
    shutdown_token: CancellationToken,
    shutdown_completion: Arc<AsyncCompletion>,
    shutdown_task: Arc<StdMutex<Option<tokio::task::JoinHandle<()>>>>,
    #[cfg(test)]
    list_registration_barrier:
        Arc<StdMutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>,
}

impl<S> std::fmt::Debug for SessionCoordinator<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCoordinator")
            .field("runtime_factory", &self.runtime_factory)
            .field("event_sink", &self.event_sink.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct ActiveSessionRun {
    id: u64,
    token: CancellationToken,
    inference_identity: Option<SessionRunInferenceIdentity>,
}

#[derive(Debug, Default)]
struct RunCoordination {
    sessions: BTreeMap<SessionId, SessionRunState>,
    wiping: bool,
    shutdown: bool,
}

#[derive(Debug, Default)]
struct AsyncCompletion {
    complete: AtomicBool,
    completed: Notify,
}

impl AsyncCompletion {
    fn finish(&self) {
        self.complete.store(true, Ordering::Release);
        self.completed.notify_waiters();
    }

    fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let notified = self.completed.notified();
            if self.is_complete() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Default)]
struct RunDrain {
    active: AtomicU64,
    idle: Notify,
}

impl RunDrain {
    fn begin(self: &Arc<Self>) -> RunActivity {
        self.active.fetch_add(1, Ordering::AcqRel);
        RunActivity {
            drain: Arc::clone(self),
        }
    }

    async fn wait_until_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
struct RunActivity {
    drain: Arc<RunDrain>,
}

impl Drop for RunActivity {
    fn drop(&mut self) {
        if self.drain.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drain.idle.notify_waiters();
        }
    }
}

#[derive(Debug, Default)]
struct SessionRunState {
    active: Option<ActiveSessionRun>,
    completion_barrier: Option<CompletionBarrier>,
    pause: Option<OwnedSessionPause>,
    queued: VecDeque<QueuedSessionRun>,
    blocked: bool,
}

#[derive(Debug)]
struct CompletionBarrier {
    run_id: u64,
    reason: SessionCoordinationReason,
}

#[derive(Debug)]
struct OwnedSessionPause {
    id: SessionPauseId,
    source_cursor: crate::TimelineCursor,
    timeline_boundary: crate::TimelineCursor,
    mutation_cursor: Option<crate::TimelineCursor>,
    continuation: Option<QueuedSessionRun>,
    reason: SessionCoordinationReason,
}

impl SessionRunState {
    fn coordination(&self) -> SessionCoordinationState {
        let (phase, reason) = if let Some(pause) = &self.pause {
            (SessionCoordinationPhase::Paused, Some(pause.reason.clone()))
        } else if let Some(barrier) = &self.completion_barrier {
            (
                SessionCoordinationPhase::CompletionBarrier,
                Some(barrier.reason.clone()),
            )
        } else if self.active.is_some() {
            (SessionCoordinationPhase::Running, None)
        } else {
            (SessionCoordinationPhase::Ready, None)
        };
        SessionCoordinationState {
            phase,
            reason,
            queued_count: self.queued.len(),
            continuation_armed: self
                .pause
                .as_ref()
                .is_some_and(|pause| pause.continuation.is_some()),
        }
    }
}

#[derive(Debug)]
struct QueuedSessionRun {
    id: u64,
    input: AgentSessionRunInput,
    outcome_tx: RunOutcomeSender,
    limits: AgentRunLimits,
    runtime: Option<AgentRuntime>,
    runtime_factory: Option<AgentSessionRuntimeFactory>,
    inference_identity: Option<SessionRunInferenceIdentity>,
    cancellation: Option<CancellationToken>,
}

struct SessionRunStart {
    id: u64,
    token: CancellationToken,
    runtime: Option<AgentRuntime>,
    runtime_factory: Option<AgentSessionRuntimeFactory>,
    inference_identity: Option<SessionRunInferenceIdentity>,
    input: AgentSessionRunInput,
    outcome_txs: Vec<(u64, RunOutcomeSender)>,
    limits: AgentRunLimits,
    activity: RunActivity,
}

struct PreparedCompletionAdmission {
    source_cursor: crate::TimelineCursor,
    terminal_cursor: crate::TimelineCursor,
    preparation: super::SessionCompletionCoordinationPreparation,
}

struct AcceptedCompletionAdmission {
    pause_id: SessionPauseId,
    worker: crate::SessionCompletionWorkerFuture,
}

struct CapturedRuntimeRecord {
    record: super::SessionRecord,
    incarnation: super::SessionIncarnation,
}

impl<S> SessionCoordinator<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a durable session coordinator.
    pub fn new(store: S) -> Self {
        Self {
            store,
            control: SessionControl::new(),
            timeline: SessionTimelineBroadcaster::new(),
            runtime_factory: None,
            active_runs: Arc::new(Mutex::new(RunCoordination::default())),
            run_drain: Arc::new(RunDrain::default()),
            next_run_id: Arc::new(AtomicU64::new(0)),
            next_pause_id: Arc::new(AtomicU64::new(0)),
            event_sink: None,
            completion_handler: Arc::new(StdMutex::new(None)),
            shutdown_token: CancellationToken::new(),
            shutdown_completion: Arc::new(AsyncCompletion::default()),
            shutdown_task: Arc::new(StdMutex::new(None)),
            #[cfg(test)]
            list_registration_barrier: Arc::new(StdMutex::new(None)),
        }
    }

    /// Configure the runtime factory used for session run submissions.
    #[must_use]
    pub fn with_runtime_factory(mut self, factory: AgentSessionRuntimeFactory) -> Self {
        if let Some(events) = &factory.run_events {
            self.event_sink = Some(events.clone());
        }
        self.runtime_factory = Some(factory);
        self
    }

    /// Execute one host effect exclusively in an idle session, without a provider turn.
    /// A dropped caller cancels but cannot drop dispatched settlement or its audit.
    pub async fn execute_idle_host_tool(
        &self,
        id: &SessionId,
        expected_revision: crate::Revision,
        tool: crate::AgentTool,
        call: crate::ToolCall,
        audit: impl FnOnce(&crate::ToolResult) -> PendingTimelineEvent + Send + 'static,
        cancellation: CancellationToken,
    ) -> std::result::Result<crate::ToolResult, IdleHostEffectError> {
        let token = cancellation.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        let coordinator = self.clone();
        let id = id.clone();
        let driver = self.drive_owned_execution(&tokio::runtime::Handle::current(), async move {
            use crate::HostEffectExecutor as _;
            let operation = coordinator.control.session(id.clone()).try_operation()
                .ok_or(IdleHostEffectError::Busy)?;
            let record = coordinator.store.load(&id).await.map_err(session_store_error)?;
            if record.revision != expected_revision { return Err(IdleHostEffectError::Stale); }
            let snapshot = record.snapshot();
            if !matches!(snapshot.status, SessionStatus::Idle | SessionStatus::Finished) {
                return Err(IdleHostEffectError::Busy);
            }
            {
                let mut runs = coordinator.active_runs.lock().await;
                if runs.shutdown || runs.wiping { return Err(IdleHostEffectError::Canceled); }
                let state = runs.sessions.entry(id.clone()).or_default();
                if state.active.is_some() || state.pause.is_some() || state.completion_barrier.is_some()
                    || state.blocked || !state.queued.is_empty() {
                    return Err(IdleHostEffectError::Busy);
                }
                state.blocked = true;
            }
            let execution = async {
                let journal = crate::agent::host_effect::HostEffectJournal::new(coordinator.store.clone(), &record, token.clone())?;
                let context = snapshot.middleware_state.iter().filter(|(namespace, _)|
                    !crate::agent::host::reserved_context_namespace(namespace))
                    .map(|(key, value)| (key.clone(), value.clone())).collect();
                let effect = journal.execute(tool, call, snapshot.turn, context).await;
                let (commit, journal_events) = journal.synchronize(record.commit_receipt().map_err(session_store_error)?).await?;
                let mut updated = record.clone();
                updated.adopt_commit(commit);
                updated.append_journal_events(journal_events);
                let result = effect?.result?;
                let receipt = coordinator.store.append_timeline(&id, updated.execution_token().map_err(session_store_error)?, vec![audit(&result)]).await
                    .map_err(session_store_error)?;
                updated.adopt_commit(receipt.commit);
                updated.append_journal_events(receipt.events.iter().cloned().map(SessionJournalEvent::Timeline));
                coordinator.timeline.publish_all(receipt.events);
                operation.publish_host_summary(updated.summary());
                Ok::<_, IdleHostEffectError>(result)
            };
            tokio::pin!(execution);
            let outcome = tokio::select! {
                biased;
                _ = coordinator.shutdown_token.cancelled() => { token.cancel(); execution.await },
                result = &mut execution => result,
            };
            coordinator.unblock_session(&id).await;
            outcome
        }).await.ok_or(IdleHostEffectError::Canceled)?;
        driver.await.map_err(|_| {
            IdleHostEffectError::Effect(coordinator_error("host effect completion unavailable"))
        })?
    }

    /// Configure fire-and-forget semantic session event delivery.
    #[must_use]
    pub fn with_session_event_sink(mut self, sink: Arc<dyn crate::SessionEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Install the single host-owned terminal-run coordination handler.
    pub fn set_completion_admission_handler(
        &self,
        handler: Arc<dyn super::SessionCompletionCoordinationHandler>,
    ) {
        *self
            .completion_handler
            .lock()
            .expect("completion handler lock poisoned") = Some(handler);
    }

    /// Remove the host-owned terminal-run admission handler.
    pub fn clear_completion_admission_handler(&self) {
        *self
            .completion_handler
            .lock()
            .expect("completion handler lock poisoned") = None;
    }

    /// Create a durable session from complete structured input.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    #[tracing::instrument(
        name = "session.store",
        skip(self, request),
        fields(
            operation = "create",
            db_system = "session_store",
            session_id = %request.id.as_str()
        )
    )]
    pub async fn create(&self, request: SessionCreateRequest) -> Result<AgentSession> {
        tracing::debug!(
            metadata_key_count = request.metadata_values.len(),
            inference_configured = request.inference_settings.is_some(),
            agent_definition_configured = request.agent_definition.is_some(),
            "creating session record"
        );
        let operation = self.control.session(request.id.clone()).operation().await;
        let mut runtime = AgentSessionRuntime::new(&self.store);
        if let Some(sink) = &self.event_sink {
            runtime = runtime.with_session_event_sink(Arc::clone(sink));
        }
        let record = runtime.create(request).await?;
        self.active_runs.lock().await.sessions.remove(&record.id);
        self.timeline.publish_all(record.timeline_events().cloned());
        operation.publish_created(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Delete a durable session if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store cannot complete the delete operation.
    ///
    /// # Panics
    ///
    /// Panics if an internal session runtime cache lock is poisoned.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "delete", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn delete(&self, id: &SessionId) -> Result<()> {
        let operation = self.control.session(id.clone()).deletion().await;
        self.block_session(id, "session deleted").await;
        if let Err(error) = AgentSessionRuntime::new(&self.store)
            .delete_session(id)
            .await
        {
            self.unblock_session(id).await;
            return Err(error);
        }
        if let Some(factory) = &self.runtime_factory {
            factory.retire(id).await?;
        }
        operation.publish_deleted();
        drop(operation);
        Ok(())
    }

    /// Delete every durable session and clear host-owned runtime resources.
    ///
    /// # Errors
    ///
    /// Returns an error when the store wipe fails.
    ///
    /// # Panics
    ///
    /// Panics if an internal session runtime cache lock is poisoned.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "wipe", db_system = "session_store")
    )]
    pub async fn wipe_sessions(&self) -> Result<()> {
        let lifecycle = self.control.exclusive().await;
        self.begin_wipe().await;
        let wipe = self.store.wipe().await.map_err(session_store_error);
        if wipe.is_err() {
            self.end_wipe().await;
            return wipe;
        }
        lifecycle.publish_cleared();
        if let Some(factory) = &self.runtime_factory {
            factory.retire_all().await?;
        }
        self.end_wipe().await;
        Ok(())
    }

    /// Permanently stop new run admission and cancel process-local coordinated work.
    pub async fn shutdown(&self) {
        self.start_shutdown();
        self.shutdown_completion.wait().await;
    }

    pub(crate) async fn drive_owned_execution<T: Send + 'static>(
        &self,
        runtime: &tokio::runtime::Handle,
        work: impl std::future::Future<Output = T> + Send + 'static,
    ) -> Option<tokio::task::JoinHandle<T>> {
        let coordination = self.active_runs.lock().await;
        if coordination.shutdown || coordination.wiping {
            return None;
        }
        // Track blocked admission even when all join waiters are dropped.
        let activity = self.run_drain.begin();
        let driver = runtime.spawn(async move {
            let _activity = activity;
            work.await
        });
        drop(coordination);
        Some(driver)
    }

    fn start_shutdown(&self) {
        if self.shutdown_completion.is_complete() {
            return;
        }
        let mut task = self
            .shutdown_task
            .lock()
            .expect("session shutdown task lock poisoned");
        if task.is_some() {
            return;
        }
        let coordinator = self.clone();
        *task = Some(tokio::spawn(async move {
            coordinator.perform_shutdown().await;
            coordinator.shutdown_completion.finish();
        }));
    }

    async fn perform_shutdown(&self) {
        self.shutdown_token.cancel();
        self.clear_completion_admission_handler();
        let mut coordination = self.active_runs.lock().await;
        coordination.shutdown = true;
        for (session_id, state) in &mut coordination.sessions {
            cancel_session_state(session_id, state, "session coordinator shut down");
            state.blocked = true;
        }
        let invalidated = coordination.sessions.keys().cloned().collect::<Vec<_>>();
        drop(coordination);
        for session_id in invalidated {
            self.control
                .publish_coordination(&session_id, SessionCoordinationState::default());
        }
        self.run_drain.wait_until_idle().await;
        if let Some(factory) = &self.runtime_factory {
            if let Err(error) = factory.retire_all().await {
                tracing::warn!(%error, "host resource retirement failed");
            }
        }
    }

    /// Watch newly created or updated sessions.
    #[must_use]
    pub fn watch_sessions(&self) -> SessionListSubscription {
        self.control.subscribe()
    }

    /// Load an existing durable session by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "load", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn load(&self, id: &SessionId) -> Result<AgentSession> {
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime.load_session(id).await?;
        Ok(AgentSession::new(record))
    }

    /// Capture the source conversation through its latest effective completed turn.
    ///
    /// # Errors
    ///
    /// Returns a typed precondition, exhaustion, or storage failure.
    pub async fn capture_latest_completed(
        &self,
        id: &SessionId,
    ) -> std::result::Result<CompletedSessionCapture, SessionCaptureError> {
        self.capture_latest_completed_with_limits(id, SessionCaptureLimits::default())
            .await
    }

    /// Capture the latest completed turn under explicit projection bounds.
    ///
    /// # Errors
    ///
    /// Returns a typed precondition, exhaustion, or storage failure.
    pub async fn capture_latest_completed_with_limits(
        &self,
        id: &SessionId,
        limits: SessionCaptureLimits,
    ) -> std::result::Result<CompletedSessionCapture, SessionCaptureError> {
        let operation = self.control.session(id.clone()).operation().await;
        let record = self.store.load(id).await?;
        operation.register_loaded_summary(record.summary());
        super::capture::capture_record(&record, limits)
    }

    /// Capture mask-effective source context through its latest completed turn.
    ///
    /// # Errors
    ///
    /// Returns a typed precondition, exhaustion, or storage failure.
    pub async fn capture_latest_completed_effective(
        &self,
        id: &SessionId,
    ) -> std::result::Result<CompletedSessionCapture, SessionCaptureError> {
        self.capture_latest_completed_effective_with_limits(id, SessionCaptureLimits::default())
            .await
    }

    /// Capture mask-effective context under explicit projection bounds.
    ///
    /// # Errors
    ///
    /// Returns a typed precondition, exhaustion, or storage failure.
    pub async fn capture_latest_completed_effective_with_limits(
        &self,
        id: &SessionId,
        limits: SessionCaptureLimits,
    ) -> std::result::Result<CompletedSessionCapture, SessionCaptureError> {
        let operation = self.control.session(id.clone()).operation().await;
        let record = self.store.load(id).await?;
        operation.register_loaded_summary(record.summary());
        super::capture::capture_record_effective(&record, limits)
    }

    /// Capture effective completed context and an exact identity under the existing operation gate.
    /// Legacy records without a durable incarnation fail closed; this operation never writes.
    ///
    /// # Errors
    /// Returns a typed capture, resource, or storage failure without granting guest authority.
    pub async fn capture_context(
        &self,
        id: &SessionId,
        authority: &str,
        limits: SessionCaptureLimits,
    ) -> std::result::Result<super::SessionContextCapture, SessionCaptureError> {
        if authority.is_empty() || authority.len() > 128 {
            return Err(SessionCaptureError::FailedPrecondition(
                "invalid context authority identity".into(),
            ));
        }
        let operation = self.control.session(id.clone()).operation().await;
        let record = self.store.load(id).await?;
        operation.register_loaded_summary(record.summary());
        let completed = super::capture::capture_record_effective(&record, limits)?;
        let identity =
            super::ContextIdentity::capture(&record, completed.through_cursor, authority)
                .map_err(|error| SessionCaptureError::FailedPrecondition(error.to_string()))?;
        Ok(super::SessionContextCapture {
            identity,
            completed,
        })
    }

    /// Acquire an owned process-local pause without waiting for conflicting work.
    ///
    /// # Errors
    ///
    /// Returns a typed busy, boundary, capture, or storage error.
    pub async fn acquire_pause_reject_busy(
        &self,
        id: &SessionId,
        reason: SessionCoordinationReason,
    ) -> std::result::Result<SessionPauseLease, SessionPauseError> {
        {
            let coordination = self.active_runs.lock().await;
            if coordination.shutdown || coordination.wiping {
                return Err(SessionPauseError::Unavailable {
                    session_id: id.clone(),
                });
            }
            if let Some(state) = coordination.sessions.get(id) {
                if state.blocked {
                    return Err(SessionPauseError::Unavailable {
                        session_id: id.clone(),
                    });
                }
                if state.active.is_some() {
                    return Err(SessionPauseError::Active {
                        session_id: id.clone(),
                    });
                }
                if state.completion_barrier.is_some() {
                    return Err(SessionPauseError::Active {
                        session_id: id.clone(),
                    });
                }
                if state.pause.is_some() {
                    return Err(SessionPauseError::Paused {
                        session_id: id.clone(),
                    });
                }
            }
        }
        let operation = self
            .control
            .session(id.clone())
            .try_operation()
            .ok_or_else(|| SessionPauseError::LifecycleBusy {
                session_id: id.clone(),
            })?;
        let record = self
            .store
            .load(id)
            .await
            .map_err(SessionCaptureError::from)?;
        operation.register_loaded_summary(record.summary());
        if !record
            .effective_journal_events()
            .iter()
            .any(|event| matches!(event, super::SessionJournalEvent::TurnFinished { .. }))
        {
            return Err(SessionPauseError::CompletedBoundaryRequired {
                session_id: id.clone(),
            });
        }
        let capture =
            super::capture::capture_record_effective(&record, SessionCaptureLimits::default())?;
        let timeline_boundary = record
            .timeline_events()
            .last()
            .map(|event| event.cursor)
            .ok_or_else(|| SessionPauseError::CompletedBoundaryRequired {
                session_id: id.clone(),
            })?;

        let pause_id = SessionPauseId::new(self.next_pause_id.fetch_add(1, Ordering::Relaxed));
        let mut coordination = self.active_runs.lock().await;
        if coordination.shutdown || coordination.wiping {
            return Err(SessionPauseError::Unavailable {
                session_id: id.clone(),
            });
        }
        let state = coordination.sessions.entry(id.clone()).or_default();
        if state.blocked {
            return Err(SessionPauseError::Unavailable {
                session_id: id.clone(),
            });
        }
        if state.active.is_some() {
            return Err(SessionPauseError::Active {
                session_id: id.clone(),
            });
        }
        if state.completion_barrier.is_some() {
            return Err(SessionPauseError::Active {
                session_id: id.clone(),
            });
        }
        if state.pause.is_some() {
            return Err(SessionPauseError::Paused {
                session_id: id.clone(),
            });
        }
        state.pause = Some(OwnedSessionPause {
            id: pause_id,
            source_cursor: capture.through_cursor,
            timeline_boundary,
            mutation_cursor: None,
            continuation: None,
            reason,
        });
        let projected = state.coordination();
        drop(coordination);
        self.control.publish_coordination(id, projected);
        Ok(SessionPauseLease::new(
            id.clone(),
            pause_id,
            capture.through_cursor,
            timeline_boundary,
        ))
    }

    /// Capture effective completed context while the matching pause remains live.
    pub async fn capture_paused_effective(
        &self,
        lease: &SessionPauseLease,
    ) -> std::result::Result<CompletedSessionCapture, SessionPauseError> {
        self.capture_paused_effective_with_limits(lease, SessionCaptureLimits::default())
            .await
    }

    /// Capture effective completed context under explicit projection bounds.
    pub async fn capture_paused_effective_with_limits(
        &self,
        lease: &SessionPauseLease,
        limits: SessionCaptureLimits,
    ) -> std::result::Result<CompletedSessionCapture, SessionPauseError> {
        let operation = self
            .control
            .session(lease.session_id.clone())
            .operation()
            .await;
        self.validate_pause_owner(lease).await?;
        let record = self
            .store
            .load(&lease.session_id)
            .await
            .map_err(SessionCaptureError::from)?;
        operation.register_loaded_summary(record.summary());
        validate_pause_boundary(lease, &record)?;
        let capture = super::capture::capture_record_effective(&record, limits)?;
        if capture.through_cursor != lease.source_cursor {
            return Err(SessionPauseError::BoundaryChanged {
                session_id: lease.session_id.clone(),
                expected: lease.source_cursor,
                actual: Some(capture.through_cursor),
            });
        }
        Ok(capture)
    }

    /// Atomically apply a mask mutation at the matching pause's pinned boundary.
    pub async fn apply_paused_mask_mutation(
        &self,
        lease: &SessionPauseLease,
        mutation: SessionMaskMutation,
    ) -> std::result::Result<SessionMaskMutationOutcome, SessionPauseError> {
        self.apply_paused_session_mutation(
            lease,
            super::SessionMutation {
                masks: Some(mutation),
                ..Default::default()
            },
        )
        .await?
        .masks
        .ok_or_else(|| SessionPauseError::Store(super::SessionStoreError::Backend {
            message: "missing mask outcome".into(),
        }))
    }

    /// Commit metadata and masks together while retaining exact pause/continuation ownership.
    pub async fn apply_paused_session_mutation(
        &self,
        lease: &SessionPauseLease,
        mut mutation: super::SessionMutation,
    ) -> std::result::Result<super::SessionMutationOutcome, SessionPauseError> {
        let operation = self
            .control
            .session(lease.session_id.clone())
            .operation()
            .await;
        self.validate_pause_owner(lease).await?;
        let masks = mutation.masks.as_mut().ok_or_else(|| {
            SessionPauseError::Store(super::SessionStoreError::Backend {
                message: "paused history mutation requires masks".into(),
            })
        })?;
        masks.expected_timeline_boundary = Some(lease.timeline_boundary);
        let outcome = self
            .store
            .mutate(&lease.session_id, mutation)
            .await
            .map_err(|error| pause_store_error(lease, error))?;
        let record = &outcome.record;
        operation.publish_run_summary(record.summary());
        self.timeline
            .publish_projection_invalidation(lease.session_id.clone(), record.revision);
        let mut coordination = self.active_runs.lock().await;
        let pause = matching_pause_mut(&mut coordination, lease)?;
        pause.mutation_cursor = outcome.masks.as_ref().map(|masks| masks.cursor);
        Ok(outcome)
    }

    /// Arm a mask-aware continuation that will run before queued user work on release.
    pub async fn arm_paused_continuation(
        &self,
        lease: &SessionPauseLease,
        mutation_cursor: crate::TimelineCursor,
        limits: AgentRunLimits,
    ) -> std::result::Result<AgentSessionRunSubmission, SessionPauseError> {
        {
            let _operation = self
                .control
                .session(lease.session_id.clone())
                .operation()
                .await;
            self.validate_pause_owner(lease).await?;
            let record = self
                .store
                .load(&lease.session_id)
                .await
                .map_err(SessionPauseError::Store)?;
            validate_pause_boundary(lease, &record)?;
        }
        let (runtime, inference_identity) = self
            .resolve_runtime_with_identity(&lease.session_id)
            .await
            .map_err(SessionPauseError::Run)?;
        let _operation = self
            .control
            .session(lease.session_id.clone())
            .operation()
            .await;
        let record = self
            .store
            .load(&lease.session_id)
            .await
            .map_err(SessionPauseError::Store)?;
        validate_pause_boundary(lease, &record)?;
        let mut coordination = self.active_runs.lock().await;
        let pause = matching_pause_mut(&mut coordination, lease)?;
        if pause.mutation_cursor != Some(mutation_cursor) {
            return Err(SessionPauseError::ContinuationCursorMismatch {
                session_id: lease.session_id.clone(),
                expected: pause.mutation_cursor,
                actual: mutation_cursor,
            });
        }
        if pause.continuation.is_some() {
            return Err(SessionPauseError::ContinuationAlreadyArmed {
                session_id: lease.session_id.clone(),
            });
        }
        let id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        pause.continuation = Some(QueuedSessionRun {
            id,
            cancellation: None,
            input: AgentSessionRunInput::Continue,
            outcome_tx,
            limits,
            runtime: Some(runtime),
            runtime_factory: self.runtime_factory.clone(),
            inference_identity: Some(inference_identity),
        });
        let projected = coordination.sessions[&lease.session_id].coordination();
        drop(coordination);
        self.control
            .publish_coordination(&lease.session_id, projected);
        Ok(AgentSessionRunSubmission {
            session_id: lease.session_id.clone(),
            run_id: id,
            disposition: AgentSessionRunDisposition::Queued { position: 1 },
            handle: AgentSessionRunHandle::new(lease.session_id.clone(), id, outcome_rx),
        })
    }

    /// Release the matching pause, starting continuation or queued work when present.
    pub async fn release_pause(
        &self,
        lease: &SessionPauseLease,
    ) -> std::result::Result<(), SessionPauseError> {
        if lease.is_released() {
            return Ok(());
        }
        let operation = self
            .control
            .session(lease.session_id.clone())
            .operation()
            .await;
        let mut coordination = self.active_runs.lock().await;
        let lifecycle_blocked = coordination.shutdown || coordination.wiping;
        let Some(state) = coordination.sessions.get_mut(&lease.session_id) else {
            return Err(SessionPauseError::OwnershipLost {
                session_id: lease.session_id.clone(),
            });
        };
        if !state
            .pause
            .as_ref()
            .is_some_and(|pause| pause.id == lease.id)
        {
            return Err(SessionPauseError::OwnershipLost {
                session_id: lease.session_id.clone(),
            });
        }
        let pause = state.pause.take().expect("matching pause exists");
        let start = if state.blocked || lifecycle_blocked {
            if let Some(continuation) = pause.continuation {
                cancel_queued(
                    &lease.session_id,
                    VecDeque::from([continuation]),
                    "session lifecycle canceled paused work",
                );
            }
            cancel_queued(
                &lease.session_id,
                std::mem::take(&mut state.queued),
                "session lifecycle canceled paused work",
            );
            None
        } else if let Some(continuation) = pause.continuation {
            Some(start_queued(state, continuation, &self.run_drain))
        } else {
            pop_next_start(state, &self.run_drain, &lease.session_id)
        };
        if state.active.is_none() && state.queued.is_empty() && !state.blocked {
            coordination.sessions.remove(&lease.session_id);
        }
        let projected = coordination.sessions.get(&lease.session_id).map_or_else(
            SessionCoordinationState::default,
            SessionRunState::coordination,
        );
        drop(coordination);
        self.control
            .publish_coordination(&lease.session_id, projected);
        drop(operation);
        lease.mark_released();
        if let Some(start) = start {
            self.spawn_run(lease.session_id.clone(), start);
        }
        Ok(())
    }

    /// List durable sessions, newest activity first.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store cannot list sessions.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "list", db_system = "session_store")
    )]
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let summaries = AgentSessionRuntime::new(&self.store)
            .list_sessions()
            .await?;
        let mut registered = summaries
            .into_iter()
            .map(|summary| {
                let incarnation = self.control.register_listed_summary(summary.clone());
                (incarnation, summary)
            })
            .collect::<Vec<_>>();
        #[cfg(test)]
        self.wait_at_list_registration_barrier().await;

        let coordination = self.active_runs.lock().await;
        let mut summaries = Vec::with_capacity(registered.len());
        for (incarnation, mut summary) in registered.drain(..) {
            if let Some(state) = coordination.sessions.get(&summary.id) {
                summary.set_coordination(state.coordination());
            }
            if let Some(summary) = self.control.reconcile_listed_summary(incarnation, summary) {
                summaries.push(summary);
            }
        }
        Ok(summaries)
    }

    /// Read the current admission state for one session without a store traversal.
    pub async fn coordination_state(&self, id: &SessionId) -> SessionCoordinationState {
        self.active_runs
            .lock()
            .await
            .sessions
            .get(id)
            .map_or_else(SessionCoordinationState::default, |state| {
                state.coordination()
            })
    }

    #[cfg(test)]
    async fn wait_at_list_registration_barrier(&self) {
        let barrier = self
            .list_registration_barrier
            .lock()
            .expect("list registration barrier lock poisoned")
            .clone();
        if let Some((entered, release)) = barrier {
            entered.notify_one();
            release.notified().await;
        }
    }

    /// Persist an interruption for an existing durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "interrupt", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn interrupt(&self, id: &SessionId) -> Result<AgentSession> {
        let operation = self.control.session(id.clone()).operation().await;
        self.interrupt_active_run(id).await;
        let record = AgentSessionRuntime::new(&self.store)
            .with_session_run_context(self.run_context(id), operation.incarnation())
            .interrupt(id)
            .await?;
        Ok(AgentSession::new(record))
    }

    /// Read persisted timeline events for a durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    #[tracing::instrument(
        name = "session.store",
        skip(self, query),
        fields(operation = "timeline", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn timeline(
        &self,
        id: &SessionId,
        query: TimelineQuery,
    ) -> Result<Vec<AgentTimelineEvent>> {
        self.store
            .timeline(id, query)
            .await
            .map_err(session_store_error)
    }

    /// Read one explicit raw or mask-effective timeline projection page.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn timeline_projection(
        &self,
        id: &SessionId,
        mode: SessionTimelineMode,
        query: TimelineQuery,
    ) -> Result<SessionTimelinePage> {
        let operation = self.control.session(id.clone()).operation().await;
        let record = self.store.load(id).await.map_err(session_store_error)?;
        operation.register_loaded_summary(record.summary());
        Ok(super::projection::project_timeline(&record, mode, query))
    }

    /// Append timeline events to an existing durable session and publish the assigned events.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store rejects the append.
    #[tracing::instrument(
        name = "session.store",
        skip(self, events),
        fields(operation = "append_timeline", db_system = "session_store", session_id = %id.as_str(), event_count = events.len())
    )]
    pub async fn append_timeline(
        &self,
        id: &SessionId,
        events: Vec<PendingTimelineEvent>,
    ) -> Result<Vec<AgentTimelineEvent>> {
        let operation = self.control.session(id.clone()).operation().await;
        let mut record = self.load(id).await?;
        operation.register_loaded_summary(record.record.summary());
        let receipt = self
            .store
            .append_timeline(
                id,
                record
                    .record
                    .execution_token()
                    .map_err(session_store_error)?,
                events,
            )
            .await
            .map_err(session_store_error)?;
        record.record.adopt_commit(receipt.commit);
        record.record.append_journal_events(
            receipt
                .events
                .iter()
                .cloned()
                .map(SessionJournalEvent::Timeline),
        );
        self.timeline.publish_all(receipt.events.clone());
        operation.publish_run_summary(record.record.summary());
        Ok(receipt.events)
    }

    /// Reconstruct text content for one persisted timeline item.
    ///
    /// # Errors
    ///
    /// Returns an error when the item does not exist, is not text-like, or the store fails.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "timeline_item_text", db_system = "session_store", session_id = %id.as_str(), item_id = %item_id.as_str())
    )]
    pub async fn timeline_item_text(
        &self,
        id: &SessionId,
        item_id: &TimelineItemId,
    ) -> Result<TimelineItemText> {
        self.store
            .timeline_item_text(id, item_id)
            .await
            .map_err(session_store_error)
    }

    /// Watch persisted timeline updates for one durable session.
    #[must_use]
    pub fn watch_timeline(&self, id: SessionId) -> SessionTimelineSubscription {
        self.timeline.subscribe(id)
    }

    /// Watch raw appends and authoritative invalidations for an effective projection.
    #[must_use]
    pub fn watch_timeline_projection(
        &self,
        id: SessionId,
    ) -> SessionTimelineProjectionSubscription {
        self.timeline.subscribe_projection(id)
    }

    /// Set a host-defined metadata value and publish an updated session summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    #[tracing::instrument(
        name = "session.store",
        skip(self, key, value),
        fields(operation = "set_metadata_value", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn set_metadata_value(
        &self,
        id: &SessionId,
        key: impl Into<String>,
        value: Value,
    ) -> Result<AgentSession> {
        let operation = self.control.session(id.clone()).operation().await;
        let key = key.into();
        tracing::debug!(metadata_key = %key, "setting session metadata value");
        let record = AgentSessionRuntime::new(&self.store)
            .set_metadata_value(id, key, value)
            .await?;
        operation.publish_host_summary(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Apply host-validated context mask preconditions under the existing session operation gate.
    pub async fn mutate_context_masks(
        &self,
        id: &SessionId,
        mutation: SessionMaskMutation,
    ) -> super::SessionStoreResult<SessionMaskMutationOutcome> {
        self.mutate_session(
            id,
            super::SessionMutation {
                masks: Some(mutation),
                ..Default::default()
            },
        )
        .await?
        .masks
        .ok_or_else(|| super::SessionStoreError::Backend {
            message: "missing mask outcome".into(),
        })
    }

    /// Commit opaque session/item metadata and dependent mask edits under the session gate.
    pub async fn mutate_session(
        &self,
        id: &SessionId,
        mutation: super::SessionMutation,
    ) -> super::SessionStoreResult<super::SessionMutationOutcome> {
        let operation = self.control.session(id.clone()).operation().await;
        let outcome = self.store.mutate(id, mutation).await?;
        operation.publish_host_summary(outcome.record.summary());
        self.timeline
            .publish_projection_invalidation(id.clone(), outcome.record.revision);
        Ok(outcome)
    }

    /// Read bounded opaque metadata under the existing lifecycle/mutation operation gate.
    pub async fn metadata(
        &self,
        id: &SessionId,
        target: super::MetadataTarget,
    ) -> super::SessionStoreResult<std::collections::BTreeMap<String, Value>> {
        let _operation = self.control.session(id.clone()).operation().await;
        self.store.metadata(id, target).await
    }

    /// Atomically set metadata according to `condition` and publish only changed records.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store cannot apply the write.
    pub async fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: impl Into<String>,
        value: Value,
        condition: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome> {
        let operation = self.control.session(id.clone()).operation().await;
        let outcome = AgentSessionRuntime::new(&self.store)
            .set_metadata_value_conditionally(id, key.into(), value, condition)
            .await?;
        if outcome.changed {
            operation.publish_host_summary(outcome.record.summary());
        }
        Ok(outcome)
    }

    /// Remove a host-defined metadata value and publish an updated session summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    #[tracing::instrument(
        name = "session.store",
        skip(self, key),
        fields(operation = "remove_metadata_value", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn remove_metadata_value(
        &self,
        id: &SessionId,
        key: impl Into<String>,
    ) -> Result<AgentSession> {
        let operation = self.control.session(id.clone()).operation().await;
        let key = key.into();
        tracing::debug!(metadata_key = %key, "removing session metadata value");
        let record = AgentSessionRuntime::new(&self.store)
            .remove_metadata_value(id, key)
            .await?;
        operation.publish_host_summary(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Set inference settings and publish an updated session summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn set_inference_settings(
        &self,
        id: &SessionId,
        inference_settings: Option<InferenceSettings>,
    ) -> Result<AgentSession> {
        let operation = self.control.session(id.clone()).operation().await;
        let record = AgentSessionRuntime::new(&self.store)
            .set_inference_settings(id, inference_settings)
            .await?;
        operation.publish_host_summary(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Set the agent-definition bundle and publish an updated session summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn set_agent_definition(
        &self,
        id: &SessionId,
        agent_definition: Option<serde_json::Value>,
    ) -> Result<AgentSession> {
        let operation = self.control.session(id.clone()).operation().await;
        let record = AgentSessionRuntime::new(&self.store)
            .set_agent_definition(id, agent_definition)
            .await?;
        operation.publish_host_summary(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Transform opaque host configuration under the session mutation boundary.
    pub async fn update_host_configuration<F>(
        &self,
        id: &SessionId,
        update: F,
    ) -> Result<AgentSession>
    where
        F: FnOnce(&SessionRecord) -> Result<Option<Value>> + Send,
    {
        let operation = self.control.session(id.clone()).operation().await;
        let current = self.load(id).await?.record;
        let configuration = update(&current)?;
        let record = AgentSessionRuntime::new(&self.store)
            .set_agent_definition(id, configuration)
            .await?;
        operation.publish_host_summary(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Submit a prompt run.
    ///
    /// # Errors
    ///
    /// Returns an error when runtime resolution or run admission fails.
    pub async fn submit_prompt(
        &self,
        session_id: SessionId,
        prompt: impl Into<String>,
        policy: RunConflictPolicy,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_prompt_with_limits(session_id, prompt, policy, AgentRunLimits::default())
            .await
    }

    /// Submit a prompt run with limits captured for this submission.
    ///
    /// # Errors
    ///
    /// Returns an error when runtime resolution or run admission fails.
    pub async fn submit_prompt_with_limits(
        &self,
        session_id: SessionId,
        prompt: impl Into<String>,
        policy: RunConflictPolicy,
        limits: AgentRunLimits,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_with_limits(
            session_id,
            AgentSessionRunInput::Prompt(prompt.into()),
            policy,
            limits,
        )
        .await
    }

    /// Start the already durable current prompt with normal continue semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when another run is active or runtime resolution fails.
    pub async fn start_durable_prompt(
        &self,
        session_id: SessionId,
        limits: AgentRunLimits,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_with_limits(
            session_id,
            AgentSessionRunInput::Continue,
            RunConflictPolicy::RejectIfActive,
            limits,
        )
        .await
    }

    /// Roll back a session to the end of a visible provider turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or persistence fails.
    pub async fn rollback_to_turn(&self, session_id: SessionId, turn: u64) -> Result<AgentSession> {
        let operation = self.control.session(session_id.clone()).operation().await;
        self.interrupt_active_run(&session_id).await;
        let output = AgentSessionRuntime::new(&self.store)
            .with_session_run_context(self.run_context(&session_id), operation.incarnation())
            .rollback_to_turn(&session_id, turn)
            .await?;
        self.timeline
            .publish_truncation(session_id.clone(), output.after_cursor);
        self.timeline
            .publish_projection_invalidation(session_id.clone(), output.record.revision);
        Ok(AgentSession::new(output.record))
    }

    /// Roll back a session to a visible timeline cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or persistence fails.
    pub async fn rollback_to_cursor(
        &self,
        session_id: SessionId,
        cursor: crate::timeline::TimelineCursor,
    ) -> Result<AgentSession> {
        let operation = self.control.session(session_id.clone()).operation().await;
        self.interrupt_active_run(&session_id).await;
        let output = AgentSessionRuntime::new(&self.store)
            .with_session_run_context(self.run_context(&session_id), operation.incarnation())
            .rollback_to_cursor(&session_id, cursor)
            .await?;
        self.timeline
            .publish_truncation(session_id.clone(), output.after_cursor);
        self.timeline
            .publish_projection_invalidation(session_id.clone(), output.record.revision);
        Ok(AgentSession::new(output.record))
    }

    /// Roll back a session to immediately before a visible timeline cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or persistence fails.
    pub async fn rollback_before_cursor(
        &self,
        session_id: SessionId,
        cursor: crate::timeline::TimelineCursor,
    ) -> Result<AgentSession> {
        let operation = self.control.session(session_id.clone()).operation().await;
        self.interrupt_active_run(&session_id).await;
        let output = AgentSessionRuntime::new(&self.store)
            .with_session_run_context(self.run_context(&session_id), operation.incarnation())
            .rollback_before_cursor(&session_id, cursor)
            .await?;
        self.timeline
            .publish_truncation(session_id.clone(), output.after_cursor);
        self.timeline
            .publish_projection_invalidation(session_id.clone(), output.record.revision);
        Ok(AgentSession::new(output.record))
    }

    /// Roll back to the end of a turn and continue from that durable checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when rollback, runtime resolution, or run admission fails.
    pub async fn continue_from_turn(
        &self,
        session_id: SessionId,
        turn: u64,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.preflight_authentication(&session_id).await?;
        self.rollback_to_turn(session_id.clone(), turn).await?;
        self.submit(
            session_id,
            AgentSessionRunInput::Continue,
            RunConflictPolicy::ReplaceActive,
        )
        .await
    }

    /// Roll back to a timeline cursor and continue from that durable checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when rollback, runtime resolution, or run admission fails.
    pub async fn continue_from_cursor(
        &self,
        session_id: SessionId,
        cursor: crate::timeline::TimelineCursor,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.preflight_authentication(&session_id).await?;
        self.rollback_to_cursor(session_id.clone(), cursor).await?;
        self.submit(
            session_id,
            AgentSessionRunInput::Continue,
            RunConflictPolicy::ReplaceActive,
        )
        .await
    }

    /// Submit a session run or resume input.
    ///
    /// # Errors
    ///
    /// Returns an error when conflict policy rejects the run or runtime resolution fails.
    pub async fn submit(
        &self,
        session_id: SessionId,
        input: AgentSessionRunInput,
        policy: RunConflictPolicy,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_with_limits(session_id, input, policy, AgentRunLimits::default())
            .await
    }

    /// Submit a session run with limits captured through queueing and execution.
    ///
    /// # Errors
    ///
    /// Returns an error when conflict policy rejects the run or runtime resolution fails.
    #[allow(clippy::significant_drop_tightening, clippy::too_many_lines)]
    pub async fn submit_with_limits(
        &self,
        session_id: SessionId,
        input: AgentSessionRunInput,
        policy: RunConflictPolicy,
        limits: AgentRunLimits,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_configured(session_id, input, policy, limits, None)
            .await
    }

    /// Start an isolated helper using a runtime pinned by the trusted consumer.
    ///
    /// # Errors
    /// Rejects an active helper, unavailable session or invalid durable identity.
    pub async fn start_pinned_helper(
        &self,
        session_id: SessionId,
        runtime: AgentRuntime,
        validation: Option<crate::agent::completion::CompletionValidation>,
        limits: AgentRunLimits,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        let mut runtime = runtime;
        if let Some(validation) = &validation {
            runtime.max_provider_turns = Some(
                runtime
                    .max_provider_turns
                    .map_or(validation.max_turns, |limit| {
                        limit.min(validation.max_turns)
                    }),
            );
        }
        runtime.completion_validation = validation;
        self.submit_configured(
            session_id,
            AgentSessionRunInput::Continue,
            RunConflictPolicy::RejectIfActive,
            limits,
            Some(runtime),
        )
        .await
    }

    /// Execute a host-selected tool in a consumer-owned durable session.
    ///
    /// # Errors
    /// Rejects overlapping ownership or unavailable durable identity.
    pub async fn start_pinned_host_tool(
        &self,
        session_id: SessionId,
        mut runtime: AgentRuntime,
        call: crate::ToolCall,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        runtime.completion_validation = None;
        self.submit_configured(
            session_id,
            AgentSessionRunInput::HostTool(call),
            RunConflictPolicy::RejectIfActive,
            AgentRunLimits::default(),
            Some(runtime),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_configured(
        &self,
        session_id: SessionId,
        input: AgentSessionRunInput,
        policy: RunConflictPolicy,
        limits: AgentRunLimits,
        prepared: Option<AgentRuntime>,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_configured_cancellable(session_id, input, policy, limits, prepared, None)
            .await
    }

    /// Submit an independently cancellable prompt without merging other submissions.
    /// The supplied token fences admission and execution; dropping a waiter does not cancel it.
    ///
    /// # Errors
    /// Returns the ordinary admission or runtime-resolution error.
    pub async fn submit_prompt_cancellable(
        &self,
        session_id: SessionId,
        prompt: String,
        policy: RunConflictPolicy,
        cancellation: CancellationToken,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit_configured_cancellable(
            session_id,
            AgentSessionRunInput::Prompt(prompt),
            policy,
            AgentRunLimits::default(),
            None,
            Some(cancellation.child_token()),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_configured_cancellable(
        &self,
        session_id: SessionId,
        input: AgentSessionRunInput,
        policy: RunConflictPolicy,
        limits: AgentRunLimits,
        prepared: Option<AgentRuntime>,
        cancellation: Option<CancellationToken>,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        let id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let origin_operation = if cancellation.is_some() {
            Some(self.control.session(session_id.clone()).operation().await)
        } else {
            None
        };
        let mut handle = AgentSessionRunHandle::new(session_id.clone(), id, outcome_rx);
        if let (Some(operation), Some(token)) = (&origin_operation, &cancellation) {
            let coordinator = self.clone();
            let session = session_id.clone();
            let incarnation = operation.incarnation();
            let token = token.clone();
            handle =
                handle.with_cancellation(super::run::AgentSessionRunCancellation::new(move || {
                    let coordinator = coordinator.clone();
                    let session = session.clone();
                    let token = token.clone();
                    Box::pin(async move {
                        token.cancel();
                        let operation = coordinator
                            .control
                            .session(session.clone())
                            .operation()
                            .await;
                        if operation.incarnation() != incarnation {
                            return;
                        }
                        let mut coordination = coordinator.active_runs.lock().await;
                        let Some(state) = coordination.sessions.get_mut(&session) else {
                            return;
                        };
                        if let Some(position) =
                            state.queued.iter().position(|queued| queued.id == id)
                        {
                            let queued = state.queued.remove(position).expect("matched queued run");
                            cancel_queued(&session, VecDeque::from([queued]), "origin canceled");
                        }
                        let projected = state.coordination();
                        drop(coordination);
                        coordinator
                            .control
                            .publish_coordination(&session, projected);
                    })
                }));
        }
        let mut start = None;
        let disposition;
        let projected;

        {
            let mut coordination = self.active_runs.lock().await;
            if coordination.shutdown || coordination.wiping {
                return Err(AgentSessionRunError::Unavailable { session_id });
            }
            let current = coordination.sessions.entry(session_id.clone()).or_default();
            if current.blocked {
                return Err(AgentSessionRunError::Unavailable { session_id });
            }
            if current.pause.is_some() || current.completion_barrier.is_some() {
                match policy {
                    RunConflictPolicy::QueueIfActive => {
                        current.queued.push_back(QueuedSessionRun {
                            id,
                            cancellation: cancellation.clone(),
                            input,
                            outcome_tx,
                            limits,
                            runtime: None,
                            runtime_factory: self.runtime_factory.clone(),
                            inference_identity: None,
                        });
                        disposition = AgentSessionRunDisposition::Queued {
                            position: current.queued.len(),
                        };
                    }
                    RunConflictPolicy::RejectIfActive | RunConflictPolicy::ReplaceActive => {
                        if current.pause.is_some() {
                            return Err(AgentSessionRunError::Paused { session_id });
                        }
                        return Err(AgentSessionRunError::AlreadyRunning { session_id });
                    }
                }
            } else if let Some(active_run) = current.active.as_ref() {
                match policy {
                    RunConflictPolicy::RejectIfActive => {
                        return Err(AgentSessionRunError::AlreadyRunning { session_id });
                    }
                    RunConflictPolicy::QueueIfActive => {
                        current.queued.push_back(QueuedSessionRun {
                            id,
                            cancellation: cancellation.clone(),
                            input,
                            outcome_tx,
                            limits,
                            runtime: None,
                            runtime_factory: self.runtime_factory.clone(),
                            inference_identity: None,
                        });
                        disposition = AgentSessionRunDisposition::Queued {
                            position: current.queued.len(),
                        };
                    }
                    RunConflictPolicy::ReplaceActive => {
                        let token = cancellation.clone().unwrap_or_default();
                        active_run.token.cancel();
                        cancel_queued(
                            &session_id,
                            std::mem::take(&mut current.queued),
                            "replaced by a new run",
                        );
                        current.active = Some(ActiveSessionRun {
                            id,
                            token: token.clone(),
                            inference_identity: None,
                        });
                        start = Some(SessionRunStart {
                            id,
                            token,
                            runtime: None,
                            runtime_factory: self.runtime_factory.clone(),
                            inference_identity: None,
                            input,
                            outcome_txs: vec![(id, outcome_tx)],
                            limits,
                            activity: self.run_drain.begin(),
                        });
                        disposition = AgentSessionRunDisposition::ReplacedActive;
                    }
                }
            } else {
                let token = cancellation.clone().unwrap_or_default();
                current.active = Some(ActiveSessionRun {
                    id,
                    token: token.clone(),
                    inference_identity: None,
                });
                start = Some(SessionRunStart {
                    id,
                    token,
                    runtime: None,
                    runtime_factory: self.runtime_factory.clone(),
                    inference_identity: None,
                    input,
                    outcome_txs: vec![(id, outcome_tx)],
                    limits,
                    activity: self.run_drain.begin(),
                });
                disposition = AgentSessionRunDisposition::Started;
            }
            projected = current.coordination();
        }
        self.control.publish_coordination(&session_id, projected);
        drop(origin_operation);

        if let Some(mut start) = start {
            let resolution = if let Some(runtime) = prepared {
                async {
                    let captured = if runtime.provider.transient_input().is_some() {
                        self.capture_runtime_record(&session_id).await?
                    } else {
                        self.capture_runtime_record_for_cycle(&session_id).await?
                    };
                    let settings =
                        captured.record.inference_settings.as_ref().ok_or_else(|| {
                            super::AgentSessionRuntimeError::FailedPrecondition(
                                "pinned helper requires explicit inference settings".into(),
                            )
                        })?;
                    let identity = SessionRunInferenceIdentity::from_settings(
                        settings,
                        captured
                            .record
                            .execution_token()
                            .map_err(session_store_error)?,
                    );
                    let _operation = self
                        .validate_incarnation(&session_id, captured.incarnation)
                        .await?;
                    Ok((runtime, identity))
                }
                .await
            } else {
                self.resolve_runtime_with_identity(&session_id).await
            };
            match resolution {
                Ok((runtime, identity)) => {
                    start.runtime = Some(runtime);
                    start.inference_identity = Some(identity.clone());
                    self.set_active_inference_identity(&session_id, start.id, identity)
                        .await;
                }
                Err(error) => {
                    let next = self.fail_active_start(&session_id, start.id).await;
                    if let Some(next) = next {
                        self.spawn_run(session_id.clone(), next);
                    }
                    return Err(error);
                }
            }
            self.spawn_run(session_id.clone(), start);
        }

        Ok(AgentSessionRunSubmission {
            session_id,
            run_id: id,
            disposition,
            handle,
        })
    }

    /// Return active-run state for a session, when present.
    #[must_use]
    pub async fn active_state(&self, session_id: &SessionId) -> Option<AgentSessionActiveRunState> {
        self.active_runs
            .lock()
            .await
            .sessions
            .get(session_id)
            .and_then(|state| state.active.as_ref().map(|active| (state, active)))
            .map(|(state, active)| AgentSessionActiveRunState {
                run_id: active.id,
                queued_count: state.queued.len(),
            })
    }

    async fn interrupt_active_run(&self, id: &SessionId) {
        let mut coordination = self.active_runs.lock().await;
        let state = coordination.sessions.remove(id);
        let coordination_changed = state
            .as_ref()
            .is_some_and(|state| state.coordination() != SessionCoordinationState::default());
        if let Some(mut state) = state {
            if let Some(pause) = state.pause.take()
                && let Some(continuation) = pause.continuation
            {
                cancel_queued(id, VecDeque::from([continuation]), "session interrupted");
            }
            cancel_queued(id, state.queued, "session interrupted");
            if let Some(active) = state.active {
                active.token.cancel();
                tracing::info!(session_id = %id.as_str(), run_id = active.id, "active session run cancellation requested");
            } else {
                tracing::debug!(session_id = %id.as_str(), "no active session run to cancel");
            }
        } else {
            tracing::debug!(session_id = %id.as_str(), "no active session run to cancel");
        }
        drop(coordination);
        if coordination_changed {
            self.control
                .publish_coordination(id, SessionCoordinationState::default());
        }
    }

    fn spawn_run(&self, session_id: SessionId, start: SessionRunStart) {
        let mut coordinator = self.clone();
        coordinator.runtime_factory = start.runtime_factory.clone();
        if let Some(factory) = &start.runtime_factory {
            if let Some(events) = &factory.run_events {
                coordinator.event_sink = Some(events.clone());
            }
        }
        let run_id = start.id;
        let run_kind = start.input.kind();
        let span = tracing::info_span!(
            "session.run",
            session_id = %session_id.as_str(),
            run_id,
            run_kind,
            terminal_state = tracing::field::Empty,
        );
        tracing::info!(session_id = %session_id.as_str(), run_id, run_kind, "session run started");
        tokio::spawn(
            async move {
                let _activity = start.activity;
                let outcome = drain_run(
                    coordinator.clone(),
                    start.runtime,
                    session_id.clone(),
                    start.input,
                    start.limits,
                    start.id,
                    start.token,
                )
                .await;
                if let Err(error) = &outcome {
                    tracing::error!(session_id = %session_id.as_str(), run_id, %error, "session run failed");
                }
                let terminal_state = terminal_state(&outcome);
                tracing::Span::current().record("terminal_state", terminal_state);
                tracing::info!(session_id = %session_id.as_str(), run_id, terminal_state, "session run completed");
                let outcomes = start.outcome_txs;
                let next = coordinator.complete_active_run(&session_id, run_id, &outcome).await;
                send_run_outcome(outcomes, outcome);
                if let Some(next) = next {
                    coordinator.spawn_run(session_id, next);
                }
            }
            .instrument(span),
        );
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn finish_active_run(
        &self,
        session_id: &SessionId,
        run_id: u64,
    ) -> Option<SessionRunStart> {
        let mut coordination = self.active_runs.lock().await;
        let current = coordination.sessions.get_mut(session_id)?;
        if current.active.as_ref()?.id != run_id {
            return None;
        }
        current.active = None;
        if current.pause.is_some() || current.blocked {
            let projected = current.coordination();
            drop(coordination);
            self.control.publish_coordination(session_id, projected);
            return None;
        }
        let next = pop_next_start(current, &self.run_drain, session_id);
        if next.is_none() {
            coordination.sessions.remove(session_id);
        }
        let projected = coordination.sessions.get(session_id).map_or_else(
            SessionCoordinationState::default,
            SessionRunState::coordination,
        );
        drop(coordination);
        self.control.publish_coordination(session_id, projected);
        next
    }

    async fn complete_active_run(
        &self,
        session_id: &SessionId,
        run_id: u64,
        outcome: &RunResult,
    ) -> Option<SessionRunStart> {
        if !matches!(outcome, Ok(outcome) if outcome.status == SessionStatus::Finished) {
            return self.finish_active_run(session_id, run_id).await;
        }
        let inference_identity = {
            let mut coordination = self.active_runs.lock().await;
            let Some(state) = coordination.sessions.get_mut(session_id) else {
                return None;
            };
            if state.active.as_ref().map(|active| active.id) != Some(run_id) {
                return None;
            }
            let inference_identity = state
                .active
                .as_ref()
                .and_then(|active| active.inference_identity.clone());
            state.active = None;
            state.completion_barrier = Some(CompletionBarrier {
                run_id,
                reason: SessionCoordinationReason::post_run_automation(),
            });
            let projected = state.coordination();
            drop(coordination);
            self.control.publish_coordination(session_id, projected);
            inference_identity
        };

        let Some(inference_identity) = inference_identity.as_ref() else {
            return self
                .finish_completion_barrier(session_id, run_id, None)
                .await;
        };
        let admission = self
            .run_completion_admission(session_id, run_id, inference_identity)
            .await;
        self.finish_completion_barrier(session_id, run_id, admission)
            .await
    }

    async fn run_completion_admission(
        &self,
        session_id: &SessionId,
        run_id: u64,
        inference_identity: &SessionRunInferenceIdentity,
    ) -> Option<AcceptedCompletionAdmission> {
        const COMPLETION_ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);

        let handler = self
            .runtime_factory
            .as_ref()
            .and_then(|factory| factory.run_completion.clone())
            .or_else(|| {
                self.completion_handler
                    .lock()
                    .expect("completion handler lock poisoned")
                    .clone()
            })?;
        let store = self.store.clone();
        let prepared_session_id = session_id.clone();
        let inference_identity = inference_identity.clone();
        let mut task = tokio::spawn(async move {
            let record = store
                .load(&prepared_session_id)
                .await
                .map_err(|error| error.to_string())?;
            let effective = record.effective_journal_events();
            let completed_boundary = effective
                .iter()
                .rposition(|event| matches!(event, super::SessionJournalEvent::TurnFinished { .. }))
                .ok_or_else(|| "completed run has no completed turn boundary".to_owned())?;
            let completed_turn = match effective[completed_boundary] {
                super::SessionJournalEvent::TurnFinished { turn } => *turn,
                _ => unreachable!("completed boundary selected by variant"),
            };
            let source_cursor = effective[..=completed_boundary]
                .iter()
                .rev()
                .find_map(|event| match event {
                    super::SessionJournalEvent::Timeline(event) => Some(event.cursor),
                    _ => None,
                })
                .ok_or_else(|| "completed run has no source cursor".to_owned())?;
            let terminal_cursor = record
                .timeline_events()
                .last()
                .map(|event| event.cursor)
                .ok_or_else(|| "completed run has no terminal cursor".to_owned())?;
            let context_tokens = effective[..=completed_boundary]
                .iter()
                .rev()
                .find_map(|event| match event {
                    super::SessionJournalEvent::ProviderTurnMetadataRecorded { turn, metadata }
                        if *turn == completed_turn =>
                    {
                        metadata.usage.map(crate::TokenUsage::context_tokens)
                    }
                    _ => None,
                });
            let snapshot = crate::SessionRunCompletionSnapshot {
                session_id: prepared_session_id.clone(),
                incarnation: record.metadata.incarnation.clone(),
                revision: record.revision,
                run_id: format!("session-run:{}:r{run_id}", prepared_session_id.as_str()),
                terminal_cursor,
                context_tokens,
                provider_key: inference_identity.provider_key,
                model_id: inference_identity.model_id,
            };
            let preparation = handler.prepare(snapshot).await?;
            Ok::<_, String>(PreparedCompletionAdmission {
                source_cursor,
                terminal_cursor,
                preparation,
            })
        });
        let prepared = tokio::select! {
            biased;
            () = self.shutdown_token.cancelled() => {
                task.abort();
                return None;
            }
            result = tokio::time::timeout(COMPLETION_ADMISSION_TIMEOUT, &mut task) => match result {
                Ok(Ok(Ok(prepared))) => prepared,
                Ok(Ok(Err(message))) => {
                    tracing::warn!(%message, "session completion preparation failed open");
                    return None;
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "session completion preparation task failed open");
                    return None;
                }
                Err(_) => {
                    task.abort();
                    tracing::warn!(timeout_ms = COMPLETION_ADMISSION_TIMEOUT.as_millis(), "session completion preparation timed out");
                    return None;
                }
            }
        };
        let super::SessionCompletionCoordinationPreparation::Claim { reason, finalizer } =
            prepared.preparation
        else {
            return None;
        };
        let lease = match self
            .claim_completion_pause(
                session_id,
                run_id,
                prepared.source_cursor,
                prepared.terminal_cursor,
                reason,
            )
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(%error, "session completion pause claim failed open");
                return None;
            }
        };
        let pause_id = lease.id;
        let finalization = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            finalizer.finalize(lease)
        })) {
            Ok(finalization) => finalization,
            Err(_) => {
                tracing::warn!(
                    "session completion finalizer panicked during worker construction; failing open"
                );
                return None;
            }
        };
        match tokio::spawn(finalization).await {
            Ok(Ok(worker)) => Some(AcceptedCompletionAdmission { pause_id, worker }),
            Ok(Err(message)) => {
                tracing::warn!(%message, "session completion finalization failed open");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "session completion finalization panicked; failing open");
                None
            }
        }
    }

    async fn finish_completion_barrier(
        &self,
        session_id: &SessionId,
        run_id: u64,
        admission: Option<AcceptedCompletionAdmission>,
    ) -> Option<SessionRunStart> {
        let mut coordination = self.active_runs.lock().await;
        let lifecycle_blocked = coordination.shutdown || coordination.wiping;
        let Some(state) = coordination.sessions.get_mut(session_id) else {
            drop(coordination);
            if !lifecycle_blocked
                && let Some(AcceptedCompletionAdmission { worker, .. }) = admission
            {
                // A successful durable admission still receives terminal executor cleanup if an
                // interrupt invalidated the process-local pause before acknowledgement.
                drop(tokio::spawn(worker));
            }
            return None;
        };
        if state
            .completion_barrier
            .as_ref()
            .map(|barrier| barrier.run_id)
            != Some(run_id)
        {
            drop(coordination);
            if !lifecycle_blocked
                && let Some(AcceptedCompletionAdmission { worker, .. }) = admission
            {
                drop(tokio::spawn(worker));
            }
            return None;
        }
        state.completion_barrier = None;

        if let Some(AcceptedCompletionAdmission { pause_id, worker }) = admission
            && !lifecycle_blocked
            && !state.blocked
            && state
                .pause
                .as_ref()
                .is_some_and(|pause| pause.id == pause_id)
        {
            let projected = state.coordination();
            drop(coordination);
            self.control.publish_coordination(session_id, projected);
            drop(tokio::spawn(worker));
            return None;
        }

        if let Some(pause) = state.pause.take() {
            if let Some(continuation) = pause.continuation {
                cancel_queued(
                    session_id,
                    VecDeque::from([continuation]),
                    "completion admission failed open",
                );
            }
        }
        let next = if lifecycle_blocked || state.blocked {
            cancel_queued(
                session_id,
                std::mem::take(&mut state.queued),
                "session lifecycle canceled queued work",
            );
            None
        } else {
            pop_next_start(state, &self.run_drain, session_id)
        };
        if state.active.is_none() && state.queued.is_empty() && !state.blocked {
            coordination.sessions.remove(session_id);
        }
        let projected = coordination.sessions.get(session_id).map_or_else(
            SessionCoordinationState::default,
            SessionRunState::coordination,
        );
        drop(coordination);
        self.control.publish_coordination(session_id, projected);
        next
    }

    async fn claim_completion_pause(
        &self,
        session_id: &SessionId,
        run_id: u64,
        source_cursor: crate::TimelineCursor,
        terminal_cursor: crate::TimelineCursor,
        reason: SessionCoordinationReason,
    ) -> std::result::Result<SessionPauseLease, SessionPauseError> {
        let pause_id = SessionPauseId::new(self.next_pause_id.fetch_add(1, Ordering::Relaxed));
        let mut coordination = self.active_runs.lock().await;
        if coordination.shutdown || coordination.wiping {
            return Err(SessionPauseError::Unavailable {
                session_id: session_id.clone(),
            });
        }
        let state = coordination.sessions.get_mut(session_id).ok_or_else(|| {
            SessionPauseError::OwnershipLost {
                session_id: session_id.clone(),
            }
        })?;
        if state
            .completion_barrier
            .as_ref()
            .map(|barrier| barrier.run_id)
            != Some(run_id)
            || state.pause.is_some()
        {
            return Err(SessionPauseError::OwnershipLost {
                session_id: session_id.clone(),
            });
        }
        state.pause = Some(OwnedSessionPause {
            id: pause_id,
            source_cursor,
            timeline_boundary: terminal_cursor,
            mutation_cursor: None,
            continuation: None,
            reason,
        });
        let projected = state.coordination();
        drop(coordination);
        self.control.publish_coordination(session_id, projected);
        Ok(SessionPauseLease::new(
            session_id.clone(),
            pause_id,
            source_cursor,
            terminal_cursor,
        ))
    }

    async fn fail_active_start(
        &self,
        session_id: &SessionId,
        run_id: u64,
    ) -> Option<SessionRunStart> {
        let mut coordination = self.active_runs.lock().await;
        let state = coordination.sessions.get_mut(session_id)?;
        if state.active.as_ref()?.id != run_id {
            return None;
        }
        state.active = None;
        let next = pop_next_start(state, &self.run_drain, session_id);
        if next.is_none() && state.pause.is_none() && !state.blocked {
            coordination.sessions.remove(session_id);
        }
        let projected = coordination.sessions.get(session_id).map_or_else(
            SessionCoordinationState::default,
            SessionRunState::coordination,
        );
        drop(coordination);
        self.control.publish_coordination(session_id, projected);
        next
    }

    /// Start a durable session turn from a user prompt.
    pub fn prompt(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<S> {
        let context = self.run_context(&session_id);
        runtime
            .prompt(prompt)
            .with_session_store(self.store.clone(), session_id)
            .with_session_run_context(context)
    }

    /// Resume an existing durable session with explicit input.
    pub fn resume(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        input: ResumeInput,
    ) -> AgentRunBuilder<S> {
        let context = self.run_context(&session_id);
        AgentRunBuilder::resume(runtime, session_id, self.store.clone(), input)
            .with_session_run_context(context)
    }

    /// Resume an existing helper task with invocation-local attachments.
    /// Uses the supplied runtime and the normal session store and middleware.
    ///
    /// # Errors
    /// Rejects unsupported provider APIs or invalid model/input capabilities.
    pub fn resume_with_transient_input(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        input: crate::TransientInput,
    ) -> crate::Result<AgentRunBuilder<S>> {
        let builder =
            AgentRunBuilder::resume_with_transient_input(runtime, session_id, &self.store, input)?;
        let context = self.run_context(builder.session_id());
        Ok(builder.with_session_run_context(context))
    }

    /// Return the session store used by this coordinator.
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Search prompt-template resources available to a durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when no runtime factory exists or runtime/template search fails.
    pub async fn search_templates(
        &self,
        session_id: &SessionId,
        request: SessionTemplateSearchRequest,
    ) -> std::result::Result<Vec<SessionTemplateOption>, AgentSessionRunError> {
        let (runtime, middleware_state, incarnation) =
            self.resolve_runtime_for_template_search(session_id).await?;
        let mut middleware_state = middleware_state;
        let templates = match runtime.host_context.as_ref() {
            Some(host) => {
                host.search_templates(
                    crate::HostContext {
                        session_id: session_id.as_str(),
                        turn: 0,
                        cancellation: tokio_util::sync::CancellationToken::new(),
                        metadata: None,
                        state: &mut middleware_state,
                    },
                    &request,
                )
                .await
            }
            None => Ok(Vec::new()),
        };
        let _operation = self.validate_incarnation(session_id, incarnation).await?;
        Ok(templates?)
    }

    async fn resolve_runtime_for_template_search(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<
        (
            AgentRuntime,
            crate::middleware::context::MiddlewareStateLog,
            super::SessionIncarnation,
        ),
        AgentSessionRunError,
    > {
        let captured = self.capture_runtime_record(session_id).await?;
        let record = &captured.record;
        let middleware_state =
            crate::middleware::context::MiddlewareStateLog::from_snapshot(&record.snapshot());
        record.inference_settings.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::FailedPrecondition(
                "session runtime settings are required before prompting".to_owned(),
            )
        })?;
        let factory = self.runtime_factory.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::Internal(
                "session runtime factory is not configured".to_owned(),
            )
        })?;
        let runtime = factory
            .build_for_discovery(record)
            .await
            .map_err(super::AgentSessionRuntimeError::from);
        let _operation = self
            .validate_incarnation(session_id, captured.incarnation)
            .await?;
        let runtime = runtime?;
        Ok((runtime, middleware_state, captured.incarnation))
    }

    /// Resolve the immutable definition and inference captured in a durable session.
    /// Hosts may use this only for an exclusively owned helper or concrete-execution session.
    /// Normal run admission still verifies the session incarnation.
    pub async fn resolve_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<AgentRuntime, AgentSessionRunError> {
        self.resolve_runtime_with_identity(session_id)
            .await
            .map(|(runtime, _)| runtime)
    }

    async fn resolve_runtime_with_identity(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(AgentRuntime, SessionRunInferenceIdentity), AgentSessionRunError>
    {
        let captured = self.capture_runtime_record_for_cycle(session_id).await?;
        let record = &captured.record;
        let inference_settings = record.inference_settings.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::FailedPrecondition(
                "session runtime settings are required before prompting".to_owned(),
            )
        })?;
        let inference_identity = SessionRunInferenceIdentity::from_settings(
            inference_settings,
            record.execution_token().map_err(session_store_error)?,
        );
        let factory = self.runtime_factory.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::Internal(
                "session runtime factory is not configured".to_owned(),
            )
        })?;
        let runtime = factory
            .build(record)
            .await
            .map_err(super::AgentSessionRuntimeError::from);
        let _operation = self
            .validate_incarnation(session_id, captured.incarnation)
            .await?;
        self.store
            .load(session_id)
            .await
            .map_err(session_store_error)?
            .check_execution_token(&inference_identity.execution_token)
            .map_err(session_store_error)?;
        Ok((runtime?, inference_identity))
    }

    async fn set_active_inference_identity(
        &self,
        session_id: &SessionId,
        run_id: u64,
        identity: SessionRunInferenceIdentity,
    ) {
        let mut coordination = self.active_runs.lock().await;
        if let Some(active) = coordination
            .sessions
            .get_mut(session_id)
            .and_then(|state| state.active.as_mut())
            .filter(|active| active.id == run_id)
        {
            active.inference_identity = Some(identity);
        }
        let projected = coordination
            .sessions
            .get(session_id)
            .map(SessionRunState::coordination);
        drop(coordination);
        if let Some(projected) = projected {
            self.control.publish_coordination(session_id, projected);
        }
    }

    async fn capture_runtime_record(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<CapturedRuntimeRecord, AgentSessionRunError> {
        let operation = self.control.session(session_id.clone()).operation().await;
        let record = self.load(session_id).await?.record;
        operation.register_loaded_summary(record.summary());
        Ok(CapturedRuntimeRecord {
            record,
            incarnation: operation.incarnation(),
        })
    }

    async fn capture_runtime_record_for_cycle(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<CapturedRuntimeRecord, AgentSessionRunError> {
        let operation = self.control.session(session_id.clone()).operation().await;
        let mut record = self.load(session_id).await?.record;
        operation.register_loaded_summary(record.summary());
        let incarnation = operation.incarnation();
        if let Some(update) = self
            .runtime_factory
            .as_ref()
            .map(|factory| factory.configuration_for_cycle(&record))
            .transpose()?
            .flatten()
        {
            if !update.timeline.is_empty() {
                let receipt = self
                    .store
                    .append_timeline(
                        session_id,
                        record.execution_token().map_err(session_store_error)?,
                        update.timeline,
                    )
                    .await
                    .map_err(session_store_error)?;
                record.adopt_commit(receipt.commit);
                record.append_journal_events(
                    receipt
                        .events
                        .iter()
                        .cloned()
                        .map(SessionJournalEvent::Timeline),
                );
                self.timeline.publish_all(receipt.events);
            }
            record = self
                .store
                .set_agent_definition_if_execution(
                    session_id,
                    record.execution_token().map_err(session_store_error)?,
                    update.configuration,
                )
                .await
                .map_err(session_store_error)?;
        }
        operation.publish_host_summary(record.summary());
        Ok(CapturedRuntimeRecord {
            record,
            incarnation,
        })
    }

    fn run_context(&self, session_id: &SessionId) -> SessionRunContext {
        SessionRunContext::new(
            self.control.session(session_id.clone()),
            self.timeline.clone(),
            self.event_sink.clone(),
        )
    }

    async fn validate_incarnation(
        &self,
        session_id: &SessionId,
        expected: super::SessionIncarnation,
    ) -> std::result::Result<super::SessionOperation, super::AgentSessionRuntimeError> {
        self.control
            .session(session_id.clone())
            .operation_for_incarnation(expected)
            .await
            .ok_or_else(stale_runtime_resolution)
    }

    async fn preflight_authentication(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), AgentSessionRunError> {
        match self.resolve_runtime(session_id).await {
            Err(error) if error.authentication_failure().is_some() => Err(error),
            _ => Ok(()),
        }
    }

    async fn persist_run_failure(
        &self,
        session_id: &SessionId,
        error: &AgentSessionRunError,
        incarnation: super::SessionIncarnation,
        token: &CancellationToken,
    ) -> Result<()> {
        let operation = self
            .control
            .session(session_id.clone())
            .operation_for_incarnation(incarnation)
            .await
            .ok_or_else(|| crate::LlmError::Protocol {
                provider: "session",
                message: "session changed during runtime resolution".to_owned(),
            })?;
        if token.is_cancelled() {
            return Ok(());
        }
        AgentSessionRuntime::new(&self.store)
            .with_session_run_context(self.run_context(session_id), operation.incarnation())
            .fail_run(
                session_id,
                error.to_string(),
                error.authentication_failure(),
            )
            .await?;
        Ok(())
    }

    async fn validate_pause_owner(
        &self,
        lease: &SessionPauseLease,
    ) -> std::result::Result<(), SessionPauseError> {
        let coordination = self.active_runs.lock().await;
        let pause = coordination
            .sessions
            .get(&lease.session_id)
            .and_then(|state| state.pause.as_ref());
        if pause.is_some_and(|pause| {
            pause.id == lease.id
                && pause.source_cursor == lease.source_cursor
                && pause.timeline_boundary == lease.timeline_boundary
        }) {
            Ok(())
        } else {
            Err(SessionPauseError::OwnershipLost {
                session_id: lease.session_id.clone(),
            })
        }
    }

    async fn block_session(&self, session_id: &SessionId, reason: &'static str) {
        let mut coordination = self.active_runs.lock().await;
        let state = coordination.sessions.entry(session_id.clone()).or_default();
        let coordination_changed = state.coordination() != SessionCoordinationState::default();
        cancel_session_state(session_id, state, reason);
        state.blocked = true;
        drop(coordination);
        if coordination_changed {
            self.control
                .publish_coordination(session_id, SessionCoordinationState::default());
        }
    }

    async fn unblock_session(&self, session_id: &SessionId) {
        let mut coordination = self.active_runs.lock().await;
        if coordination.sessions.get(session_id).is_some_and(|state| {
            state.blocked
                && state.active.is_none()
                && state.pause.is_none()
                && state.queued.is_empty()
        }) {
            coordination.sessions.remove(session_id);
        }
    }

    async fn begin_wipe(&self) {
        let mut coordination = self.active_runs.lock().await;
        coordination.wiping = true;
        for (session_id, state) in &mut coordination.sessions {
            cancel_session_state(session_id, state, "session store wiped");
            state.blocked = true;
        }
        let invalidated = coordination.sessions.keys().cloned().collect::<Vec<_>>();
        drop(coordination);
        for session_id in invalidated {
            self.control
                .publish_coordination(&session_id, SessionCoordinationState::default());
        }
    }

    async fn end_wipe(&self) {
        let mut coordination = self.active_runs.lock().await;
        coordination.wiping = false;
        coordination.sessions.clear();
    }
}

fn matching_pause_mut<'a>(
    coordination: &'a mut RunCoordination,
    lease: &SessionPauseLease,
) -> std::result::Result<&'a mut OwnedSessionPause, SessionPauseError> {
    coordination
        .sessions
        .get_mut(&lease.session_id)
        .and_then(|state| state.pause.as_mut())
        .filter(|pause| {
            pause.id == lease.id
                && pause.source_cursor == lease.source_cursor
                && pause.timeline_boundary == lease.timeline_boundary
        })
        .ok_or_else(|| SessionPauseError::OwnershipLost {
            session_id: lease.session_id.clone(),
        })
}

fn validate_pause_boundary(
    lease: &SessionPauseLease,
    record: &super::SessionRecord,
) -> std::result::Result<(), SessionPauseError> {
    let actual = record.timeline_events().last().map(|event| event.cursor);
    if actual == Some(lease.timeline_boundary) {
        Ok(())
    } else {
        Err(SessionPauseError::BoundaryChanged {
            session_id: lease.session_id.clone(),
            expected: lease.timeline_boundary,
            actual,
        })
    }
}

fn pause_store_error(
    lease: &SessionPauseLease,
    error: super::SessionStoreError,
) -> SessionPauseError {
    match error {
        super::SessionStoreError::MaskMutation {
            error: super::SessionMaskError::BoundaryMismatch { actual, .. },
        } => SessionPauseError::BoundaryChanged {
            session_id: lease.session_id.clone(),
            expected: lease.timeline_boundary,
            actual,
        },
        error => SessionPauseError::Store(error),
    }
}

fn start_queued(
    state: &mut SessionRunState,
    queued: QueuedSessionRun,
    run_drain: &Arc<RunDrain>,
) -> SessionRunStart {
    let token = CancellationToken::new();
    let id = queued.id;
    state.active = Some(ActiveSessionRun {
        id,
        token: token.clone(),
        inference_identity: queued.inference_identity.clone(),
    });
    SessionRunStart {
        id,
        token,
        runtime: queued.runtime,
        runtime_factory: queued.runtime_factory,
        inference_identity: queued.inference_identity,
        input: queued.input,
        outcome_txs: vec![(queued.id, queued.outcome_tx)],
        limits: queued.limits,
        activity: run_drain.begin(),
    }
}

fn pop_next_start(
    state: &mut SessionRunState,
    run_drain: &Arc<RunDrain>,
    session_id: &SessionId,
) -> Option<SessionRunStart> {
    let queued = loop {
        let queued = state.queued.pop_front()?;
        if queued
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            cancel_queued(session_id, VecDeque::from([queued]), "origin canceled");
        } else {
            break queued;
        }
    };
    let independently_cancellable = queued.cancellation.is_some();
    let token = queued.cancellation.unwrap_or_default();
    let id = queued.id;
    let mut input = queued.input;
    let limits = queued.limits;
    let runtime = queued.runtime;
    let runtime_factory = queued.runtime_factory;
    let inference_identity = queued.inference_identity;
    let mut outcome_txs = vec![(queued.id, queued.outcome_tx)];
    if let AgentSessionRunInput::Prompt(first) = input {
        let mut prompts = vec![first];
        while let Some(next) = state.queued.front() {
            if !matches!(next.input, AgentSessionRunInput::Prompt(_))
                || independently_cancellable
                || next.cancellation.is_some()
                || next.limits != limits
                || next.runtime.is_some()
                || !match (&runtime_factory, &next.runtime_factory) {
                    (Some(left), Some(right)) => left.same_host_binding(right),
                    (None, None) => true,
                    _ => false,
                }
            {
                break;
            }
            let next = state.queued.pop_front().expect("queued prompt exists");
            let AgentSessionRunInput::Prompt(prompt) = next.input else {
                unreachable!("front input was checked as prompt");
            };
            prompts.push(prompt);
            outcome_txs.push((next.id, next.outcome_tx));
        }
        input = if prompts.len() == 1 {
            AgentSessionRunInput::Prompt(prompts.pop().expect("single prompt exists"))
        } else {
            AgentSessionRunInput::Prompts(prompts)
        };
    }
    state.active = Some(ActiveSessionRun {
        id,
        token: token.clone(),
        inference_identity: inference_identity.clone(),
    });
    Some(SessionRunStart {
        id,
        token,
        runtime,
        runtime_factory,
        inference_identity,
        input,
        outcome_txs,
        limits,
        activity: run_drain.begin(),
    })
}

fn cancel_session_state(session_id: &SessionId, state: &mut SessionRunState, reason: &'static str) {
    if let Some(active) = state.active.take() {
        active.token.cancel();
    }
    if let Some(pause) = state.pause.take()
        && let Some(continuation) = pause.continuation
    {
        cancel_queued(session_id, VecDeque::from([continuation]), reason);
    }
    cancel_queued(session_id, std::mem::take(&mut state.queued), reason);
}

fn stale_runtime_resolution() -> super::AgentSessionRuntimeError {
    super::AgentSessionRuntimeError::FailedPrecondition(
        "session changed during runtime resolution".to_owned(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn drain_run<S>(
    coordinator: SessionCoordinator<S>,
    runtime: Option<AgentRuntime>,
    session_id: SessionId,
    input: AgentSessionRunInput,
    limits: AgentRunLimits,
    run_id: u64,
    token: CancellationToken,
) -> RunResult
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    async {
        let runtime = match runtime {
            Some(runtime) => runtime,
            None => {
                let incarnation = coordinator
                    .control
                    .session(session_id.clone())
                    .operation()
                    .await
                    .incarnation();
                match coordinator.resolve_runtime_with_identity(&session_id).await {
                    Ok((runtime, identity)) => {
                        coordinator
                            .set_active_inference_identity(&session_id, run_id, identity.clone())
                            .await;
                        runtime
                    }
                    Err(error) => {
                        if let Err(persistence_error) = coordinator
                            .persist_run_failure(
                                &session_id,
                                &error,
                                incarnation,
                                &token,
                            )
                            .await
                        {
                            tracing::error!(session_id = %session_id.as_str(), %error, %persistence_error, "failed to persist session runtime failure");
                        }
                        return Err(error);
                    }
                }
            },
        };
        let validated = runtime.completion_validation.is_some();
        let mut context = coordinator.run_context(&session_id);
        if let Some(identity) = coordinator.active_runs.lock().await.sessions.get(&session_id)
            .and_then(|state| state.active.as_ref()).filter(|active| active.id == run_id)
            .and_then(|active| active.inference_identity.as_ref()) {
            context = context.with_execution_token(identity.execution_token.clone());
        }
        let mut stream = if let AgentSessionRunInput::HostTool(call) = input {
            let run = super::compat::EphemeralSessionRun {
                provider: runtime.provider,
                model: runtime.model,
                model_cost: runtime.model_cost,
                input: ResumeInput::Continue,
                system_prompt_override: None,
                tools: runtime.tools,
                middleware_chain: runtime.middleware_chain,
                retry_config: runtime.retry_config,

                max_provider_turns: runtime.max_provider_turns,
                host_context: runtime.host_context,
                session_id: session_id.clone(),
                session_store: coordinator.store.clone(),
                session_run_context: Some(context),
                cancellation_token: token.clone(),
                event_sink: coordinator.event_sink.clone(),
                completion_validation: None,
            };
            crate::agent::r#loop::AgentLoopState::direct_tool_stream(run, call).await?
        } else {
        let builder = match input {
            AgentSessionRunInput::HostTool(_) => unreachable!(),
            AgentSessionRunInput::Continue => {
                coordinator.resume(&runtime, session_id.clone(), ResumeInput::Continue)
            }
            AgentSessionRunInput::Prompt(prompt) => {
                coordinator.prompt(&runtime, session_id.clone(), prompt)
            }
            AgentSessionRunInput::Prompts(prompts) => {
                coordinator.resume(&runtime, session_id.clone(), ResumeInput::Prompts(prompts))
            }
            AgentSessionRunInput::ToolResults(results) => coordinator.resume(
                &runtime,
                session_id.clone(),
                ResumeInput::ToolResults(results),
            ),
        }.with_session_run_context(context)
        .with_run_limits(limits);
        builder.with_cancellation_token(token.clone()).stream()
        };

        let mut assistant_text = String::new();
        while let Some(event) = stream.next().await {
            let event = event?;
            if !validated && is_assistant_text_delta(&event)
                && let TimelineContent::TextDelta { delta } = &event.content
            {
                assistant_text.push_str(delta);
            }
            if validated && event.channel.as_ref().is_some_and(|channel| channel.as_str() == CHANNEL_ASSISTANT_TEXT)
                && let TimelineContent::TextSnapshot { text, .. } = &event.content {
                assistant_text.clone_from(text);
            }
        }

        let session = coordinator.load(&session_id).await?;
        let status = session.record.summary().status;
        Ok(AgentSessionRunOutcome {
            session_id,
            run_id,
            status,
            assistant_text: (!assistant_text.is_empty()).then_some(assistant_text),
        })
    }
    .await
}

fn is_assistant_text_delta(event: &AgentTimelineEvent) -> bool {
    event
        .channel
        .as_ref()
        .is_some_and(|channel| channel.as_str() == CHANNEL_ASSISTANT_TEXT)
        && matches!(event.content, TimelineContent::TextDelta { .. })
}

fn cancel_queued(session_id: &SessionId, queued: VecDeque<QueuedSessionRun>, reason: &'static str) {
    for queued in queued {
        drop(queued.outcome_tx.send(Err(AgentSessionRunError::canceled(
            session_id.clone(),
            queued.id,
            reason,
        ))));
    }
}

fn send_run_outcome(outcome_txs: Vec<(u64, RunOutcomeSender)>, outcome: RunResult) {
    let mut outcome = Some(outcome);
    for (run_id, outcome_tx) in outcome_txs {
        let Some(current) = outcome.take() else {
            break;
        };
        match current {
            Ok(mut run_outcome) => {
                run_outcome.run_id = run_id;
                let send_outcome = Ok(run_outcome.clone());
                outcome = Some(Ok(run_outcome));
                drop(outcome_tx.send(send_outcome));
            }
            Err(error) => {
                let message = error.to_string();
                drop(outcome_tx.send(Err(error)));
                outcome = Some(Err(AgentSessionRunError::Agent(LlmError::Protocol {
                    provider: "session-run",
                    message,
                })));
            }
        }
    }
}

const fn terminal_state(outcome: &RunResult) -> &'static str {
    match outcome {
        Ok(outcome) => match &outcome.status {
            SessionStatus::Failed { .. } => "error",
            SessionStatus::Suspended(super::Suspension::AwaitingToolResults { .. }) => {
                "awaiting_tool_results"
            }
            SessionStatus::Suspended(super::Suspension::Interrupted { .. }) => "interrupted",
            SessionStatus::Finished
            | SessionStatus::Idle
            | SessionStatus::BuildingRequest
            | SessionStatus::StreamingProvider
            | SessionStatus::RetryingProvider { .. }
            | SessionStatus::EvaluatingToolCall
            | SessionStatus::RunningTool => "finished",
        },
        Err(AgentSessionRunError::Canceled { .. }) => "interrupted",
        Err(
            AgentSessionRunError::AlreadyRunning { .. }
            | AgentSessionRunError::Paused { .. }
            | AgentSessionRunError::Unavailable { .. }
            | AgentSessionRunError::OutcomeDropped { .. }
            | AgentSessionRunError::Runtime(_)
            | AgentSessionRunError::Agent(_),
        ) => "error",
    }
}

fn coordinator_error(message: &str) -> LlmError {
    LlmError::Protocol {
        provider: "session-host",
        message: message.into(),
    }
}

fn session_store_error(error: impl std::fmt::Display) -> crate::LlmError {
    crate::LlmError::Protocol {
        provider: "session-store",
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroU64,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
        },
    };

    use futures_util::{StreamExt as _, stream};
    use serde_json::json;
    use tokio::sync::{Mutex, Notify};

    use super::*;
    use crate::{
        BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionReplayItem,
        CompletionRequest, LlmStream, ModelApi, ModelRef, PendingTimelineEvent, ProviderId,
        Revision, SessionCompletionCoordinationFuture, SessionCompletionCoordinationHandler,
        SessionCompletionCoordinationPreparation, SessionCopy, SessionJournalEvent, SessionRecord,
        SessionStart, SessionStartMessage, SessionStatus, SessionStoreResult, SessionTimelineEvent,
        TimelineCursor, ToolCall,
        agent::session::{
            FileCredentialStore, InferenceProviderSettings, MemorySessionStore, SessionListEvent,
        },
    };

    fn pause_reason() -> SessionCoordinationReason {
        SessionCoordinationReason::new(
            "host-operation",
            Some("agent-test".to_owned()),
            Some("manual".to_owned()),
        )
        .unwrap()
    }

    fn completion_reason() -> SessionCoordinationReason {
        SessionCoordinationReason::new(
            "context-compaction",
            Some("compact-and-continue".to_owned()),
            Some("run-completed".to_owned()),
        )
        .unwrap()
    }

    struct TextOnlyProvider;

    struct BlockingCompletionHandler {
        calls: AtomicUsize,
        entered: Notify,
    }

    impl SessionCompletionCoordinationHandler for BlockingCompletionHandler {
        fn prepare(
            &self,
            _snapshot: crate::SessionRunCompletionSnapshot,
        ) -> SessionCompletionCoordinationFuture {
            let first = self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0;
            self.entered.notify_one();
            Box::pin(async move {
                if first {
                    std::future::pending().await
                } else {
                    Ok(SessionCompletionCoordinationPreparation::Continue)
                }
            })
        }
    }

    enum TestFinalization {
        Fail,
        Delay {
            entered: Arc<Notify>,
            release: Arc<Notify>,
        },
        Panic,
    }

    struct FinalizingCompletionHandler(StdMutex<Option<TestFinalization>>);

    struct TestCompletionFinalizer(TestFinalization);

    impl crate::SessionCompletionAdmissionFinalizer for TestCompletionFinalizer {
        fn finalize(
            self: Box<Self>,
            _lease: SessionPauseLease,
        ) -> crate::SessionCompletionFinalizationFuture {
            match self.0 {
                TestFinalization::Fail => {
                    Box::pin(async { Err("injected finalization failure".to_owned()) })
                }
                TestFinalization::Delay { entered, release } => Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Err("injected delayed finalization failure".to_owned())
                }),
                TestFinalization::Panic => panic!("injected worker construction panic"),
            }
        }
    }

    impl SessionCompletionCoordinationHandler for FinalizingCompletionHandler {
        fn prepare(
            &self,
            _snapshot: crate::SessionRunCompletionSnapshot,
        ) -> SessionCompletionCoordinationFuture {
            let finalization = self.0.lock().expect("finalization lock").take();
            Box::pin(async move {
                Ok(finalization.map_or(
                    SessionCompletionCoordinationPreparation::Continue,
                    |behavior| SessionCompletionCoordinationPreparation::Claim {
                        reason: completion_reason(),
                        finalizer: Box::new(TestCompletionFinalizer(behavior)),
                    },
                ))
            })
        }
    }

    struct FailingCompletionHandler;

    impl SessionCompletionCoordinationHandler for FailingCompletionHandler {
        fn prepare(
            &self,
            _snapshot: crate::SessionRunCompletionSnapshot,
        ) -> SessionCompletionCoordinationFuture {
            Box::pin(async { Err("injected completion failure".to_owned()) })
        }
    }

    struct SnapshotCompletionHandler(
        StdMutex<Option<tokio::sync::oneshot::Sender<crate::SessionRunCompletionSnapshot>>>,
    );

    struct ControlledClaimHandler {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        lease_tx: StdMutex<Option<tokio::sync::oneshot::Sender<SessionPauseLease>>>,
        reason: SessionCoordinationReason,
    }

    struct LeaseSendingFinalizer(tokio::sync::oneshot::Sender<SessionPauseLease>);

    impl crate::SessionCompletionAdmissionFinalizer for LeaseSendingFinalizer {
        fn finalize(
            self: Box<Self>,
            lease: SessionPauseLease,
        ) -> crate::SessionCompletionFinalizationFuture {
            Box::pin(async move {
                let sender = self.0;
                Ok(Box::pin(async move {
                    drop(sender.send(lease));
                }) as crate::SessionCompletionWorkerFuture)
            })
        }
    }

    impl SessionCompletionCoordinationHandler for ControlledClaimHandler {
        fn prepare(
            &self,
            _snapshot: crate::SessionRunCompletionSnapshot,
        ) -> SessionCompletionCoordinationFuture {
            self.entered.notify_one();
            let release = Arc::clone(&self.release);
            let reason = self.reason.clone();
            let sender = self.lease_tx.lock().expect("lease sender lock").take();
            Box::pin(async move {
                release.notified().await;
                Ok(match sender {
                    Some(sender) => SessionCompletionCoordinationPreparation::Claim {
                        reason,
                        finalizer: Box::new(LeaseSendingFinalizer(sender)),
                    },
                    None => SessionCompletionCoordinationPreparation::Continue,
                })
            })
        }
    }

    impl SessionCompletionCoordinationHandler for SnapshotCompletionHandler {
        fn prepare(
            &self,
            snapshot: crate::SessionRunCompletionSnapshot,
        ) -> SessionCompletionCoordinationFuture {
            if let Some(sender) = self.0.lock().expect("snapshot sender lock").take() {
                drop(sender.send(snapshot));
            }
            Box::pin(async { Ok(SessionCompletionCoordinationPreparation::Continue) })
        }
    }

    struct UsageProvider;

    #[derive(Clone)]
    struct SlowLoadStore {
        inner: MemorySessionStore,
        delay_next_load: Arc<std::sync::atomic::AtomicBool>,
        entered: Arc<Notify>,
        configuration_barrier: Option<Arc<(Notify, Notify)>>,
    }

    impl SessionStore for SlowLoadStore {
        async fn set_agent_definition_if_execution(
            &self,
            id: &SessionId,
            expected: crate::ExecutionToken,
            definition: Option<Value>,
        ) -> SessionStoreResult<SessionRecord> {
            if let Some(barrier) = &self.configuration_barrier {
                barrier.0.notify_one();
                tokio::time::timeout(Duration::from_secs(5), barrier.1.notified())
                    .await
                    .unwrap();
            }
            self.inner
                .set_agent_definition_if_execution(id, expected, definition)
                .await
        }
        fn create(
            &self,
            request: SessionCreateRequest,
        ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
            self.inner.create(request)
        }
        fn load(
            &self,
            id: &SessionId,
        ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
            let inner = self.inner.clone();
            let id = id.clone();
            let delay = Arc::clone(&self.delay_next_load);
            let entered = Arc::clone(&self.entered);
            async move {
                if delay.swap(false, AtomicOrdering::SeqCst) {
                    entered.notify_one();
                    std::future::pending::<()>().await;
                }
                inner.load(&id).await
            }
        }
        fn delete(
            &self,
            id: &SessionId,
        ) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
            self.inner.delete(id)
        }
        fn wipe(&self) -> impl std::future::Future<Output = SessionStoreResult<()>> + Send {
            self.inner.wipe()
        }
        fn list(
            &self,
        ) -> impl std::future::Future<Output = SessionStoreResult<Vec<SessionSummary>>> + Send
        {
            self.inner.list()
        }
        fn append_timeline(
            &self,
            id: &SessionId,
            revision: crate::ExecutionToken,
            events: Vec<PendingTimelineEvent>,
        ) -> impl std::future::Future<Output = SessionStoreResult<crate::TimelineAppendReceipt>> + Send
        {
            self.inner.append_timeline(id, revision, events)
        }
        fn append_journal(
            &self,
            id: &SessionId,
            revision: crate::ExecutionToken,
            events: Vec<SessionJournalEvent>,
        ) -> impl std::future::Future<Output = SessionStoreResult<crate::SessionCommitReceipt>> + Send
        {
            self.inner.append_journal(id, revision, events)
        }
        fn mutate(
            &self,
            id: &SessionId,
            mutation: crate::SessionMutation,
        ) -> impl std::future::Future<Output = SessionStoreResult<crate::SessionMutationOutcome>> + Send
        {
            self.inner.mutate(id, mutation)
        }
        fn set_metadata_value_conditionally(
            &self,
            id: &SessionId,
            key: String,
            value: Value,
            condition: super::MetadataSetCondition,
        ) -> impl std::future::Future<Output = SessionStoreResult<super::MetadataSetOutcome>> + Send
        {
            self.inner
                .set_metadata_value_conditionally(id, key, value, condition)
        }
        fn remove_metadata_value(
            &self,
            id: &SessionId,
            key: String,
        ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
            self.inner.remove_metadata_value(id, key)
        }
        fn set_inference_settings(
            &self,
            id: &SessionId,
            settings: Option<InferenceSettings>,
        ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
            self.inner.set_inference_settings(id, settings)
        }
        fn set_agent_definition(
            &self,
            id: &SessionId,
            definition: Option<serde_json::Value>,
        ) -> impl std::future::Future<Output = SessionStoreResult<SessionRecord>> + Send {
            self.inner.set_agent_definition(id, definition)
        }
        fn timeline(
            &self,
            id: &SessionId,
            query: TimelineQuery,
        ) -> impl std::future::Future<Output = SessionStoreResult<Vec<AgentTimelineEvent>>> + Send
        {
            self.inner.timeline(id, query)
        }
        fn timeline_item_text(
            &self,
            id: &SessionId,
            item_id: &TimelineItemId,
        ) -> impl std::future::Future<Output = SessionStoreResult<crate::TimelineItemText>> + Send
        {
            self.inner.timeline_item_text(id, item_id)
        }
    }

    impl LlmStream for UsageProvider {
        fn name(&self) -> &'static str {
            "usage_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::ProviderMetadata(
                        crate::ProviderTurnMetadata {
                            provider: "test".to_owned(),
                            url: String::new(),
                            model: "test-model".to_owned(),
                            usage: Some(crate::TokenUsage {
                                input_tokens: Some(120),
                                output_tokens: Some(7),
                                cache_read_tokens: Some(3),
                                ..crate::TokenUsage::default()
                            }),
                            cost: None,
                            resume: Value::Null,
                        },
                    )),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    struct RecordingEventSink {
        events: StdMutex<Vec<crate::SessionEvent>>,
        fail: bool,
    }

    impl crate::SessionEventSink for RecordingEventSink {
        fn send(&self, event: crate::SessionEvent) -> crate::SessionEventSinkFuture {
            self.events.lock().expect("event sink lock").push(event);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err("injected event failure".to_owned())
                } else {
                    Ok(())
                }
            })
        }
    }

    impl LlmStream for TextOnlyProvider {
        fn name(&self) -> &'static str {
            "text_only_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("ok".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    struct AuthenticationFailingProvider;

    impl LlmStream for AuthenticationFailingProvider {
        fn name(&self) -> &'static str {
            "authentication_failing_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async {
                Err(LlmError::AuthenticationFailure {
                    failure: crate::AuthenticationFailure {
                        target: crate::AuthTarget::provider("configured-provider"),
                        reason: crate::AuthenticationFailureReason::CredentialRejected,
                        host_context: None,
                    },
                    message: "provider rejected the configured credential".to_owned(),
                })
            })
        }
    }

    struct ToolThenTextProvider {
        requests: AtomicUsize,
    }

    impl ToolThenTextProvider {
        const fn new() -> Self {
            Self {
                requests: AtomicUsize::new(0),
            }
        }
    }

    impl LlmStream for ToolThenTextProvider {
        fn name(&self) -> &'static str {
            "tool_then_text_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let request = self.requests.fetch_add(1, AtomicOrdering::SeqCst);
            Box::pin(async move {
                let events = match request {
                    0 => vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "call-1",
                            "missing",
                            json!({}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ],
                    1 => vec![
                        Ok(CompletionEvent::Delta("finished".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ],
                    _ => panic!("unexpected provider request {request}"),
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[derive(Debug)]
    struct QueuedPromptProvider {
        release_active: Arc<Notify>,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl QueuedPromptProvider {
        fn new() -> Self {
            Self {
                release_active: Arc::new(Notify::new()),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn release_active(&self) {
            self.release_active.notify_waiters();
        }

        async fn wait_for_request_count(&self, count: usize) {
            loop {
                if self.requests.lock().await.len() >= count {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }

    impl LlmStream for QueuedPromptProvider {
        fn name(&self) -> &'static str {
            "queued_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let prompts = request
                    .replay_items()
                    .into_iter()
                    .filter_map(|item| match item {
                        CompletionReplayItem::Prompt(prompt) => Some(prompt.to_owned()),
                        CompletionReplayItem::Turn(_) => None,
                    })
                    .collect::<Vec<_>>();
                self.requests.lock().await.push(prompts.clone());
                if prompts.last().is_some_and(|prompt| prompt == "active") {
                    let release_active = self.release_active.clone();
                    let stream = stream::once(async move {
                        release_active.notified().await;
                        Ok(CompletionEvent::Delta("active done".to_owned()))
                    })
                    .chain(stream::iter(vec![Ok(CompletionEvent::Finished)]));
                    return Ok(Box::pin(stream) as BoxCompletionStream);
                }
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("queued done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    fn test_inference_settings() -> InferenceSettings {
        InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: "test-model".to_owned(),
                model_cost: None,
                headers: BTreeMap::default(),
                oauth: None,
                reasoning_effort: None,
            },
        }
    }

    fn create_request(
        inference_settings: Option<InferenceSettings>,
        agent_definition: Option<serde_json::Value>,
        metadata_values: BTreeMap<String, Value>,
    ) -> SessionCreateRequest {
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = inference_settings;
        request.agent_definition = agent_definition;
        request.metadata_values = metadata_values;
        request
    }

    fn with_fake_host(factory: AgentSessionRuntimeFactory) -> AgentSessionRuntimeFactory {
        struct ProviderHost(AgentSessionRuntimeFactory);
        impl crate::AgentHostRuntimeFactory for ProviderHost {
            fn build<'a>(&'a self, record: &'a SessionRecord) -> crate::HostRuntimeFuture<'a> {
                Box::pin(async move {
                    let settings = record.inference_settings.as_ref().ok_or_else(|| {
                        crate::LlmError::Protocol {
                            provider: "test-host",
                            message: "inference settings required".into(),
                        }
                    })?;
                    self.0
                        .provider_builder(settings)
                        .await
                        .map(|builder| builder.build())
                        .map_err(|error| AgentSessionRunError::from(error).into())
                })
            }
        }
        factory
            .clone()
            .with_host_runtime_factory(Arc::new(ProviderHost(factory)))
    }

    async fn runtime_sessions<P>(provider: Arc<P>) -> SessionCoordinator<MemorySessionStore>
    where
        P: LlmStream + 'static,
    {
        let environment = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
            std::env::temp_dir().join("waltr-agent-queue-test-auth.json"),
        ))
        .with_runtime_provider(move |_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                provider.clone(),
            ))
        });

        SessionCoordinator::new(MemorySessionStore::new())
            .with_runtime_factory(with_fake_host(environment))
    }

    async fn completed_session(sessions: &SessionCoordinator<MemorySessionStore>) -> SessionId {
        let source = r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec: {}
";
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                Some(crate::agent::session::opaque_test_configuration(
                    "test",
                    vec![source.to_owned()],
                )),
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let id = session.record.id;
        sessions
            .submit_prompt(id.clone(), "seed", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .unwrap();
        id
    }

    async fn next_coordination(
        watch: &mut SessionListSubscription,
        phase: SessionCoordinationPhase,
    ) -> SessionSummary {
        loop {
            let update = tokio::time::timeout(Duration::from_secs(1), watch.next())
                .await
                .expect("coordination update timeout")
                .expect("coordination update");
            if let SessionListEvent::Upsert(summary) = update.event
                && summary.coordination.phase == phase
            {
                assert_projection_is_atomic(&summary);
                return summary;
            }
        }
    }

    async fn next_coordination_matching(
        watch: &mut SessionListSubscription,
        predicate: impl Fn(&SessionCoordinationState) -> bool,
    ) -> SessionSummary {
        loop {
            let update = tokio::time::timeout(Duration::from_secs(1), watch.next())
                .await
                .expect("coordination update timeout")
                .expect("coordination update");
            if let SessionListEvent::Upsert(summary) = update.event
                && predicate(&summary.coordination)
            {
                assert_projection_is_atomic(&summary);
                return summary;
            }
        }
    }

    fn assert_projection_is_atomic(summary: &SessionSummary) {
        assert_eq!(
            summary.progress,
            crate::SessionProgress::from_state(&summary.status, &summary.coordination)
        );
        assert_eq!(
            summary.capabilities,
            crate::SessionCapabilities::from_state(&summary.status, &summary.coordination)
        );
    }

    fn authentication_runtime_error() -> crate::AgentSessionRuntimeError {
        crate::AgentSessionRuntimeError::Authentication {
            target: crate::AuthTarget::provider("configured-provider"),
            message: "provider OAuth credential is required".to_owned(),
            reason: crate::AuthenticationFailureReason::CredentialRequired,
            host_context: None,
        }
    }

    #[tokio::test]
    async fn fresh_create_publishes_initial_timeline_and_list_once() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = SessionId::new("fresh-publication");
        let mut timeline = sessions.watch_timeline(id.clone());
        let mut list = sessions.watch_sessions();
        let mut request = SessionCreateRequest::new(id.clone());
        request.start = SessionStart::Fresh {
            messages: vec![SessionStartMessage::user("hello").unwrap()],
        };

        let created = sessions.create(request).await.unwrap();

        let update = timeline.next().await.expect("initial timeline event");
        let SessionTimelineEvent::Event(event) = update.event else {
            panic!("expected persisted timeline event");
        };
        assert_eq!(event.cursor, TimelineCursor::new(1));
        assert_eq!(event.item_id.as_str(), "i1");
        assert_eq!(event.session_id, id);
        assert_eq!(created.record.revision, Revision::ZERO);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), timeline.next())
                .await
                .is_err()
        );
        let list_update = list.next().await.expect("initial list event");
        assert!(matches!(
            list_update.event,
            SessionListEvent::Upsert(summary) if summary.id == created.record.id
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), list.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn copied_create_publishes_preserved_and_configured_timeline_once() {
        let mut source = SessionRecord::new("copy-publication-source");
        source.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "source prompt".to_owned(),
            },
            SessionJournalEvent::Timeline(
                PendingTimelineEvent::user_text_snapshot(
                    source.id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "source prompt",
                )
                .with_cursor(TimelineCursor::new(5)),
            ),
            SessionJournalEvent::TurnFinished { turn: 0 },
        ];
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = SessionId::new("copy-publication-destination");
        let mut timeline = sessions.watch_timeline(id.clone());
        let mut list = sessions.watch_sessions();
        let mut request = SessionCreateRequest::new(id.clone());
        request.start = SessionStart::Copy {
            copy: SessionCopy::from_record(&source, TimelineCursor::new(5)).unwrap(),
            messages: vec![SessionStartMessage::user("destination prompt").unwrap()],
        };

        let created = sessions.create(request).await.unwrap();

        let first = timeline.next().await.expect("copied timeline event");
        let SessionTimelineEvent::Event(first) = first.event else {
            panic!("expected copied timeline event");
        };
        let second = timeline.next().await.expect("configured timeline event");
        let SessionTimelineEvent::Event(second) = second.event else {
            panic!("expected configured timeline event");
        };
        assert_eq!(first.cursor, TimelineCursor::new(5));
        assert_eq!(first.item_id.as_str(), "i5");
        assert_eq!(first.session_id, id);
        assert_eq!(second.cursor, TimelineCursor::new(6));
        assert_eq!(second.item_id.as_str(), "i6");
        assert_eq!(second.session_id, id);
        assert_eq!(created.record.revision, Revision::ZERO);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), timeline.next())
                .await
                .is_err()
        );
        let list_update = list.next().await.expect("initial list event");
        assert!(matches!(
            list_update.event,
            SessionListEvent::Upsert(summary) if summary.id == created.record.id
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), list.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn copy_start_publishes_only_new_post_durable_semantic_event() {
        let sink = Arc::new(RecordingEventSink {
            events: StdMutex::new(Vec::new()),
            fail: false,
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new())
            .with_session_event_sink(sink.clone());
        let id = SessionId::new("atomic-initial");
        let mut source = SessionRecord::new("copy-source");
        source.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "copied source prompt".to_owned(),
            },
            SessionJournalEvent::Timeline(
                PendingTimelineEvent::user_text_snapshot(
                    source.id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "copied source prompt",
                )
                .with_cursor(TimelineCursor::new(1)),
            ),
            SessionJournalEvent::TurnFinished { turn: 0 },
        ];
        let text = "configured destination prompt".to_owned();
        let mut request = SessionCreateRequest::new(id.clone());
        request
            .metadata_values
            .insert("host.key".to_owned(), json!("value"));
        request.start = SessionStart::Copy {
            copy: SessionCopy::from_record(&source, TimelineCursor::new(1)).unwrap(),
            messages: vec![SessionStartMessage::user(text.clone()).unwrap()],
        };

        let session = sessions.create(request).await.unwrap();

        for _ in 0..10 {
            if sink.events.lock().expect("event sink lock").len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let events = sink.events.lock().expect("event sink lock");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            crate::SessionEvent::UserMessageCreated {
                id: "user-message:atomic-initial:j3".to_owned(),
                session_id: id,
                incarnation: session.record.metadata.incarnation.clone(),
                prompt_index: 1,
                user_message_count: 2,
                text,
                metadata: session.record.metadata.values.clone(),
                revision: session.record.revision,
                cursor: Some(TimelineCursor::new(2)),
            }
        );
    }

    #[tokio::test]
    async fn copy_creation_never_publishes_copied_prompts_or_failed_start_messages() {
        let sink = Arc::new(RecordingEventSink {
            events: StdMutex::new(Vec::new()),
            fail: false,
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new())
            .with_session_event_sink(sink.clone());
        let id = SessionId::new("seed-only");
        let mut source = SessionRecord::new("copy-source");
        source.journal = vec![
            SessionJournalEvent::PromptAppended {
                prompt: "copied source prompt".to_owned(),
            },
            SessionJournalEvent::Timeline(
                PendingTimelineEvent::user_text_snapshot(
                    source.id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "copied source prompt",
                )
                .with_cursor(TimelineCursor::new(1)),
            ),
            SessionJournalEvent::TurnFinished { turn: 0 },
        ];
        let mut request = SessionCreateRequest::new(id.clone());
        request.start = SessionStart::Copy {
            copy: SessionCopy::from_record(&source, TimelineCursor::new(1)).unwrap(),
            messages: Vec::new(),
        };
        sessions.create(request).await.unwrap();

        let mut duplicate = SessionCreateRequest::new(id);
        duplicate.start = SessionStart::Fresh {
            messages: vec![SessionStartMessage::user("must not publish").unwrap()],
        };
        assert!(sessions.create(duplicate).await.is_err());
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(sink.events.lock().expect("event sink lock").is_empty());
    }

    #[tokio::test]
    async fn metadata_update_publishes_session_upsert() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap();
        let mut watch = sessions.watch_sessions();

        let updated = sessions
            .set_metadata_value(&session.record.id, "host.key", json!("value"))
            .await
            .unwrap();
        let update = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
            .await
            .unwrap()
            .expect("metadata update should publish");

        assert!(update.sequence > 0);
        match update.event {
            SessionListEvent::Upsert(summary) => {
                assert_eq!(summary.id, session.record.id);
                assert_eq!(summary.revision, updated.record.revision);
                assert_eq!(
                    summary.metadata_values.get("host.key"),
                    Some(&json!("value"))
                );
            }
            SessionListEvent::Deleted(id) => panic!("expected upsert, got deleted {id:?}"),
            SessionListEvent::Cleared => panic!("expected upsert, got cleared"),
        }
    }

    #[tokio::test]
    async fn unchanged_conditional_metadata_suppresses_session_upsert() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request
            .metadata_values
            .insert("host.key".to_owned(), json!("original"));
        let session = sessions.create(request).await.unwrap();
        let mut watch = sessions.watch_sessions();

        let outcome = sessions
            .set_metadata_value_conditionally(
                &session.record.id,
                "host.key",
                json!("replacement"),
                MetadataSetCondition::Absent,
            )
            .await
            .unwrap();

        assert!(!outcome.changed);
        assert_eq!(
            outcome.record.metadata_value("host.key"),
            Some(&json!("original"))
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), watch.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn runtime_creation_preserves_agent_definition_bundle() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let bundle = crate::agent::session::opaque_test_configuration(
            "selected",
            vec![
                r"apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: selected }
spec: {}
"
                .to_owned(),
            ],
        );

        let created = sessions
            .create(create_request(None, Some(bundle.clone()), BTreeMap::new()))
            .await
            .expect("create session with agent definition");

        assert_eq!(created.record.agent_definition, Some(bundle));
    }

    #[tokio::test]
    async fn mutation_gate_linearizes_runtime_record_capture() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let id = session.record.id;
        let held = sessions.control.session(id.clone()).operation().await;
        let capture_sessions = sessions.clone();
        let capture_id = id.clone();
        let capture = tokio::spawn(async move {
            capture_sessions
                .capture_runtime_record(&capture_id)
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        let mut updated = test_inference_settings();
        updated.provider.model_id = "new-model".to_owned();
        let update_sessions = sessions.clone();
        let update_id = id.clone();
        let update = tokio::spawn(async move {
            update_sessions
                .set_inference_settings(&update_id, Some(updated))
                .await
                .unwrap()
        });
        drop(held);

        let captured = capture.await.unwrap();
        update.await.unwrap();
        assert_eq!(
            captured
                .record
                .inference_settings
                .unwrap()
                .provider
                .model_id,
            "test-model"
        );

        let captured_after = sessions.capture_runtime_record(&id).await.unwrap();
        assert_eq!(
            captured_after
                .record
                .inference_settings
                .unwrap()
                .provider
                .model_id,
            "new-model"
        );
    }

    #[tokio::test]
    async fn runtime_startup_does_not_hold_mutation_gate() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let id = session.record.id;

        let _runtime = sessions.resolve_runtime(&id).await.unwrap();
        let _guard = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            sessions.control.session(id).operation(),
        )
        .await
        .expect("runtime startup must release the mutation gate");
    }

    #[tokio::test]
    async fn cycle_configuration_commit_checks_captured_execution_after_optional_audit() {
        struct CountingProvider(Arc<AtomicUsize>);
        impl LlmStream for CountingProvider {
            fn name(&self) -> &'static str {
                "cycle-commit-test"
            }
            fn stream_request<'a>(
                &'a self,
                request: CompletionRequest<'a>,
            ) -> crate::BoxStreamFuture<'a> {
                Box::pin(async move {
                    self.0.fetch_add(1, AtomicOrdering::SeqCst);
                    TextOnlyProvider.stream_request(request).await
                })
            }
        }
        struct CycleHost {
            audit: bool,
            builds: Arc<AtomicUsize>,
            requests: Arc<AtomicUsize>,
        }
        impl crate::AgentHostRuntimeFactory for CycleHost {
            fn configuration_for_cycle(
                &self,
                record: &SessionRecord,
            ) -> Result<Option<crate::HostConfigurationUpdate>> {
                Ok(Some(crate::HostConfigurationUpdate {
                    configuration: Some(serde_json::json!({"authorized":"cycle"})),
                    timeline: if self.audit {
                        vec![PendingTimelineEvent::assistant_text_snapshot(
                            record.id.clone(),
                            0,
                            crate::PendingTimelineItemId::New,
                            "configuration audit",
                        )]
                    } else {
                        Vec::new()
                    },
                }))
            }
            fn build<'a>(&'a self, record: &'a SessionRecord) -> crate::HostRuntimeFuture<'a> {
                Box::pin(async move {
                    self.builds.fetch_add(1, AtomicOrdering::SeqCst);
                    assert_eq!(
                        record.agent_definition,
                        Some(serde_json::json!({"authorized":"cycle"}))
                    );
                    assert_eq!(
                        record.metadata_value("title"),
                        Some(&serde_json::json!("latest"))
                    );
                    Ok(AgentRuntime::builder(
                        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                        Arc::new(CountingProvider(self.requests.clone())),
                    )
                    .build())
                })
            }
        }
        for audit in [false, true] {
            for change in ["definition", "settings", "journal", "metadata"] {
                let barrier = Arc::new((Notify::new(), Notify::new()));
                let inner = MemorySessionStore::new();
                let store = SlowLoadStore {
                    inner: inner.clone(),
                    delay_next_load: Arc::new(AtomicBool::new(false)),
                    entered: Arc::new(Notify::new()),
                    configuration_barrier: Some(barrier.clone()),
                };
                let builds = Arc::new(AtomicUsize::new(0));
                let requests = Arc::new(AtomicUsize::new(0));
                let directory = tempfile::tempdir().unwrap();
                let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
                    directory.path().join("auth.json"),
                ))
                .with_host_runtime_factory(Arc::new(CycleHost {
                    audit,
                    builds: builds.clone(),
                    requests: requests.clone(),
                }));
                let sessions = SessionCoordinator::new(store).with_runtime_factory(factory);
                let id = sessions
                    .create(create_request(
                        Some(test_inference_settings()),
                        None,
                        BTreeMap::new(),
                    ))
                    .await
                    .unwrap()
                    .record
                    .id;
                let interleave = async {
                    tokio::time::timeout(Duration::from_secs(5), barrier.0.notified())
                        .await
                        .unwrap();
                    let before = inner.load(&id).await.unwrap();
                    assert_eq!(before.execution_sequence.get(), u64::from(audit));
                    match change {
                        "definition" => {
                            inner
                                .set_agent_definition(
                                    &id,
                                    Some(serde_json::json!({"external":"preserved"})),
                                )
                                .await
                                .unwrap();
                        }
                        "settings" => {
                            let mut settings = test_inference_settings();
                            settings.provider.model_id = "external".into();
                            inner
                                .set_inference_settings(&id, Some(settings))
                                .await
                                .unwrap();
                        }
                        "journal" => {
                            inner
                                .append_journal(
                                    &id,
                                    before.execution_token().unwrap(),
                                    vec![SessionJournalEvent::MiddlewareStateUpdated {
                                        namespace: "external".into(),
                                        state: serde_json::json!(true),
                                    }],
                                )
                                .await
                                .unwrap();
                        }
                        "metadata" => {
                            inner
                                .set_metadata_value(
                                    &id,
                                    "title".into(),
                                    serde_json::json!("latest"),
                                )
                                .await
                                .unwrap();
                        }
                        _ => unreachable!(),
                    }
                    let committed = inner.load(&id).await.unwrap();
                    barrier.1.notify_one();
                    committed
                };
                let (submission, committed) =
                    tokio::time::timeout(Duration::from_secs(10), async {
                        tokio::join!(
                            sessions.submit_prompt(
                                id.clone(),
                                "run",
                                RunConflictPolicy::QueueIfActive
                            ),
                            interleave
                        )
                    })
                    .await
                    .unwrap();
                if change == "metadata" {
                    tokio::time::timeout(Duration::from_secs(5), submission.unwrap().handle.wait())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(builds.load(AtomicOrdering::SeqCst), 1);
                    assert_eq!(requests.load(AtomicOrdering::SeqCst), 1);
                    let final_record = inner.load(&id).await.unwrap();
                    assert_eq!(
                        final_record.metadata_value("title"),
                        Some(&serde_json::json!("latest"))
                    );
                    assert_eq!(
                        final_record.agent_definition,
                        Some(serde_json::json!({"authorized":"cycle"}))
                    );
                } else {
                    let error = submission
                        .err()
                        .expect("external execution must reject cycle commit");
                    assert!(
                        error.to_string().contains("stale session execution token"),
                        "{error:?}"
                    );
                    assert_eq!(inner.load(&id).await.unwrap(), committed);
                    assert_eq!(builds.load(AtomicOrdering::SeqCst), 0);
                    assert_eq!(requests.load(AtomicOrdering::SeqCst), 0);
                }
            }
        }
    }

    #[tokio::test]
    async fn runtime_resolution_adopts_own_configuration_and_rejects_external_execution_changes() {
        struct BlockingHost {
            entered: Notify,
            release: Notify,
        }
        impl crate::AgentHostRuntimeFactory for BlockingHost {
            fn configuration_for_cycle(
                &self,
                _: &SessionRecord,
            ) -> Result<Option<crate::HostConfigurationUpdate>> {
                Ok(Some(crate::HostConfigurationUpdate {
                    configuration: Some(serde_json::json!({"authorized":"cycle"})),
                    timeline: Vec::new(),
                }))
            }
            fn build<'a>(&'a self, record: &'a SessionRecord) -> crate::HostRuntimeFuture<'a> {
                Box::pin(async move {
                    assert_eq!(
                        record.agent_definition,
                        Some(serde_json::json!({"authorized":"cycle"}))
                    );
                    assert_eq!(record.execution_sequence.get(), 1);
                    self.entered.notify_one();
                    tokio::time::timeout(Duration::from_secs(5), self.release.notified())
                        .await
                        .unwrap();
                    Ok(AgentRuntime::builder(
                        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                        Arc::new(TextOnlyProvider),
                    )
                    .build())
                })
            }
        }
        for execution_change in [false, true] {
            let host = Arc::new(BlockingHost {
                entered: Notify::new(),
                release: Notify::new(),
            });
            let directory = tempfile::tempdir().unwrap();
            let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
                directory.path().join("auth.json"),
            ))
            .with_host_runtime_factory(host.clone());
            let sessions =
                SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
            let id = sessions
                .create(create_request(
                    Some(test_inference_settings()),
                    None,
                    BTreeMap::new(),
                ))
                .await
                .unwrap()
                .record
                .id;
            let update = async {
                tokio::time::timeout(Duration::from_secs(5), host.entered.notified())
                    .await
                    .unwrap();
                if execution_change {
                    sessions
                        .set_agent_definition(&id, Some(serde_json::json!({"external":"change"})))
                        .await
                        .unwrap();
                } else {
                    sessions
                        .set_metadata_value(&id, "title", serde_json::json!("concurrent"))
                        .await
                        .unwrap();
                }
                host.release.notify_one();
            };
            let (resolved, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(sessions.resolve_runtime(&id), update)
            })
            .await
            .unwrap();
            if execution_change {
                assert!(
                    resolved
                        .unwrap_err()
                        .to_string()
                        .contains("stale session execution token")
                );
            } else {
                assert!(resolved.is_ok());
                assert_eq!(
                    sessions
                        .load(&id)
                        .await
                        .unwrap()
                        .record
                        .metadata_value("title"),
                    Some(&serde_json::json!("concurrent"))
                );
            }
        }
    }

    #[tokio::test]
    async fn append_timeline_publishes_session_summary_upsert() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap();
        let mut watch = sessions.watch_sessions();

        sessions
            .append_timeline(
                &session.record.id,
                vec![PendingTimelineEvent::assistant_text_delta(
                    session.record.id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "hi",
                )],
            )
            .await
            .unwrap();

        let update = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
            .await
            .unwrap()
            .expect("timeline append should publish session summary");

        match update.event {
            SessionListEvent::Upsert(summary) => {
                assert_eq!(summary.id, session.record.id);
                assert!(matches!(summary.status, SessionStatus::StreamingProvider));
            }
            SessionListEvent::Deleted(id) => panic!("expected upsert, got deleted {id:?}"),
            SessionListEvent::Cleared => panic!("expected upsert, got cleared"),
        }
    }

    #[tokio::test]
    async fn queued_prompt_runs_are_coalesced_into_adjacent_user_messages() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(provider.clone()).await;
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let session_id = session.record.id.clone();

        let active = sessions
            .submit_prompt(
                session_id.clone(),
                "active",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;

        let queued_one = sessions
            .submit_prompt(
                session_id.clone(),
                "queued one",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        let queued_two = sessions
            .submit_prompt(
                session_id.clone(),
                "queued two",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();

        provider.release_active();
        active.handle.wait().await.unwrap();
        let first = queued_one.handle.wait().await.unwrap();
        let second = queued_two.handle.wait().await.unwrap();

        assert_eq!(first.run_id, queued_one.run_id);
        assert_eq!(second.run_id, queued_two.run_id);
        provider.wait_for_request_count(2).await;
        let requests = provider.requests.lock().await.clone();
        assert_eq!(requests[0], vec!["active"]);
        assert_eq!(
            &requests[1][requests[1].len() - 2..],
            ["queued one", "queued two"]
        );

        let record = sessions.load(&session_id).await.unwrap().record;
        let snapshot = record.snapshot();
        assert_eq!(snapshot.prompts, vec!["active", "queued one", "queued two"]);
        assert_eq!(snapshot.prompt_turns[1].turn, snapshot.prompt_turns[2].turn);
    }

    #[tokio::test]
    async fn pause_acquisition_is_exclusive_rejects_active_and_requires_completed_boundary() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let id = completed_session(&sessions).await;

        let (left, right) = tokio::join!(
            sessions.acquire_pause_reject_busy(&id, pause_reason()),
            sessions.acquire_pause_reject_busy(&id, pause_reason())
        );
        let lease = match (left, right) {
            (Ok(lease), Err(SessionPauseError::Paused { .. }))
            | (Err(SessionPauseError::Paused { .. }), Ok(lease)) => lease,
            outcomes => panic!("unexpected acquisition race outcomes: {outcomes:?}"),
        };
        sessions.release_pause(&lease).await.unwrap();

        let active = sessions
            .submit_prompt(id.clone(), "active", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        provider.wait_for_request_count(2).await;
        assert!(matches!(
            sessions
                .acquire_pause_reject_busy(&id, pause_reason())
                .await,
            Err(SessionPauseError::Active { .. })
        ));
        provider.release_active();
        active.handle.wait().await.unwrap();

        let held = sessions.control.session(id.clone()).operation().await;
        assert!(matches!(
            sessions
                .acquire_pause_reject_busy(&id, pause_reason())
                .await,
            Err(SessionPauseError::LifecycleBusy { .. })
        ));
        drop(held);

        let incomplete = sessions
            .create(create_request(
                Some(test_inference_settings()),
                Some(crate::agent::session::opaque_test_configuration(
                    "test",
                    vec![
                        r"apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec: {}"
                            .to_owned(),
                    ],
                )),
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            sessions
                .acquire_pause_reject_busy(&incomplete.record.id, pause_reason())
                .await,
            Err(SessionPauseError::CompletedBoundaryRequired { .. })
        ));
    }

    #[tokio::test]
    async fn paused_prompts_queue_coalesce_and_release_is_idempotent() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let id = completed_session(&sessions).await;
        let lease = sessions
            .acquire_pause_reject_busy(&id, pause_reason())
            .await
            .unwrap();

        let first = sessions
            .submit_prompt(id.clone(), "queued one", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let second = sessions
            .submit_prompt(id.clone(), "queued two", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        assert!(matches!(
            sessions
                .submit_prompt(id.clone(), "reject", RunConflictPolicy::RejectIfActive)
                .await,
            Err(AgentSessionRunError::Paused { .. })
        ));
        assert!(matches!(
            sessions
                .submit_prompt(id.clone(), "replace", RunConflictPolicy::ReplaceActive)
                .await,
            Err(AgentSessionRunError::Paused { .. })
        ));
        assert_eq!(provider.requests.lock().await.len(), 1);

        sessions.release_pause(&lease).await.unwrap();
        sessions.release_pause(&lease).await.unwrap();
        first.handle.wait().await.unwrap();
        second.handle.wait().await.unwrap();
        provider.wait_for_request_count(2).await;
        let requests = provider.requests.lock().await.clone();
        assert_eq!(
            &requests[1][requests[1].len() - 2..],
            ["queued one", "queued two"]
        );
    }

    #[tokio::test]
    async fn paused_mask_mutation_accepts_metadata_but_rejects_timeline_change() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(provider).await;
        let id = completed_session(&sessions).await;
        let lease = sessions
            .acquire_pause_reject_busy(&id, pause_reason())
            .await
            .unwrap();
        sessions
            .set_metadata_value(&id, "routine", json!("running"))
            .await
            .unwrap();

        let mut mutation = SessionMaskMutation::new(None);
        mutation.add.push(crate::SessionMaskAddition::new(
            "context.compaction-prefix",
            crate::ThroughCursor::new(lease.source_cursor()),
            crate::ReplacementUserContent::new("compact summary"),
        ));
        let outcome = sessions
            .apply_paused_mask_mutation(&lease, mutation)
            .await
            .unwrap();
        let capture = sessions.capture_paused_effective(&lease).await.unwrap();
        assert_eq!(capture.through_cursor, lease.source_cursor());
        let effective = sessions
            .load(&id)
            .await
            .unwrap()
            .record
            .materialize_context(crate::SessionContextMode::Effective);
        assert_eq!(effective.current_prompt.as_deref(), Some("compact summary"));
        assert!(outcome.cursor > lease.source_cursor());

        sessions
            .append_timeline(
                &id,
                vec![PendingTimelineEvent::assistant_text_delta(
                    id.clone(),
                    1,
                    crate::PendingTimelineItemId::New,
                    "late",
                )],
            )
            .await
            .unwrap();
        assert!(matches!(
            sessions
                .apply_paused_mask_mutation(&lease, SessionMaskMutation::new(None))
                .await,
            Err(SessionPauseError::BoundaryChanged { .. })
        ));
        sessions.release_pause(&lease).await.unwrap();
    }

    #[tokio::test]
    async fn effective_watch_invalidates_for_mask_add_subtract_and_rollback() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(provider).await;
        let id = completed_session(&sessions).await;
        let lease = sessions
            .acquire_pause_reject_busy(&id, pause_reason())
            .await
            .unwrap();
        let mut effective_watch = sessions.watch_timeline_projection(id.clone());
        let mut raw_watch = sessions.watch_timeline(id.clone());
        let initial = sessions
            .timeline_projection(
                &id,
                SessionTimelineMode::Effective,
                TimelineQuery::forward(None, 64),
            )
            .await
            .unwrap();

        let mut addition = SessionMaskMutation::new(None);
        addition.add.push(crate::SessionMaskAddition::new(
            "summary",
            crate::ThroughCursor::new(lease.source_cursor()),
            crate::ReplacementUserContent::new("compact summary"),
        ));
        let added = sessions
            .apply_paused_mask_mutation(&lease, addition)
            .await
            .unwrap();
        let add_update = effective_watch.next().await.unwrap();
        assert!(matches!(
            add_update.event,
            crate::SessionTimelineProjectionUpdateEvent::Invalidated {
                projection_revision,
                ..
            } if projection_revision == added.revision && projection_revision > initial.projection_revision
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), raw_watch.next())
                .await
                .is_err(),
            "mask addition must not enter the raw watch"
        );
        let added_page = sessions
            .timeline_projection(
                &id,
                SessionTimelineMode::Effective,
                TimelineQuery::forward(None, 64),
            )
            .await
            .unwrap();
        assert!(added_page.events.iter().any(|event| matches!(
            event.presentation,
            crate::SessionTimelinePresentation::MaskReplacement { .. }
        )));

        let mut subtraction = SessionMaskMutation::new(None);
        subtraction
            .subtract
            .ids
            .push(added.added_masks[0].id.clone());
        let subtracted = sessions
            .apply_paused_mask_mutation(&lease, subtraction)
            .await
            .unwrap();
        let subtract_update = effective_watch.next().await.unwrap();
        assert!(matches!(
            subtract_update.event,
            crate::SessionTimelineProjectionUpdateEvent::Invalidated {
                projection_revision,
                ..
            } if projection_revision == subtracted.revision && projection_revision > added.revision
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), raw_watch.next())
                .await
                .is_err(),
            "mask subtraction must not enter the raw watch"
        );

        let mut readdition = SessionMaskMutation::new(None);
        readdition.add.push(crate::SessionMaskAddition::new(
            "summary",
            crate::ThroughCursor::new(lease.source_cursor()),
            crate::ReplacementUserContent::new("compact summary"),
        ));
        let readded = sessions
            .apply_paused_mask_mutation(&lease, readdition)
            .await
            .unwrap();
        drop(effective_watch.next().await.unwrap());
        sessions.release_pause(&lease).await.unwrap();

        let rolled_back = sessions.rollback_to_turn(id.clone(), 0).await.unwrap();
        let rollback_update = effective_watch.next().await.unwrap();
        assert!(matches!(
            rollback_update.event,
            crate::SessionTimelineProjectionUpdateEvent::Invalidated {
                projection_revision,
                ..
            } if projection_revision == rolled_back.revision() && projection_revision > readded.revision
        ));
        assert!(matches!(
            raw_watch.next().await.unwrap().event,
            SessionTimelineEvent::Truncated { .. }
        ));
        let rollback_page = sessions
            .timeline_projection(
                &id,
                SessionTimelineMode::Effective,
                TimelineQuery::forward(None, 64),
            )
            .await
            .unwrap();
        assert_eq!(rollback_page.projection_revision, rolled_back.revision());
        assert!(rollback_page.events.iter().all(|event| !matches!(
            event.presentation,
            crate::SessionTimelinePresentation::MaskReplacement { .. }
        )));
    }

    #[tokio::test]
    async fn armed_continuation_runs_before_queued_prompts_without_rollback() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let id = completed_session(&sessions).await;
        let lease = sessions
            .acquire_pause_reject_busy(&id, pause_reason())
            .await
            .unwrap();
        let queued = sessions
            .submit_prompt(
                id.clone(),
                "after continuation",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        let mut mutation = SessionMaskMutation::new(None);
        mutation.add.push(crate::SessionMaskAddition::new(
            "context.compaction-prefix",
            crate::ThroughCursor::new(lease.source_cursor()),
            crate::ReplacementUserContent::new("compact summary"),
        ));
        let mutation = sessions
            .apply_paused_mask_mutation(&lease, mutation)
            .await
            .unwrap();
        let continuation = sessions
            .arm_paused_continuation(&lease, mutation.cursor, AgentRunLimits::default())
            .await
            .unwrap();

        sessions.release_pause(&lease).await.unwrap();
        continuation.handle.wait().await.unwrap();
        queued.handle.wait().await.unwrap();
        provider.wait_for_request_count(3).await;
        let requests = provider.requests.lock().await.clone();
        assert_eq!(requests[1], ["compact summary"]);
        assert_eq!(
            requests[2].last().map(String::as_str),
            Some("after continuation")
        );
        let record = sessions.load(&id).await.unwrap().record;
        assert!(
            !record
                .journal
                .iter()
                .any(|event| matches!(event, SessionJournalEvent::Rollback { .. }))
        );
    }

    #[tokio::test]
    async fn delete_wipe_and_shutdown_invalidate_pauses_and_cancel_queues() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(provider).await;

        let deleted_id = completed_session(&sessions).await;
        let deleted_lease = sessions
            .acquire_pause_reject_busy(&deleted_id, pause_reason())
            .await
            .unwrap();
        let deleted_queue = sessions
            .submit_prompt(
                deleted_id.clone(),
                "deleted",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        sessions.delete(&deleted_id).await.unwrap();
        assert!(matches!(
            deleted_queue.handle.wait().await,
            Err(AgentSessionRunError::Canceled { .. })
        ));
        assert!(matches!(
            sessions.release_pause(&deleted_lease).await,
            Err(SessionPauseError::OwnershipLost { .. })
        ));

        let wiped_id = completed_session(&sessions).await;
        let wiped_lease = sessions
            .acquire_pause_reject_busy(&wiped_id, pause_reason())
            .await
            .unwrap();
        let wiped_queue = sessions
            .submit_prompt(wiped_id.clone(), "wiped", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        sessions.wipe_sessions().await.unwrap();
        assert!(matches!(
            wiped_queue.handle.wait().await,
            Err(AgentSessionRunError::Canceled { .. })
        ));
        assert!(matches!(
            sessions.release_pause(&wiped_lease).await,
            Err(SessionPauseError::OwnershipLost { .. })
        ));

        let shutdown_sessions = runtime_sessions(Arc::new(QueuedPromptProvider::new())).await;
        let shutdown_id = completed_session(&shutdown_sessions).await;
        let shutdown_lease = shutdown_sessions
            .acquire_pause_reject_busy(&shutdown_id, pause_reason())
            .await
            .unwrap();
        let shutdown_queue = shutdown_sessions
            .submit_prompt(
                shutdown_id.clone(),
                "shutdown",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        shutdown_sessions.shutdown().await;
        assert!(matches!(
            shutdown_queue.handle.wait().await,
            Err(AgentSessionRunError::Canceled { .. })
        ));
        assert!(matches!(
            shutdown_sessions
                .submit_prompt(shutdown_id, "late", RunConflictPolicy::QueueIfActive)
                .await,
            Err(AgentSessionRunError::Unavailable { .. })
        ));
        assert!(matches!(
            shutdown_sessions.release_pause(&shutdown_lease).await,
            Err(SessionPauseError::OwnershipLost { .. })
        ));
    }

    #[tokio::test]
    async fn shutdown_waits_for_active_run_cleanup() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let session_id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;
        let submission = sessions
            .submit_prompt(
                session_id.clone(),
                "active",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;

        sessions.shutdown().await;

        let _outcome = tokio::time::timeout(Duration::from_millis(10), submission.handle.wait())
            .await
            .expect("shutdown must resolve the active run handle before returning");
        assert!(sessions.active_state(&session_id).await.is_none());
        assert_eq!(sessions.run_drain.active.load(AtomicOrdering::Acquire), 0);
    }

    #[tokio::test]
    async fn submitted_prompt_limit_fails_before_second_logical_dispatch() {
        let provider = Arc::new(ToolThenTextProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let session_id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;

        let submission = sessions
            .submit_prompt_with_limits(
                session_id,
                "limited",
                RunConflictPolicy::QueueIfActive,
                AgentRunLimits::default()
                    .with_max_provider_turns(NonZeroU64::new(1).expect("positive limit")),
            )
            .await
            .unwrap();
        let outcome = submission.handle.wait().await.unwrap();

        assert_eq!(provider.requests.load(AtomicOrdering::SeqCst), 1);
        assert!(matches!(
            outcome.status,
            SessionStatus::Failed { message, .. }
                if message == "maximum provider turn limit exceeded"
        ));
    }

    #[tokio::test]
    async fn existing_unlimited_prompt_submission_still_dispatches_until_finished() {
        let provider = Arc::new(ToolThenTextProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let session_id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;

        let submission = sessions
            .submit_prompt(session_id, "unlimited", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let outcome = submission.handle.wait().await.unwrap();

        assert_eq!(provider.requests.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(outcome.status, SessionStatus::Finished);
        assert_eq!(outcome.assistant_text.as_deref(), Some("finished"));
    }

    #[tokio::test]
    async fn queued_prompts_with_different_limits_are_not_coalesced() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let session_id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;
        let active = sessions
            .submit_prompt(
                session_id.clone(),
                "active",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let limited = sessions
            .submit_prompt_with_limits(
                session_id.clone(),
                "limited",
                RunConflictPolicy::QueueIfActive,
                AgentRunLimits::default()
                    .with_max_provider_turns(NonZeroU64::new(1).expect("positive limit")),
            )
            .await
            .unwrap();
        let unlimited = sessions
            .submit_prompt(session_id, "unlimited", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();

        provider.release_active();
        active.handle.wait().await.unwrap();
        limited.handle.wait().await.unwrap();
        unlimited.handle.wait().await.unwrap();
        provider.wait_for_request_count(3).await;

        let requests = provider.requests.lock().await.clone();
        assert_eq!(requests[1].last().map(String::as_str), Some("limited"));
        assert_eq!(requests[2].last().map(String::as_str), Some("unlimited"));
    }

    #[tokio::test]
    async fn asynchronous_provider_authentication_failure_is_durable() {
        let sessions = runtime_sessions(Arc::new(AuthenticationFailingProvider)).await;
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let session_id = session.record.id.clone();

        let submission = sessions
            .submit_prompt(
                session_id.clone(),
                "authenticate",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .expect("submission returns before provider failure");
        let error = submission
            .handle
            .wait()
            .await
            .expect_err("provider authentication should fail");
        assert!(matches!(error, AgentSessionRunError::Agent(_)));

        let status = sessions
            .load(&session_id)
            .await
            .unwrap()
            .record
            .summary()
            .status;
        assert!(matches!(
            status,
            SessionStatus::Failed {
                authentication_failure: Some(crate::AuthenticationFailure {
                    target: crate::AuthTarget::Provider { ref key },
                    reason: crate::AuthenticationFailureReason::CredentialRejected,
                    ..
                }),
                ..
            } if key == "configured-provider"
        ));
    }

    async fn cancellable_fixture() -> (
        tempfile::TempDir,
        SessionCoordinator<MemorySessionStore>,
        Arc<QueuedPromptProvider>,
        SessionId,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let provider = Arc::new(QueuedPromptProvider::new());
        let runtime_provider = provider.clone();
        let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
            directory.path().join("auth.json"),
        ))
        .with_runtime_provider(move |_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                runtime_provider.clone(),
            ))
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new())
            .with_runtime_factory(with_fake_host(factory));
        let id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;
        (directory, sessions, provider, id)
    }

    #[tokio::test]
    async fn semantic_user_event_contains_the_committed_prompt_cursor() {
        struct Sink(StdMutex<Option<tokio::sync::oneshot::Sender<crate::SessionEvent>>>);
        impl crate::SessionEventSink for Sink {
            fn send(&self, event: crate::SessionEvent) -> crate::SessionEventSinkFuture {
                if let Some(tx) = self.0.lock().unwrap().take() {
                    let _sent = tx.send(event);
                }
                Box::pin(async { Ok(()) })
            }
        }
        let (_directory, sessions, provider, id) = cancellable_fixture().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sessions = sessions.with_session_event_sink(Arc::new(Sink(StdMutex::new(Some(tx)))));
        let submission = sessions
            .submit_prompt_cancellable(
                id.clone(),
                "durable first message".into(),
                RunConflictPolicy::QueueIfActive,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        let crate::SessionEvent::UserMessageCreated { cursor, .. } = event;
        assert_eq!(cursor, Some(TimelineCursor::new(1)));
        assert!(sessions.load(&id).await.unwrap().record.timeline_events().any(|event| matches!(&event.content, TimelineContent::TextSnapshot { text } if text == "durable first message")));
        provider.wait_for_request_count(1).await;
        submission.handle.cancel_and_wait().await.unwrap();
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn exact_origin_cancellation_preserves_other_queued_work() {
        let (_directory, sessions, provider, id) = cancellable_fixture().await;
        let active = sessions
            .submit_prompt_cancellable(
                id.clone(),
                "active".into(),
                RunConflictPolicy::QueueIfActive,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let expired = sessions
            .submit_prompt_cancellable(
                id.clone(),
                "expired".into(),
                RunConflictPolicy::QueueIfActive,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let valid = sessions
            .submit_prompt(id.clone(), "valid", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        assert!(matches!(
            expired.handle.cancel_and_wait().await,
            Err(AgentSessionRunError::Canceled { .. })
        ));
        active.handle.cancel_and_wait().await.unwrap();
        assert_eq!(
            valid.handle.wait().await.unwrap().status,
            SessionStatus::Finished
        );
        let requests = provider.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|prompts| !prompts.iter().any(|prompt| prompt == "expired"))
        );
        drop(requests);
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn exact_origin_waiter_drop_does_not_cancel_valid_work() {
        let (_directory, sessions, provider, id) = cancellable_fixture().await;
        let token = CancellationToken::new();
        let active = sessions
            .submit_prompt_cancellable(
                id.clone(),
                "active".into(),
                RunConflictPolicy::QueueIfActive,
                token.clone(),
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let run_id = active.run_id;
        drop(active.handle);
        assert!(!token.is_cancelled());
        assert_eq!(sessions.active_state(&id).await.unwrap().run_id, run_id);
        let valid = sessions
            .submit_prompt(id, "valid", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        provider.release_active();
        valid.handle.wait().await.unwrap();
        assert_eq!(provider.requests.lock().await.len(), 2);
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn exact_origin_stale_handle_cannot_cancel_recreated_session() {
        let (_directory, sessions, provider, id) = cancellable_fixture().await;
        let token = CancellationToken::new();
        let old = sessions
            .submit_prompt_cancellable(
                id.clone(),
                "old".into(),
                RunConflictPolicy::QueueIfActive,
                token.clone(),
            )
            .await
            .unwrap();
        let stale = old.handle.cancellation().unwrap();
        old.handle.wait().await.unwrap();
        sessions.delete(&id).await.unwrap();
        let mut request = create_request(Some(test_inference_settings()), None, BTreeMap::new());
        request.id = id.clone();
        sessions.create(request).await.unwrap();
        let current = sessions
            .submit_prompt_cancellable(
                id.clone(),
                "active".into(),
                RunConflictPolicy::QueueIfActive,
                token.clone(),
            )
            .await
            .unwrap();
        provider.wait_for_request_count(2).await;
        stale.cancel().await;
        assert!(!token.is_cancelled());
        assert_eq!(
            sessions.active_state(&id).await.unwrap().run_id,
            current.run_id
        );
        provider.release_active();
        current.handle.wait().await.unwrap();
        sessions.shutdown().await;
    }

    #[tokio::test]
    async fn queued_runs_retain_their_origin_coordination_factory() {
        struct CompletionOrigin {
            label: &'static str,
            calls: Arc<StdMutex<Vec<&'static str>>>,
        }
        impl crate::SessionCompletionCoordinationHandler for CompletionOrigin {
            fn prepare(
                &self,
                _: crate::SessionRunCompletionSnapshot,
            ) -> crate::SessionCompletionCoordinationFuture {
                self.calls.lock().unwrap().push(self.label);
                Box::pin(async { Ok(crate::SessionCompletionCoordinationPreparation::Continue) })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let provider = Arc::new(QueuedPromptProvider::new());
        let runtime_provider = provider.clone();
        let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
            directory.path().join("auth.json"),
        ))
        .with_runtime_provider(move |_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                runtime_provider.clone(),
            ))
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let sinks = (0..3)
            .map(|_| {
                Arc::new(RecordingEventSink {
                    events: StdMutex::new(Vec::new()),
                    fail: false,
                })
            })
            .collect::<Vec<_>>();
        let scoped = ["A", "B", "C"]
            .into_iter()
            .zip(&sinks)
            .map(|(label, sink)| {
                sessions.clone().with_runtime_factory(
                    with_fake_host(factory.clone()).with_run_coordination(
                        sink.clone(),
                        Arc::new(CompletionOrigin {
                            label,
                            calls: calls.clone(),
                        }),
                    ),
                )
            })
            .collect::<Vec<_>>();
        let id = scoped[0]
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;
        let active = scoped[0]
            .submit_prompt(id.clone(), "active", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let second = scoped[1]
            .submit_prompt(id.clone(), "second", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let third = scoped[2]
            .submit_prompt(id, "third", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        provider.release_active();
        active.handle.wait().await.unwrap();
        second.handle.wait().await.unwrap();
        third.handle.wait().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), ["A", "B", "C"]);
        assert_eq!(
            provider.requests.lock().await.len(),
            3,
            "different origins must not batch"
        );
        for sink in sinks {
            assert_eq!(sink.events.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn queued_runtime_authentication_failure_is_durable() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let resolutions = Arc::new(AtomicU64::new(0));
        let resolution_count = Arc::clone(&resolutions);
        let runtime_provider = Arc::clone(&provider);
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
            directory.path().join("auth.json"),
        ))
        .with_runtime_provider(move |_| {
            if resolution_count.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(crate::AgentRuntimeProvider::new(
                    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                    Arc::clone(&runtime_provider),
                ))
            } else {
                Err(authentication_runtime_error())
            }
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new())
            .with_runtime_factory(with_fake_host(factory));
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let session_id = session.record.id.clone();
        let active = sessions
            .submit_prompt(
                session_id.clone(),
                "active",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let queued = sessions
            .submit_prompt(
                session_id.clone(),
                "queued",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();

        provider.release_active();
        active.handle.wait().await.unwrap();
        let error = queued
            .handle
            .wait()
            .await
            .expect_err("queued runtime resolution should fail");
        assert!(matches!(error, AgentSessionRunError::Runtime(_)));
        let status = sessions
            .load(&session_id)
            .await
            .unwrap()
            .record
            .summary()
            .status;
        assert!(matches!(
            status,
            SessionStatus::Failed {
                authentication_failure: Some(crate::AuthenticationFailure {
                    target: crate::AuthTarget::Provider { ref key },
                    reason: crate::AuthenticationFailureReason::CredentialRequired,
                    ..
                }),
                ..
            } if key == "configured-provider"
        ));
    }

    #[tokio::test]
    async fn queued_runtime_failure_is_published_before_release_and_next_run_survives() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let resolutions = Arc::new(AtomicU64::new(0));
        let runtime_provider = provider.clone();
        let directory = tempfile::tempdir().unwrap();
        let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
            directory.path().join("auth.json"),
        ))
        .with_runtime_provider(move |_| {
            if resolutions.fetch_add(1, Ordering::SeqCst) == 1 {
                Err(crate::AgentSessionRuntimeError::FailedPrecondition(
                    "runtime construction failed".to_owned(),
                ))
            } else {
                Ok(crate::AgentRuntimeProvider::new(
                    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                    runtime_provider.clone(),
                ))
            }
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new())
            .with_runtime_factory(with_fake_host(factory));
        let id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;
        let active = sessions
            .submit_prompt(id.clone(), "active", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let failed = sessions
            .submit(
                id.clone(),
                AgentSessionRunInput::Continue,
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        let next = sessions
            .submit_prompt(id.clone(), "next", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let mut watch = sessions.watch_sessions();
        provider.release_active();
        active.handle.wait().await.unwrap();
        failed.handle.wait().await.expect_err("runtime should fail");
        next.handle.wait().await.unwrap();
        let record = sessions.load(&id).await.unwrap().record;
        assert_eq!(record.summary().status, SessionStatus::Finished);
        assert_eq!(
            record
                .journal
                .iter()
                .filter(|event| matches!(event, SessionJournalEvent::Failed { .. }))
                .count(),
            1
        );
        loop {
            let update = tokio::time::timeout(Duration::from_secs(1), watch.next())
                .await
                .unwrap()
                .unwrap();
            if let SessionListEvent::Upsert(summary) = update.event
                && let SessionStatus::Failed {
                    message,
                    authentication_failure,
                } = summary.status
            {
                assert!(message.contains("runtime construction failed"));
                assert!(authentication_failure.is_none());
                assert!(
                    summary.coordination.phase == crate::SessionCoordinationPhase::Running,
                    "failure is published before releasing ownership"
                );
                break;
            }
        }
        assert_eq!(provider.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn continue_authentication_preflight_does_not_append_rollback() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(FileCredentialStore::new(
            directory.path().join("auth.json"),
        ))
        .with_runtime_provider(|_| Err(authentication_runtime_error()));
        let sessions = SessionCoordinator::new(MemorySessionStore::new())
            .with_runtime_factory(with_fake_host(factory));
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                None,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let session_id = session.record.id.clone();
        sessions
            .append_timeline(
                &session_id,
                vec![PendingTimelineEvent::assistant_text_delta(
                    session_id.clone(),
                    0,
                    crate::PendingTimelineItemId::New,
                    "before",
                )],
            )
            .await
            .unwrap();
        let before = sessions.load(&session_id).await.unwrap().record;

        let error = sessions
            .continue_from_cursor(session_id.clone(), crate::TimelineCursor::new(1))
            .await
            .expect_err("authentication preflight should reject continuation");
        assert!(error.authentication_failure().is_some());
        let after = sessions.load(&session_id).await.unwrap().record;
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.journal, before.journal);
    }

    struct PreparationFailure {
        template: bool,
        authentication: bool,
    }

    impl PreparationFailure {
        fn error(&self) -> LlmError {
            if self.authentication {
                LlmError::AuthenticationFailure {
                    failure: crate::AuthenticationFailure {
                        target: crate::AuthTarget::provider("preparation"),
                        reason: crate::AuthenticationFailureReason::CredentialRejected,
                        host_context: None,
                    },
                    message: "preparation failed".to_owned(),
                }
            } else {
                LlmError::Protocol {
                    provider: "test",
                    message: "preparation failed".to_owned(),
                }
            }
        }
    }

    impl crate::middleware::AgentMiddleware for PreparationFailure {
        async fn before_provider_request(
            &self,
            _ctx: &mut crate::middleware::context::RequestContext<'_>,
            _request: &mut crate::middleware::context::ModelRequestDraft,
        ) -> crate::Result<crate::middleware::HookOutcome> {
            assert!(
                !self.template,
                "template failure must occur during initialization"
            );
            Err(self.error())
        }
    }

    impl crate::AgentHostContext for PreparationFailure {
        fn render_prompt<'a>(
            &'a self,
            _context: crate::HostContext<'a>,
            prompt: &'a str,
            _submission: u64,
            _prompt_index: usize,
        ) -> crate::HostPromptFuture<'a> {
            Box::pin(async move {
                if self.template {
                    Err(self.error())
                } else {
                    Ok(crate::HostPromptRender {
                        prompt: prompt.into(),
                        tool_ids: Vec::new(),
                    })
                }
            })
        }
    }

    struct NeverDispatchedProvider;

    impl LlmStream for NeverDispatchedProvider {
        fn name(&self) -> &'static str {
            "never_dispatched"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            panic!("request preparation must fail before dispatch")
        }
    }

    #[tokio::test]
    async fn preparation_failure_publishes_durable_failed_summary_without_dispatch() {
        for (template, authentication, prior_outcome) in [
            (false, false, None),
            (false, true, None),
            (true, false, None),
            (true, false, Some(SessionJournalEvent::Finished)),
            (
                true,
                false,
                Some(SessionJournalEvent::Failed {
                    message: "prior failure".to_owned(),
                    authentication_failure: None,
                }),
            ),
            (
                true,
                false,
                Some(SessionJournalEvent::Suspended {
                    suspension: crate::Suspension::Interrupted { work: Vec::new() },
                }),
            ),
        ] {
            assert_preparation_failure(template, authentication, prior_outcome).await;
        }
    }

    async fn assert_preparation_failure(
        template: bool,
        authentication: bool,
        prior_outcome: Option<SessionJournalEvent>,
    ) {
        let agent = AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
            Arc::new(NeverDispatchedProvider),
        )
        .with_middleware(PreparationFailure {
            template,
            authentication,
        })
        .with_host_context(Arc::new(PreparationFailure {
            template,
            authentication,
        }))
        .build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.start = crate::SessionStart::Fresh {
            messages: vec![crate::SessionStartMessage::user("durable prompt").unwrap()],
        };
        let created = sessions.create(request).await.unwrap().record;
        let id = created.id.clone();
        let prior_failures = usize::from(matches!(
            &prior_outcome,
            Some(SessionJournalEvent::Failed { .. })
        ));
        if let Some(event) = prior_outcome {
            sessions
                .store
                .append_journal(&id, created.execution_token().unwrap(), vec![event])
                .await
                .unwrap();
        }
        let mut watch = sessions.watch_sessions();
        let prompt = if template {
            r#"@resource(test, "failure")"#
        } else {
            "durable prompt"
        };
        let mut run = sessions.prompt(&agent, id.clone(), prompt).stream();
        let mut errors = 0;
        while let Some(event) = run.next().await {
            if let Err(error) = event {
                errors += 1;
                assert!(error.to_string().contains("preparation failed"));
                assert_eq!(error.authentication_failure().is_some(), authentication);
                let summary = sessions.load(&id).await.unwrap().record.summary();
                assert!(
                    matches!(summary.status, SessionStatus::Failed { message, .. } if message.contains("preparation failed"))
                );
            }
        }
        assert_eq!(errors, 1);
        let record = sessions.load(&id).await.unwrap().record;
        assert_eq!(record.snapshot().prompts[0], "durable prompt");
        assert_eq!(
            record
                .journal
                .iter()
                .filter(|event| matches!(event, SessionJournalEvent::Failed { .. }))
                .count(),
            prior_failures + 1
        );
        assert!(
            !record
                .journal
                .iter()
                .any(|event| matches!(event, SessionJournalEvent::ProviderRequestRecorded { .. }))
        );
        loop {
            let update = tokio::time::timeout(Duration::from_secs(1), watch.next())
                .await
                .unwrap()
                .unwrap();
            if let SessionListEvent::Upsert(summary) = update.event
                && let SessionStatus::Failed {
                    message,
                    authentication_failure,
                } = &summary.status
                && message.contains("preparation failed")
            {
                assert_eq!(authentication_failure.is_some(), authentication);
                assert!(!summary.progress.running);
                assert_eq!(summary.progress.phase, crate::SessionProgressPhase::Failed);
                break;
            }
        }
    }

    #[tokio::test]
    async fn prompt_run_publishes_live_session_status_upserts() {
        let provider = Arc::new(TextOnlyProvider);
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, provider).build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap();
        let mut watch = sessions.watch_sessions();

        let mut run = sessions
            .prompt(&agent, session.record.id.clone(), "hello")
            .stream();
        while let Some(event) = run.next().await {
            event.unwrap();
        }

        let mut saw_streaming = false;
        let mut saw_finished = false;
        loop {
            let update = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
                .await
                .unwrap()
                .expect("run should publish session summaries");
            let SessionListEvent::Upsert(summary) = update.event else {
                continue;
            };
            saw_streaming |= matches!(summary.status, SessionStatus::StreamingProvider);
            saw_finished |= matches!(summary.status, SessionStatus::Finished);
            if saw_streaming && saw_finished {
                break;
            }
        }

        assert!(saw_streaming, "expected streaming summary update");
        assert!(saw_finished, "expected finished summary update");
    }

    #[tokio::test]
    async fn completion_barrier_queues_simultaneous_submission_and_timeout_fails_open() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let id = sessions
            .create(create_request(
                Some(test_inference_settings()),
                Some(crate::agent::session::opaque_test_configuration("test", vec![
                    "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: test }\nspec: {}\n"
                        .to_owned(),
                ])),
                BTreeMap::new(),
            ))
            .await
            .unwrap()
            .record
            .id;
        let handler = Arc::new(BlockingCompletionHandler {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
        });
        sessions.set_completion_admission_handler(handler.clone());

        let first = sessions
            .submit_prompt(id.clone(), "first", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        handler.entered.notified().await;
        let second = sessions
            .submit_prompt(id.clone(), "second", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        assert!(matches!(
            second.disposition,
            AgentSessionRunDisposition::Queued { position: 1 }
        ));

        tokio::time::timeout(Duration::from_secs(3), first.handle.wait())
            .await
            .expect("completion timeout must fail open")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), second.handle.wait())
            .await
            .expect("queued run must drain after timeout")
            .unwrap();
        assert_eq!(provider.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn watch_and_list_project_running_barrier_claimed_pause_and_release() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        let id = completed_session(&sessions).await;
        let mut watch = sessions.watch_sessions();
        let (lease_tx, lease_rx) = tokio::sync::oneshot::channel();
        let reason = SessionCoordinationReason::new(
            "context-compaction",
            Some("compact-and-continue".to_owned()),
            Some("run-completed".to_owned()),
        )
        .unwrap();
        let handler = Arc::new(ControlledClaimHandler {
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            lease_tx: StdMutex::new(Some(lease_tx)),
            reason: reason.clone(),
        });
        sessions.set_completion_admission_handler(handler.clone());

        let run = sessions
            .submit_prompt(id.clone(), "claim", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let running = next_coordination(&mut watch, SessionCoordinationPhase::Running).await;
        handler.entered.notified().await;
        let barrier =
            next_coordination(&mut watch, SessionCoordinationPhase::CompletionBarrier).await;
        assert_eq!(
            barrier
                .coordination
                .reason
                .as_ref()
                .map(|reason| reason.code()),
            Some("post-run-automation")
        );

        handler.release.notify_one();
        let lease = lease_rx.await.unwrap();
        let paused = next_coordination(&mut watch, SessionCoordinationPhase::Paused).await;
        assert_eq!(paused.coordination.reason.as_ref(), Some(&reason));
        assert_eq!(paused.revision, barrier.revision);
        assert!(paused.revision >= running.revision);
        assert_eq!(
            sessions
                .list()
                .await
                .unwrap()
                .into_iter()
                .find(|summary| summary.id == id)
                .unwrap()
                .coordination,
            paused.coordination
        );

        sessions.clear_completion_admission_handler();
        let queued = sessions
            .submit_prompt(id.clone(), "after pause", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let queued_pause = next_coordination_matching(&mut watch, |coordination| {
            coordination.phase == SessionCoordinationPhase::Paused && coordination.queued_count == 1
        })
        .await;
        assert_eq!(queued_pause.coordination.queued_count, 1);
        sessions.release_pause(&lease).await.unwrap();
        let resumed = next_coordination(&mut watch, SessionCoordinationPhase::Running).await;
        assert_eq!(resumed.coordination.queued_count, 0);
        queued.handle.wait().await.unwrap();
        let ready = next_coordination(&mut watch, SessionCoordinationPhase::Ready).await;
        assert_eq!(ready.coordination, SessionCoordinationState::default());
        run.handle.wait().await.unwrap();
    }

    #[tokio::test]
    async fn list_registration_seam_converges_with_pause_publication() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        let id = completed_session(&sessions).await;
        let mut watch = sessions.watch_sessions();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *sessions
            .list_registration_barrier
            .lock()
            .expect("list registration barrier lock") =
            Some((Arc::clone(&entered), Arc::clone(&release)));

        let listing = {
            let sessions = sessions.clone();
            tokio::spawn(async move { sessions.list().await.unwrap() })
        };
        entered.notified().await;
        let reason = pause_reason();
        let lease = sessions
            .acquire_pause_reject_busy(&id, reason.clone())
            .await
            .unwrap();
        *sessions
            .list_registration_barrier
            .lock()
            .expect("list registration barrier lock") = None;
        release.notify_one();

        let snapshot = listing
            .await
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == id)
            .unwrap();
        let live = next_coordination(&mut watch, SessionCoordinationPhase::Paused).await;
        assert_eq!(snapshot, live);
        assert_eq!(snapshot.coordination.reason.as_ref(), Some(&reason));

        let reconciled = sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == id)
            .unwrap();
        assert_eq!(reconciled, snapshot);
        sessions.release_pause(&lease).await.unwrap();
    }

    #[tokio::test]
    async fn pause_queue_continuation_and_restart_projection_are_process_local() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        let id = completed_session(&sessions).await;
        let mut watch = sessions.watch_sessions();
        let reason = pause_reason();
        let lease = sessions
            .acquire_pause_reject_busy(&id, reason.clone())
            .await
            .unwrap();
        let paused = next_coordination(&mut watch, SessionCoordinationPhase::Paused).await;
        assert_eq!(paused.coordination.reason.as_ref(), Some(&reason));

        let first = sessions
            .submit_prompt(id.clone(), "queued one", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let one = next_coordination(&mut watch, SessionCoordinationPhase::Paused).await;
        assert_eq!(one.coordination.queued_count, 1);
        let second = sessions
            .submit_prompt(id.clone(), "queued two", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        let two = next_coordination(&mut watch, SessionCoordinationPhase::Paused).await;
        assert_eq!(two.coordination.queued_count, 2);

        let mut mutation = SessionMaskMutation::new(None);
        mutation.add.push(crate::SessionMaskAddition::new(
            "coordination-test",
            crate::ThroughCursor::new(lease.source_cursor()),
            crate::ReplacementUserContent::new("summary"),
        ));
        let mutation = sessions
            .apply_paused_mask_mutation(&lease, mutation)
            .await
            .unwrap();
        let continuation = sessions
            .arm_paused_continuation(&lease, mutation.cursor, AgentRunLimits::default())
            .await
            .unwrap();
        let armed = next_coordination_matching(&mut watch, |coordination| {
            coordination.phase == SessionCoordinationPhase::Paused
                && coordination.continuation_armed
        })
        .await;
        assert_eq!(armed.coordination.queued_count, 2);
        assert!(armed.coordination.continuation_armed);

        let restarted = SessionCoordinator::new(sessions.store.clone());
        let restarted_summary = restarted
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == id)
            .unwrap();
        assert_eq!(
            restarted_summary.coordination,
            SessionCoordinationState::default()
        );

        sessions.release_pause(&lease).await.unwrap();
        let continuation_running = next_coordination_matching(&mut watch, |coordination| {
            coordination.phase == SessionCoordinationPhase::Running
                && coordination.queued_count == 2
        })
        .await;
        assert!(!continuation_running.coordination.continuation_armed);
        continuation.handle.wait().await.unwrap();
        let coalesced_running = next_coordination_matching(&mut watch, |coordination| {
            coordination.phase == SessionCoordinationPhase::Running
                && coordination.queued_count == 0
        })
        .await;
        assert_eq!(coalesced_running.coordination.queued_count, 0);
        first.handle.wait().await.unwrap();
        second.handle.wait().await.unwrap();
        let ready = sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| summary.id == id)
            .unwrap();
        assert_eq!(ready.coordination, SessionCoordinationState::default());
    }

    #[tokio::test]
    async fn interrupt_delete_wipe_and_shutdown_remove_stale_coordination() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;

        let interrupted_id = completed_session(&sessions).await;
        let interrupted_pause = sessions
            .acquire_pause_reject_busy(&interrupted_id, pause_reason())
            .await
            .unwrap();
        let mut interrupted_watch = sessions.watch_sessions();
        sessions.interrupt(&interrupted_id).await.unwrap();
        let interrupted =
            next_coordination(&mut interrupted_watch, SessionCoordinationPhase::Ready).await;
        assert_eq!(
            interrupted.coordination,
            SessionCoordinationState::default()
        );
        assert!(matches!(
            sessions.release_pause(&interrupted_pause).await,
            Err(SessionPauseError::OwnershipLost { .. })
        ));

        let deleted_id = completed_session(&sessions).await;
        let _deleted_pause = sessions
            .acquire_pause_reject_busy(&deleted_id, pause_reason())
            .await
            .unwrap();
        let mut deleted_watch = sessions.watch_sessions();
        sessions.delete(&deleted_id).await.unwrap();
        loop {
            if matches!(deleted_watch.next().await.unwrap().event, SessionListEvent::Deleted(ref id) if id == &deleted_id)
            {
                break;
            }
        }

        let wiped_id = completed_session(&sessions).await;
        let _wiped_pause = sessions
            .acquire_pause_reject_busy(&wiped_id, pause_reason())
            .await
            .unwrap();
        let mut wiped_watch = sessions.watch_sessions();
        sessions.wipe_sessions().await.unwrap();
        loop {
            if matches!(
                wiped_watch.next().await.unwrap().event,
                SessionListEvent::Cleared
            ) {
                break;
            }
        }

        let shutdown_sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        let shutdown_id = completed_session(&shutdown_sessions).await;
        let _shutdown_pause = shutdown_sessions
            .acquire_pause_reject_busy(&shutdown_id, pause_reason())
            .await
            .unwrap();
        let mut shutdown_watch = shutdown_sessions.watch_sessions();
        shutdown_sessions.shutdown().await;
        let shutdown =
            next_coordination(&mut shutdown_watch, SessionCoordinationPhase::Ready).await;
        assert_eq!(shutdown.coordination, SessionCoordinationState::default());
    }

    #[tokio::test]
    async fn slow_completion_snapshot_load_is_bounded_before_pause_claim() {
        let inner = MemorySessionStore::new();
        let id = SessionId::new("slow-completion-load");
        inner
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        let entered = Arc::new(Notify::new());
        let store = SlowLoadStore {
            inner,
            delay_next_load: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            entered: Arc::clone(&entered),
            configuration_barrier: None,
        };
        let sessions = SessionCoordinator::new(store);
        sessions.set_completion_admission_handler(Arc::new(FailingCompletionHandler));
        let started = std::time::Instant::now();
        let inference_identity = SessionRunInferenceIdentity {
            provider_key: "test".to_owned(),
            model_id: "test-model".to_owned(),
            execution_token: sessions
                .store
                .inner
                .load(&id)
                .await
                .unwrap()
                .execution_token()
                .unwrap(),
        };
        let admission = sessions.run_completion_admission(&id, 1, &inference_identity);
        let (_, result) = tokio::join!(entered.notified(), admission);

        assert!(result.is_none());
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn completion_finalization_failure_releases_claimed_pause() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        sessions.set_completion_admission_handler(Arc::new(FinalizingCompletionHandler(
            StdMutex::new(Some(TestFinalization::Fail)),
        )));
        let id = completed_session(&sessions).await;

        assert!(sessions.active_state(&id).await.is_none());
        let pause = sessions
            .acquire_pause_reject_busy(&id, pause_reason())
            .await
            .unwrap();
        sessions.release_pause(&pause).await.unwrap();
    }

    #[tokio::test]
    async fn delayed_post_claim_finalization_keeps_queued_work_blocked_until_failure_resolves() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let id = completed_session(&sessions).await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        sessions.set_completion_admission_handler(Arc::new(FinalizingCompletionHandler(
            StdMutex::new(Some(TestFinalization::Delay {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            })),
        )));

        let first = sessions
            .submit_prompt(id.clone(), "claim", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        entered.notified().await;
        let queued = sessions
            .submit_prompt(id.clone(), "queued", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap();
        assert!(matches!(
            queued.disposition,
            AgentSessionRunDisposition::Queued { .. }
        ));
        assert_eq!(provider.requests.lock().await.len(), 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), queued.handle.wait())
                .await
                .is_err()
        );
        release.notify_one();
        first.handle.wait().await.unwrap();
        // Submit another prompt because the timed-out wait consumed the queued handle.
        sessions
            .submit_prompt(id, "after", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .unwrap();
        assert!(provider.requests.lock().await.len() >= 3);
    }

    #[tokio::test]
    async fn worker_construction_panic_fails_open_and_releases_claimed_pause() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        sessions.set_completion_admission_handler(Arc::new(FinalizingCompletionHandler(
            StdMutex::new(Some(TestFinalization::Panic)),
        )));
        let id = completed_session(&sessions).await;

        assert!(sessions.active_state(&id).await.is_none());
        let pause = sessions
            .acquire_pause_reject_busy(&id, pause_reason())
            .await
            .unwrap();
        sessions.release_pause(&pause).await.unwrap();
    }

    #[tokio::test]
    async fn completion_snapshot_uses_latest_turn_usage_and_effective_inference_identity() {
        let sessions = runtime_sessions(Arc::new(UsageProvider)).await;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sessions.set_completion_admission_handler(Arc::new(SnapshotCompletionHandler(
            StdMutex::new(Some(sender)),
        )));
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                Some(crate::agent::session::opaque_test_configuration("test", vec![
                    "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: test }\nspec: {}\n"
                        .to_owned(),
                ])),
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let submission = sessions
            .submit_prompt(
                session.record.id.clone(),
                "measure",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        submission.handle.wait().await.unwrap();
        let snapshot = receiver.await.unwrap();

        assert_eq!(snapshot.context_tokens, Some(130));
        assert_eq!(snapshot.incarnation, session.record.metadata.incarnation);
        assert!(snapshot.revision > session.record.revision);
        assert_eq!(snapshot.provider_key, "test");
        assert_eq!(snapshot.model_id, "test-model");
        assert_eq!(snapshot.session_id, session.record.id);
        assert!(snapshot.run_id.contains(snapshot.session_id.as_str()));
        assert!(snapshot.terminal_cursor.get() > 0);
    }

    #[tokio::test]
    async fn external_inference_change_invalidates_active_run_without_completion_or_resend() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(Arc::clone(&provider)).await;
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        sessions.set_completion_admission_handler(Arc::new(SnapshotCompletionHandler(
            StdMutex::new(Some(sender)),
        )));
        let session = sessions
            .create(create_request(
                Some(test_inference_settings()),
                Some(crate::agent::session::opaque_test_configuration("test", vec![
                    "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: test }\nspec: {}\n"
                        .to_owned(),
                ])),
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        let submission = sessions
            .submit_prompt(
                session.record.id.clone(),
                "active",
                RunConflictPolicy::QueueIfActive,
            )
            .await
            .unwrap();
        provider.wait_for_request_count(1).await;
        let mut changed = test_inference_settings();
        changed.provider.key = "changed-provider".to_owned();
        changed.provider.model_id = "changed-model".to_owned();
        sessions
            .set_inference_settings(&session.record.id, Some(changed))
            .await
            .unwrap();
        provider.release_active();
        let error = tokio::time::timeout(Duration::from_secs(10), submission.handle.wait())
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("stale session execution token"),
            "{error:?}"
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(provider.requests.lock().await.len(), 1);
        let record = sessions.load(&session.record.id).await.unwrap().record;
        assert_eq!(
            record.inference_settings.as_ref().unwrap().provider.key,
            "changed-provider"
        );
        assert!(!record.journal.iter().any(|event| matches!(
            event,
            SessionJournalEvent::TurnFinished { .. } | SessionJournalEvent::Finished
        )));
        assert!(!record.timeline_events().any(|event| {
            event
                .channel
                .as_ref()
                .is_some_and(|channel| channel.as_str() == CHANNEL_ASSISTANT_TEXT)
                && matches!(
                    event.content,
                    TimelineContent::TextDelta { .. } | TimelineContent::TextSnapshot { .. }
                )
        }));
    }

    #[tokio::test]
    async fn completion_handler_error_fails_open_without_stranding_the_session() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider)).await;
        sessions.set_completion_admission_handler(Arc::new(FailingCompletionHandler));
        let id = completed_session(&sessions).await;

        assert!(sessions.active_state(&id).await.is_none());
        sessions
            .submit_prompt(id, "after failure", RunConflictPolicy::QueueIfActive)
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .unwrap();
    }
}
