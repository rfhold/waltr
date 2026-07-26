//! Durable agent session coordinator.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, LlmError, PendingTimelineEvent, Result,
    TimelineContent, TimelineItemId, TimelineItemText, TimelineQuery, mcp::McpClientRuntime,
};
use futures_util::StreamExt as _;
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use super::watch::{SessionListBroadcaster, SessionListEvent};
use super::{
    AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntime, AgentSessionRuntimeFactory, ExternalResponse, ResumeInput,
    RunConflictPolicy, RunOutcomeSender, RunResult, SessionId, SessionListSubscription,
    SessionRuntimeSettings, SessionStatus, SessionStore, SessionSummary,
    SessionTimelineBroadcaster, SessionTimelineSubscription,
};
use crate::agent::{AgentRunBuilder, AgentRuntime, AgentSession};
use crate::resource::{SessionTemplateOption, SessionTemplateSearchRequest};

/// Durable session coordinator bound to a store.
#[derive(Debug, Clone)]
pub struct SessionCoordinator<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    store: S,
    sessions: SessionListBroadcaster,
    timeline: SessionTimelineBroadcaster,
    runtime_factory: Option<AgentSessionRuntimeFactory>,
    mcp_sessions: Arc<Mutex<BTreeMap<SessionId, Arc<McpClientRuntime>>>>,
    active_runs: Arc<Mutex<BTreeMap<SessionId, ActiveSessionRun>>>,
    next_run_id: Arc<AtomicU64>,
}

#[derive(Debug)]
struct ActiveSessionRun {
    id: u64,
    token: CancellationToken,
    queued: VecDeque<QueuedSessionRun>,
}

#[derive(Debug)]
struct QueuedSessionRun {
    id: u64,
    input: AgentSessionRunInput,
    outcome_tx: RunOutcomeSender,
}

struct SessionRunStart {
    id: u64,
    token: CancellationToken,
    runtime: Option<AgentRuntime>,
    input: AgentSessionRunInput,
    outcome_txs: Vec<(u64, RunOutcomeSender)>,
}

impl<S> SessionCoordinator<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a durable session coordinator.
    pub fn new(store: S) -> Self {
        Self {
            store,
            sessions: SessionListBroadcaster::new(),
            timeline: SessionTimelineBroadcaster::new(),
            runtime_factory: None,
            mcp_sessions: Arc::new(Mutex::new(BTreeMap::new())),
            active_runs: Arc::new(Mutex::new(BTreeMap::new())),
            next_run_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Configure the runtime factory used for session run submissions.
    #[must_use]
    pub fn with_runtime_factory(mut self, factory: AgentSessionRuntimeFactory) -> Self {
        self.runtime_factory = Some(factory);
        self
    }

    /// Create a new durable session with a generated id.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "create", db_system = "session_store", session_id)
    )]
    pub async fn create(&self) -> Result<AgentSession> {
        self.create_with_metadata_values(BTreeMap::new()).await
    }

    /// Create a new durable session with initial host-defined metadata values.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    #[tracing::instrument(
        name = "session.store",
        skip(self, metadata_values),
        fields(
            operation = "create_with_metadata_values",
            db_system = "session_store",
            session_id
        )
    )]
    pub async fn create_with_metadata_values(
        &self,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        self.create_with_id_and_metadata_values(SessionId::generate(), metadata_values)
            .await
    }

    /// Create a new durable session with runtime settings and generated id.
    pub async fn create_with_runtime_settings(
        &self,
        runtime_settings: Option<SessionRuntimeSettings>,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        self.create_with_id_and_runtime_settings(
            SessionId::generate(),
            runtime_settings,
            metadata_values,
        )
        .await
    }

    /// Create a new durable session with caller-provided id and runtime settings.
    pub async fn create_with_id_and_runtime_settings(
        &self,
        id: SessionId,
        runtime_settings: Option<SessionRuntimeSettings>,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        tracing::debug!(
            metadata_key_count = metadata_values.len(),
            runtime_configured = runtime_settings.is_some(),
            "creating session record"
        );
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime
            .create_session_with_runtime_settings(id, runtime_settings, metadata_values)
            .await?;
        tracing::Span::current().record("session_id", record.id.as_str());
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Create a new durable session with a caller-provided id and metadata values.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    #[tracing::instrument(
        name = "session.store",
        skip(self, metadata_values),
        fields(
            operation = "create_with_id_and_metadata_values",
            db_system = "session_store",
            session_id = %id.as_str()
        )
    )]
    pub async fn create_with_id_and_metadata_values(
        &self,
        id: SessionId,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        tracing::debug!(
            metadata_key_count = metadata_values.len(),
            "creating session record"
        );
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime
            .create_session_with_metadata_values(id, metadata_values)
            .await?;
        tracing::Span::current().record("session_id", record.id.as_str());
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Delete a durable session if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store cannot complete the delete operation.
    #[tracing::instrument(
        name = "session.store",
        skip(self),
        fields(operation = "delete", db_system = "session_store", session_id = %id.as_str())
    )]
    pub async fn delete(&self, id: &SessionId) -> Result<()> {
        AgentSessionRuntime::new(&self.store)
            .delete_session(id)
            .await?;
        if let Some(runtime) = self.mcp_sessions.lock().await.remove(id) {
            runtime.shutdown().await;
        }
        self.sessions.publish(SessionListEvent::Deleted(id.clone()));
        Ok(())
    }

    /// Watch newly created or updated sessions.
    #[must_use]
    pub fn watch_sessions(&self) -> SessionListSubscription {
        self.sessions.subscribe()
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
        AgentSessionRuntime::new(&self.store).list_sessions().await
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
        self.interrupt_active_run(id).await;
        let record = AgentSessionRuntime::new(&self.store)
            .with_session_broadcaster(self.sessions.clone())
            .with_timeline_broadcaster(self.timeline.clone())
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
        let record = self.load(id).await?;
        let assigned = self
            .store
            .append_timeline(id, record.revision(), events)
            .await
            .map_err(session_store_error)?;
        self.timeline.publish_all(assigned.clone());
        let record = self.load(id).await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.record.summary()));
        Ok(assigned)
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
        let key = key.into();
        tracing::debug!(metadata_key = %key, "setting session metadata value");
        let record = AgentSessionRuntime::new(&self.store)
            .set_metadata_value(id, key, value)
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
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
        let key = key.into();
        tracing::debug!(metadata_key = %key, "removing session metadata value");
        let record = AgentSessionRuntime::new(&self.store)
            .remove_metadata_value(id, key)
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Set runtime settings and publish an updated session summary.
    pub async fn set_runtime_settings(
        &self,
        id: &SessionId,
        runtime_settings: Option<SessionRuntimeSettings>,
    ) -> Result<AgentSession> {
        let record = AgentSessionRuntime::new(&self.store)
            .set_runtime_settings(id, runtime_settings)
            .await?;
        if let Some(runtime) = self.mcp_sessions.lock().await.remove(id) {
            runtime.shutdown().await;
        }
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Submit a prompt run.
    pub async fn submit_prompt(
        &self,
        session_id: SessionId,
        prompt: impl Into<String>,
        policy: RunConflictPolicy,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit(
            session_id,
            AgentSessionRunInput::Prompt(prompt.into()),
            policy,
        )
        .await
    }

    /// Submit an external-response resume run.
    pub async fn resume_external_response(
        &self,
        session_id: SessionId,
        request_id: super::ExternalRequestId,
        response: Value,
        policy: RunConflictPolicy,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.submit(
            session_id,
            AgentSessionRunInput::ExternalResponse {
                request_id,
                response: ExternalResponse::new(response),
            },
            policy,
        )
        .await
    }

    /// Roll back a session to the end of a visible provider turn.
    pub async fn rollback_to_turn(&self, session_id: SessionId, turn: u64) -> Result<AgentSession> {
        self.interrupt_active_run(&session_id).await;
        let output = AgentSessionRuntime::new(&self.store)
            .with_session_broadcaster(self.sessions.clone())
            .rollback_to_turn(&session_id, turn)
            .await?;
        self.timeline
            .publish_truncation(session_id.clone(), output.after_cursor);
        Ok(AgentSession::new(output.record))
    }

    /// Roll back a session to a visible timeline cursor.
    pub async fn rollback_to_cursor(
        &self,
        session_id: SessionId,
        cursor: crate::timeline::TimelineCursor,
    ) -> Result<AgentSession> {
        self.interrupt_active_run(&session_id).await;
        let output = AgentSessionRuntime::new(&self.store)
            .with_session_broadcaster(self.sessions.clone())
            .rollback_to_cursor(&session_id, cursor)
            .await?;
        self.timeline
            .publish_truncation(session_id.clone(), output.after_cursor);
        Ok(AgentSession::new(output.record))
    }

    /// Roll back a session to immediately before a visible timeline cursor.
    pub async fn rollback_before_cursor(
        &self,
        session_id: SessionId,
        cursor: crate::timeline::TimelineCursor,
    ) -> Result<AgentSession> {
        self.interrupt_active_run(&session_id).await;
        let output = AgentSessionRuntime::new(&self.store)
            .with_session_broadcaster(self.sessions.clone())
            .rollback_before_cursor(&session_id, cursor)
            .await?;
        self.timeline
            .publish_truncation(session_id.clone(), output.after_cursor);
        Ok(AgentSession::new(output.record))
    }

    /// Roll back to the end of a turn and continue from that durable checkpoint.
    pub async fn continue_from_turn(
        &self,
        session_id: SessionId,
        turn: u64,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.rollback_to_turn(session_id.clone(), turn).await?;
        self.submit(
            session_id,
            AgentSessionRunInput::Continue,
            RunConflictPolicy::ReplaceActive,
        )
        .await
    }

    /// Roll back to a timeline cursor and continue from that durable checkpoint.
    pub async fn continue_from_cursor(
        &self,
        session_id: SessionId,
        cursor: crate::timeline::TimelineCursor,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.rollback_to_cursor(session_id.clone(), cursor).await?;
        self.submit(
            session_id,
            AgentSessionRunInput::Continue,
            RunConflictPolicy::ReplaceActive,
        )
        .await
    }

    /// Submit a session run or resume input.
    pub async fn submit(
        &self,
        session_id: SessionId,
        input: AgentSessionRunInput,
        policy: RunConflictPolicy,
    ) -> std::result::Result<AgentSessionRunSubmission, AgentSessionRunError> {
        let id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let handle = AgentSessionRunHandle::new(session_id.clone(), id, outcome_rx);
        let mut start = None;
        let disposition;

        {
            let mut active = self.active_runs.lock().await;
            if let Some(current) = active.get_mut(&session_id) {
                match policy {
                    RunConflictPolicy::RejectIfActive => {
                        return Err(AgentSessionRunError::AlreadyRunning { session_id });
                    }
                    RunConflictPolicy::QueueIfActive => {
                        current.queued.push_back(QueuedSessionRun {
                            id,
                            input,
                            outcome_tx,
                        });
                        disposition = AgentSessionRunDisposition::Queued {
                            position: current.queued.len(),
                        };
                    }
                    RunConflictPolicy::ReplaceActive => {
                        let token = CancellationToken::new();
                        let previous = active.insert(
                            session_id.clone(),
                            ActiveSessionRun {
                                id,
                                token: token.clone(),
                                queued: VecDeque::new(),
                            },
                        );
                        if let Some(previous) = previous {
                            previous.token.cancel();
                            cancel_queued(
                                session_id.clone(),
                                previous.queued,
                                "replaced by a new run",
                            );
                        }
                        start = Some(SessionRunStart {
                            id,
                            token,
                            runtime: None,
                            input,
                            outcome_txs: vec![(id, outcome_tx)],
                        });
                        disposition = AgentSessionRunDisposition::ReplacedActive;
                    }
                }
            } else {
                let token = CancellationToken::new();
                active.insert(
                    session_id.clone(),
                    ActiveSessionRun {
                        id,
                        token: token.clone(),
                        queued: VecDeque::new(),
                    },
                );
                start = Some(SessionRunStart {
                    id,
                    token,
                    runtime: None,
                    input,
                    outcome_txs: vec![(id, outcome_tx)],
                });
                disposition = AgentSessionRunDisposition::Started;
            }
        }

        if let Some(mut start) = start {
            match self.resolve_runtime(&session_id).await {
                Ok(runtime) => start.runtime = Some(runtime),
                Err(error) => {
                    self.active_runs.lock().await.remove(&session_id);
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
            .get(session_id)
            .map(|active| AgentSessionActiveRunState {
                run_id: active.id,
                queued_count: active.queued.len(),
            })
    }

    async fn interrupt_active_run(&self, id: &SessionId) {
        let active = self.active_runs.lock().await.remove(id);
        if let Some(active) = active {
            active.token.cancel();
            tracing::info!(session_id = %id.as_str(), run_id = active.id, "active session run cancellation requested");
            cancel_queued(id.clone(), active.queued, "session interrupted");
        } else {
            tracing::debug!(session_id = %id.as_str(), "no active session run to cancel");
        }
    }

    fn spawn_run(&self, session_id: SessionId, start: SessionRunStart) {
        let coordinator = self.clone();
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
                let outcome = drain_run(
                    coordinator.clone(),
                    start.runtime,
                    session_id.clone(),
                    start.input,
                    start.id,
                    start.token,
                )
                .await;
                let terminal_state = terminal_state(&outcome);
                tracing::Span::current().record("terminal_state", terminal_state);
                tracing::info!(session_id = %session_id.as_str(), run_id, terminal_state, "session run completed");
                send_run_outcome(start.outcome_txs, outcome);
                if let Some(next) = coordinator.finish_active_run(&session_id, run_id).await {
                    coordinator.spawn_run(session_id, next);
                }
            }
            .instrument(span),
        );
    }

    async fn finish_active_run(
        &self,
        session_id: &SessionId,
        run_id: u64,
    ) -> Option<SessionRunStart> {
        let mut active = self.active_runs.lock().await;
        let current = active.get_mut(session_id)?;
        if current.id != run_id {
            return None;
        }
        let Some(queued) = current.queued.pop_front() else {
            active.remove(session_id);
            return None;
        };
        let token = CancellationToken::new();
        let id = queued.id;
        let mut input = queued.input;
        let mut outcome_txs = vec![(queued.id, queued.outcome_tx)];
        if let AgentSessionRunInput::Prompt(first) = input {
            let mut prompts = vec![first];
            while let Some(next) = current.queued.front() {
                if !matches!(next.input, AgentSessionRunInput::Prompt(_)) {
                    break;
                }
                let next = current.queued.pop_front().expect("queued prompt exists");
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
        current.id = id;
        current.token = token.clone();
        Some(SessionRunStart {
            id,
            token,
            runtime: None,
            input,
            outcome_txs,
        })
    }

    /// Start a durable session turn from a user prompt.
    pub fn prompt(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<S> {
        runtime
            .prompt(prompt)
            .with_session_store(self.store.clone(), session_id)
            .with_session_broadcaster(self.sessions.clone())
            .with_timeline_broadcaster(self.timeline.clone())
    }

    /// Resume an existing durable session with explicit input.
    pub fn resume(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        input: ResumeInput,
    ) -> AgentRunBuilder<S> {
        AgentRunBuilder::resume(runtime, session_id, self.store.clone(), input)
            .with_session_broadcaster(self.sessions.clone())
            .with_timeline_broadcaster(self.timeline.clone())
    }

    /// Return the session store used by this coordinator.
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Search prompt-template resources available to a durable session.
    pub async fn search_templates(
        &self,
        session_id: &SessionId,
        request: SessionTemplateSearchRequest,
    ) -> std::result::Result<Vec<SessionTemplateOption>, AgentSessionRunError> {
        let root = self
            .runtime_factory
            .as_ref()
            .ok_or_else(|| {
                super::AgentSessionRuntimeError::Internal(
                    "session runtime factory is not configured".to_owned(),
                )
            })?
            .root
            .clone();
        let (runtime, middleware_state) =
            self.resolve_runtime_for_template_search(session_id).await?;
        Ok(crate::resource::search_session_templates_with_state(
            &root,
            &runtime,
            &request,
            Some(&middleware_state),
        )
        .await?)
    }

    async fn resolve_runtime_for_template_search(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<
        (AgentRuntime, crate::middleware::context::MiddlewareStateLog),
        AgentSessionRunError,
    > {
        let session = self.load(session_id).await?;
        let middleware_state = crate::middleware::context::MiddlewareStateLog::from_snapshot(
            &session.record.snapshot(),
        );
        let settings = session.record.runtime_settings.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::FailedPrecondition(
                "session runtime settings are required before prompting".to_owned(),
            )
        })?;
        let factory = self.runtime_factory.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::Internal(
                "session runtime factory is not configured".to_owned(),
            )
        })?;
        let mcp_runtime = self
            .ensure_mcp_runtime(session_id, settings, factory)
            .await
            .ok()
            .flatten();
        let builder = factory
            .runtime_builder_for_sessions(settings, self.clone(), mcp_runtime)
            .await?;
        Ok((builder.build(), middleware_state))
    }

    pub(crate) async fn resolve_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<AgentRuntime, AgentSessionRunError> {
        let session = self.load(session_id).await?;
        let settings = session.record.runtime_settings.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::FailedPrecondition(
                "session runtime settings are required before prompting".to_owned(),
            )
        })?;
        let factory = self.runtime_factory.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::Internal(
                "session runtime factory is not configured".to_owned(),
            )
        })?;
        let mcp_runtime = self
            .ensure_mcp_runtime(session_id, settings, factory)
            .await?;
        let builder = factory
            .runtime_builder_for_sessions(settings, self.clone(), mcp_runtime)
            .await?;
        Ok(builder.build())
    }

    async fn ensure_mcp_runtime(
        &self,
        session_id: &SessionId,
        settings: &SessionRuntimeSettings,
        factory: &AgentSessionRuntimeFactory,
    ) -> std::result::Result<Option<Arc<McpClientRuntime>>, super::AgentSessionRuntimeError> {
        if settings.mcp_servers.is_empty() {
            return Ok(None);
        }
        if let Some(runtime) = self.mcp_sessions.lock().await.get(session_id).cloned() {
            return Ok(Some(runtime));
        }
        let config = factory.mcp_config(settings).await?;
        let runtime =
            McpClientRuntime::start(config, factory.mcp_root_uri(), CancellationToken::new())
                .await
                .map_err(|error| match error.error {
                    crate::LlmError::McpAuthorization {
                        status,
                        challenge,
                        message,
                    } => {
                        let endpoint = settings
                            .mcp_servers
                            .iter()
                            .find(|server| server.key == error.server_key)
                            .map_or_else(String::new, |server| server.url.clone());
                        let mcp_authorization = Some(Box::new(crate::McpAuthorizationContext {
                            endpoint,
                            http_status: status,
                            resource_metadata_url: challenge
                                .as_ref()
                                .and_then(|value| value.resource_metadata.clone()),
                            scope: challenge.as_ref().and_then(|value| value.scope.clone()),
                            error: challenge.as_ref().and_then(|value| value.error.clone()),
                            error_description: challenge
                                .as_ref()
                                .and_then(|value| value.error_description.clone()),
                            error_uri: challenge.as_ref().and_then(|value| value.error_uri.clone()),
                            realm: challenge.as_ref().and_then(|value| value.realm.clone()),
                        }));
                        super::AgentSessionRuntimeError::AuthorizationRequired {
                            target: crate::AuthTarget::mcp_server(error.server_key),
                            message,
                            reason: crate::AuthenticationFailureReason::CredentialRequired,
                            mcp_authorization,
                        }
                    }
                    crate::LlmError::Authorization { message, .. } => {
                        super::AgentSessionRuntimeError::AuthorizationRequired {
                            target: crate::AuthTarget::mcp_server(error.server_key),
                            message,
                            reason: crate::AuthenticationFailureReason::CredentialRequired,
                            mcp_authorization: None,
                        }
                    }
                    error => super::AgentSessionRuntimeError::from(error),
                })?;
        self.mcp_sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&runtime));
        Ok(Some(runtime))
    }
}

async fn drain_run<S>(
    coordinator: SessionCoordinator<S>,
    runtime: Option<AgentRuntime>,
    session_id: SessionId,
    input: AgentSessionRunInput,
    run_id: u64,
    token: CancellationToken,
) -> RunResult
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let runtime = match runtime {
        Some(runtime) => runtime,
        None => coordinator.resolve_runtime(&session_id).await?,
    };
    let mut stream = match input {
        AgentSessionRunInput::Continue => {
            coordinator.resume(&runtime, session_id.clone(), ResumeInput::Continue)
        }
        AgentSessionRunInput::Prompt(prompt) => {
            coordinator.prompt(&runtime, session_id.clone(), prompt)
        }
        AgentSessionRunInput::Prompts(prompts) => {
            coordinator.resume(&runtime, session_id.clone(), ResumeInput::Prompts(prompts))
        }
        AgentSessionRunInput::ExternalResponse {
            request_id,
            response,
        } => coordinator.resume(
            &runtime,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id,
                response,
            },
        ),
        AgentSessionRunInput::ToolResults(results) => coordinator.resume(
            &runtime,
            session_id.clone(),
            ResumeInput::ToolResults(results),
        ),
    }
    .with_cancellation_token(token.clone())
    .stream();

    let mut assistant_text = String::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        if is_assistant_text_delta(&event)
            && let TimelineContent::TextDelta { delta } = &event.content
        {
            assistant_text.push_str(delta);
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

fn is_assistant_text_delta(event: &AgentTimelineEvent) -> bool {
    event
        .channel
        .as_ref()
        .is_some_and(|channel| channel.as_str() == CHANNEL_ASSISTANT_TEXT)
        && matches!(event.content, TimelineContent::TextDelta { .. })
}

fn cancel_queued(session_id: SessionId, queued: VecDeque<QueuedSessionRun>, reason: &'static str) {
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

fn terminal_state(outcome: &RunResult) -> &'static str {
    match outcome {
        Ok(outcome) => match &outcome.status {
            SessionStatus::Finished => "finished",
            SessionStatus::Failed { .. } => "error",
            SessionStatus::Suspended(super::Suspension::AwaitingExternalInput { .. }) => {
                "external_request"
            }
            SessionStatus::Suspended(super::Suspension::AwaitingToolResults { .. }) => {
                "awaiting_tool_results"
            }
            SessionStatus::Suspended(super::Suspension::Interrupted { .. }) => "interrupted",
            SessionStatus::Idle
            | SessionStatus::BuildingRequest
            | SessionStatus::StreamingProvider
            | SessionStatus::RetryingProvider { .. }
            | SessionStatus::EvaluatingToolCall
            | SessionStatus::RunningTool => "finished",
        },
        Err(AgentSessionRunError::Canceled { .. }) => "interrupted",
        Err(AgentSessionRunError::AlreadyRunning { .. })
        | Err(AgentSessionRunError::OutcomeDropped { .. })
        | Err(AgentSessionRunError::Runtime(_))
        | Err(AgentSessionRunError::Agent(_)) => "error",
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
    use std::sync::Arc;

    use futures_util::{StreamExt as _, stream};
    use serde_json::json;
    use tokio::sync::{Mutex, Notify};

    use super::*;
    use crate::{
        BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionReplayItem,
        CompletionRequest, LlmStream, ModelApi, ModelRef, ProviderId, SessionStatus,
        TimelineItemId,
        agent::session::{
            FileCredentialStore, MemorySessionStore, SessionListEvent,
            SessionProviderRuntimeSettings,
        },
    };

    struct TextOnlyProvider;

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
                if prompts.as_slice() == ["active"] {
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

    fn test_runtime_settings() -> SessionRuntimeSettings {
        SessionRuntimeSettings {
            provider: SessionProviderRuntimeSettings {
                key: "test".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: "test-model".to_owned(),
                model_cost: None,
                headers: Default::default(),
                oauth: None,
                reasoning_effort: None,
            },
            mcp_servers: Vec::new(),
            mcp_trace_propagation: false,
        }
    }

    fn runtime_sessions<P>(provider: Arc<P>) -> SessionCoordinator<MemorySessionStore>
    where
        P: LlmStream + 'static,
    {
        let environment = AgentSessionRuntimeFactory::new(
            std::env::temp_dir(),
            FileCredentialStore::new(std::env::temp_dir().join("waltr-agent-queue-test-auth.json")),
        )
        .with_runtime_builder(move |_| {
            Ok(AgentRuntime::builder(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                provider.clone(),
            ))
        });

        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(environment)
    }

    #[tokio::test]
    async fn metadata_update_publishes_session_upsert() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions.create().await.unwrap();
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
        }
    }

    #[tokio::test]
    async fn append_timeline_publishes_session_summary_upsert() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions.create().await.unwrap();
        let mut watch = sessions.watch_sessions();

        sessions
            .append_timeline(
                &session.record.id,
                vec![PendingTimelineEvent::assistant_text_delta(
                    session.record.id.clone(),
                    0,
                    TimelineItemId::new("assistant-1"),
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
        }
    }

    #[tokio::test]
    async fn queued_prompt_runs_are_coalesced_into_adjacent_user_messages() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let sessions = runtime_sessions(provider.clone());
        let session = sessions
            .create_with_runtime_settings(Some(test_runtime_settings()), BTreeMap::new())
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
    async fn prompt_run_publishes_live_session_status_upserts() {
        let provider = Arc::new(TextOnlyProvider);
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, provider).build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions.create().await.unwrap();
        let mut watch = sessions.watch_sessions();

        let mut run = sessions
            .prompt(&agent, session.record.id.clone(), "hello")
            .stream();
        while let Some(event) = run.next().await {
            event.unwrap();
        }

        let mut saw_streaming = false;
        let mut saw_finished = false;
        for _ in 0..8 {
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
}
