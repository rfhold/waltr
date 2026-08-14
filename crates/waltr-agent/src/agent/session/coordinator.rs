//! Durable agent session coordinator.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, LlmError, PendingTimelineEvent, Result,
    TimelineContent, TimelineItemId, TimelineItemText, TimelineQuery,
    definition::{AgentDefinitionBundle, AgentSwitchAudit},
    mcp::McpClientRuntime,
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
    AgentSessionRuntime, AgentSessionRuntimeFactory, ExternalResponse, InferenceSettings,
    ResumeInput, RunConflictPolicy, RunOutcomeSender, RunResult, SessionId,
    SessionListSubscription, SessionStatus, SessionStore, SessionSummary,
    SessionTimelineBroadcaster, SessionTimelineSubscription,
};
use crate::agent::{AgentRunBuilder, AgentRuntime, AgentSession};
use crate::resource::{SessionTemplateOption, SessionTemplateSearchRequest};

type McpPreparationGuards = Arc<StdMutex<BTreeMap<SessionId, Arc<Mutex<()>>>>>;
type SessionMutationGates = Arc<StdMutex<BTreeMap<SessionId, Arc<Mutex<()>>>>>;

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
    mcp_sessions: Arc<StdMutex<BTreeMap<SessionId, CachedMcpRuntime>>>,
    mcp_preparation_guards: McpPreparationGuards,
    active_runs: Arc<Mutex<BTreeMap<SessionId, ActiveSessionRun>>>,
    mutation_gates: SessionMutationGates,
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

#[derive(Debug, Clone)]
struct CachedMcpRuntime {
    runtime: Arc<McpClientRuntime>,
    fingerprint: super::runtime_factory::McpRuntimeFingerprint,
    credentialed_servers: BTreeSet<String>,
}

impl CachedMcpRuntime {
    fn can_reuse(&self, prepared: &super::runtime_factory::PreparedMcpRuntime) -> bool {
        self.fingerprint == prepared.fingerprint
            && self.credentialed_servers == prepared.credentialed_servers
    }
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
            mcp_sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            mcp_preparation_guards: Arc::new(StdMutex::new(BTreeMap::new())),
            active_runs: Arc::new(Mutex::new(BTreeMap::new())),
            mutation_gates: Arc::new(StdMutex::new(BTreeMap::new())),
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
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    pub async fn create_with_inference_settings(
        &self,
        inference_settings: Option<InferenceSettings>,
        agent_definition: Option<AgentDefinitionBundle>,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        self.create_with_id_and_inference_settings(
            SessionId::generate(),
            inference_settings,
            agent_definition,
            metadata_values,
        )
        .await
    }

    /// Create a new durable session with caller-provided id and runtime settings.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    pub async fn create_with_id_and_inference_settings(
        &self,
        id: SessionId,
        inference_settings: Option<InferenceSettings>,
        agent_definition: Option<AgentDefinitionBundle>,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        tracing::debug!(
            metadata_key_count = metadata_values.len(),
            inference_configured = inference_settings.is_some(),
            agent_definition_configured = agent_definition.is_some(),
            "creating session record"
        );
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime
            .create_session_with_inference_settings(
                id,
                inference_settings,
                agent_definition,
                metadata_values,
            )
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
        AgentSessionRuntime::new(&self.store)
            .delete_session(id)
            .await?;
        let runtime = self
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .remove(id);
        self.mcp_preparation_guards
            .lock()
            .expect("MCP preparation guard map lock poisoned")
            .remove(id);
        self.mutation_gates
            .lock()
            .expect("session mutation gate map lock poisoned")
            .remove(id);
        let shutdown = match runtime {
            Some(runtime) => shutdown_when_unused(runtime.runtime).await,
            None => Ok(()),
        };
        self.sessions.publish(SessionListEvent::Deleted(id.clone()));
        shutdown?;
        Ok(())
    }

    /// Delete every durable session and clear cached per-session MCP runtimes.
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
        self.store.wipe().await.map_err(session_store_error)?;
        let runtimes = std::mem::take(
            &mut *self
                .mcp_sessions
                .lock()
                .expect("MCP runtime cache lock poisoned"),
        );
        self.mcp_preparation_guards
            .lock()
            .expect("MCP preparation guard map lock poisoned")
            .clear();
        self.mutation_gates
            .lock()
            .expect("session mutation gate map lock poisoned")
            .clear();
        self.sessions.publish(SessionListEvent::Cleared);
        drop(tokio::spawn(async move {
            for (session_id, cached) in runtimes {
                if shutdown_when_unused(cached.runtime).await.is_err() {
                    tracing::warn!(
                        session_id = %session_id.as_str(),
                        "failed to shut down cached MCP runtime after session wipe"
                    );
                }
            }
        }));
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
        let gate = self.mutation_gate(id);
        let _mutation = gate.lock().await;
        let record = AgentSessionRuntime::new(&self.store)
            .set_inference_settings(id, inference_settings)
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
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
        agent_definition: Option<AgentDefinitionBundle>,
    ) -> Result<AgentSession> {
        let gate = self.mutation_gate(id);
        let _mutation = gate.lock().await;
        let record = AgentSessionRuntime::new(&self.store)
            .set_agent_definition(id, agent_definition)
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Select a catalog agent immediately for captures beginning after this mutation boundary.
    ///
    /// Multiple switches before a run boundary coalesce to the first old name and final new name.
    ///
    /// # Errors
    ///
    /// Returns an error when no runtime factory is configured, the agent is absent from the
    /// current catalog, or the session cannot be loaded or updated.
    pub async fn switch_agent(
        &self,
        id: &SessionId,
        agent_name: &str,
    ) -> std::result::Result<AgentSession, AgentSessionRunError> {
        let factory = self.runtime_factory.as_ref().ok_or_else(|| {
            super::AgentSessionRuntimeError::Internal(
                "session runtime factory is not configured".to_owned(),
            )
        })?;
        let mut bundle = factory.bundle_for_agent(agent_name)?;
        let gate = self.mutation_gate(id);
        let _mutation = gate.lock().await;
        let current = self.load(id).await?.record;
        let old_name = current
            .agent_definition
            .as_ref()
            .map_or_else(|| factory.default_agent_name(), |bundle| &bundle.agent_name)
            .to_owned();
        let pending = current
            .agent_definition
            .as_ref()
            .and_then(|bundle| bundle.pending_switch_audit.as_ref());
        let pending_was_emitted = pending.is_some_and(|audit| {
            current.timeline_events().any(|event| {
                matches!(
                    &event.content,
                    TimelineContent::Json { value }
                        if value.get("auditId").and_then(Value::as_str)
                            == Some(audit.id.as_str())
                )
            })
        });
        let first_old = pending
            .filter(|_| !pending_was_emitted)
            .map_or(old_name, |audit| audit.old_name.clone());
        if first_old != agent_name {
            bundle.pending_switch_audit = Some(AgentSwitchAudit {
                id: uuid::Uuid::now_v7().to_string(),
                old_name: first_old,
                new_name: agent_name.to_owned(),
            });
        }
        let record = AgentSessionRuntime::new(&self.store)
            .set_agent_definition(id, Some(bundle))
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
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
        self.submit(
            session_id,
            AgentSessionRunInput::Prompt(prompt.into()),
            policy,
        )
        .await
    }

    /// Submit an external-response resume run.
    ///
    /// # Errors
    ///
    /// Returns an error when runtime resolution or run admission fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or persistence fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or persistence fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or persistence fails.
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
    #[allow(clippy::significant_drop_tightening, clippy::too_many_lines)]
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
                            cancel_queued(&session_id, previous.queued, "replaced by a new run");
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
            cancel_queued(id, active.queued, "session interrupted");
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

    #[allow(clippy::significant_drop_tightening)]
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
    ///
    /// # Errors
    ///
    /// Returns an error when no runtime factory exists or runtime/template search fails.
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
        let record = self.capture_runtime_record(session_id).await?;
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
        let mcp_runtime = self
            .ensure_mcp_runtime(session_id, &record, factory)
            .await
            .ok()
            .flatten();
        let runtime = factory
            .runtime_for_session(&record, self.clone(), mcp_runtime)
            .await?;
        Ok((runtime, middleware_state))
    }

    pub(crate) async fn resolve_runtime(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<AgentRuntime, AgentSessionRunError> {
        let record = self.capture_runtime_record_for_cycle(session_id).await?;
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
        let mcp_runtime = self
            .ensure_mcp_runtime(session_id, &record, factory)
            .await?;
        Ok(factory
            .runtime_for_session(&record, self.clone(), mcp_runtime)
            .await?)
    }

    pub(crate) async fn capture_runtime_record(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<super::SessionRecord, AgentSessionRunError> {
        let gate = self.mutation_gate(session_id);
        let _capture = gate.lock().await;
        Ok(self.load(session_id).await?.record)
    }

    async fn capture_runtime_record_for_cycle(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<super::SessionRecord, AgentSessionRunError> {
        let gate = self.mutation_gate(session_id);
        let _capture = gate.lock().await;
        let mut record = self.load(session_id).await?.record;
        let Some(audit) = record
            .agent_definition
            .as_ref()
            .and_then(|bundle| bundle.pending_switch_audit.clone())
        else {
            return Ok(record);
        };
        let already_appended = record.timeline_events().any(|event| {
            matches!(
                &event.content,
                TimelineContent::Json { value }
                    if value.get("auditId").and_then(Value::as_str) == Some(audit.id.as_str())
            )
        });
        if !already_appended {
            let event = PendingTimelineEvent::new(
                session_id.clone(),
                record.snapshot().turn,
                crate::PendingTimelineItemId::New,
                crate::TimelineEventKind::ItemStarted,
                Some(crate::TimelineChannel::new("session_info")),
                TimelineContent::Json {
                    value: serde_json::json!({
                        "kind": "agent_switch",
                        "version": 1,
                        "auditId": audit.id,
                        "oldAgentName": audit.old_name,
                        "newAgentName": audit.new_name,
                    }),
                },
            );
            let assigned = self
                .store
                .append_timeline(session_id, record.revision, vec![event])
                .await
                .map_err(session_store_error)?;
            self.timeline.publish_all(assigned);
            record = self.load(session_id).await?.record;
        }
        if let Some(mut bundle) = record.agent_definition.clone() {
            bundle.pending_switch_audit = None;
            record = AgentSessionRuntime::new(&self.store)
                .set_agent_definition(session_id, Some(bundle))
                .await?;
        }
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(record)
    }

    fn mutation_gate(&self, session_id: &SessionId) -> Arc<Mutex<()>> {
        self.mutation_gates
            .lock()
            .expect("session mutation gate map lock poisoned")
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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

    async fn persist_authentication_failure(
        &self,
        session_id: &SessionId,
        message: String,
        failure: crate::AuthenticationFailure,
    ) -> Result<()> {
        AgentSessionRuntime::new(&self.store)
            .with_session_broadcaster(self.sessions.clone())
            .with_timeline_broadcaster(self.timeline.clone())
            .fail_authentication(session_id, message, failure)
            .await?;
        Ok(())
    }

    async fn ensure_mcp_runtime(
        &self,
        session_id: &SessionId,
        record: &super::SessionRecord,
        factory: &AgentSessionRuntimeFactory,
    ) -> std::result::Result<Option<Arc<McpClientRuntime>>, super::AgentSessionRuntimeError> {
        let preparation_guard = self
            .mcp_preparation_guards
            .lock()
            .expect("MCP preparation guard map lock poisoned")
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _preparation = preparation_guard.lock().await;
        let Some(feature) = factory.mcp_feature_for_session(record)? else {
            self.evict_mcp_runtime(session_id);
            return Ok(None);
        };
        let Some(prepared) = factory.prepare_mcp_runtime(&feature).await? else {
            self.evict_mcp_runtime(session_id);
            return Ok(None);
        };
        let cached = self
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .get(session_id)
            .cloned();
        if let Some(cached) = cached
            && cached.can_reuse(&prepared)
        {
            factory
                .preflight_cached_mcp_authentication(
                    &prepared.connections,
                    &cached.credentialed_servers,
                )
                .await?;
            return Ok(Some(cached.runtime));
        }
        let runtime = McpClientRuntime::start(
            prepared.config,
            factory.mcp_root_uri(),
            CancellationToken::new(),
        )
        .await
        .map_err(|error| mcp_start_error(error.server_key, error.error, &prepared.connections))?;
        let replaced = self
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .insert(
                session_id.clone(),
                CachedMcpRuntime {
                    runtime: Arc::clone(&runtime),
                    fingerprint: prepared.fingerprint,
                    credentialed_servers: prepared.credentialed_servers,
                },
            );
        if let Some(replaced) = replaced {
            spawn_runtime_shutdown(replaced.runtime);
        }
        Ok(Some(runtime))
    }

    fn evict_mcp_runtime(&self, session_id: &SessionId) {
        let evicted = self
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .remove(session_id);
        if let Some(evicted) = evicted {
            spawn_runtime_shutdown(evicted.runtime);
        }
    }
}

fn mcp_start_error(
    server_key: String,
    error: crate::LlmError,
    connections: &[super::runtime_factory::McpResolvedConnection],
) -> super::AgentSessionRuntimeError {
    match error {
        crate::LlmError::McpAuthorization {
            status,
            challenge,
            message,
        } => {
            let endpoint = connections
                .iter()
                .find(|server| server.key == server_key)
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
            super::AgentSessionRuntimeError::Authentication {
                target: crate::AuthTarget::mcp_server(server_key),
                message,
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                mcp_authorization,
            }
        }
        crate::LlmError::Authorization { message, .. } => {
            super::AgentSessionRuntimeError::Authentication {
                target: crate::AuthTarget::mcp_server(server_key),
                message,
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                mcp_authorization: None,
            }
        }
        error => super::AgentSessionRuntimeError::from(error),
    }
}

fn spawn_runtime_shutdown(runtime: Arc<McpClientRuntime>) {
    drop(tokio::spawn(async move {
        if shutdown_when_unused(runtime).await.is_err() {
            tracing::warn!("failed to shut down replaced MCP runtime");
        }
    }));
}

async fn shutdown_when_unused(runtime: Arc<McpClientRuntime>) -> Result<()> {
    while Arc::strong_count(&runtime) > 1 {
        tokio::task::yield_now().await;
    }
    runtime.shutdown().await
}

#[allow(clippy::too_many_arguments)]
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
        None => match coordinator.resolve_runtime(&session_id).await {
            Ok(runtime) => runtime,
            Err(error) => {
                if let Some(failure) = error.authentication_failure() {
                    coordinator
                        .persist_authentication_failure(&session_id, error.to_string(), failure)
                        .await?;
                }
                return Err(error);
            }
        },
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
            SessionStatus::Suspended(super::Suspension::AwaitingExternalInput { .. }) => {
                "external_request"
            }
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
            | AgentSessionRunError::OutcomeDropped { .. }
            | AgentSessionRunError::Runtime(_)
            | AgentSessionRunError::Agent(_),
        ) => "error",
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
        CompletionRequest, ExternalRequestKind, LlmStream, ModelApi, ModelRef, PermissionLevel,
        PermissionPattern, PermissionResponse, ProviderId, SessionRecord, SessionStatus, ToolCall,
        agent::session::{
            FileCredentialStore, InferenceProviderSettings, MemorySessionStore, SessionListEvent,
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
                        mcp_authorization: None,
                    },
                    message: "provider rejected the configured credential".to_owned(),
                })
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

    fn empty_test_agent() -> crate::ResolvedAgent {
        let mut registry = crate::AgentDefinitionRegistry::new();
        registry
            .load_yaml(
                r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec: {}
",
            )
            .expect("load test definition");
        registry.resolve_agent("test").expect("resolve test agent")
    }

    fn mcp_test_agent() -> crate::ResolvedAgent {
        let mut registry = crate::AgentDefinitionRegistry::new();
        registry
            .load_yaml(
                r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec: { features: [{ featureSetRef: { name: mcp } }] }
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: mcp }
spec: { mcp: {} }
",
            )
            .expect("load MCP test definition");
        registry.resolve_agent("test").expect("resolve test agent")
    }

    fn runtime_sessions<P>(provider: Arc<P>) -> SessionCoordinator<MemorySessionStore>
    where
        P: LlmStream + 'static,
    {
        let environment = AgentSessionRuntimeFactory::new(
            std::env::temp_dir(),
            FileCredentialStore::new(std::env::temp_dir().join("waltr-agent-queue-test-auth.json")),
            empty_test_agent(),
        )
        .with_runtime_provider(move |_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                provider.clone(),
            ))
        });

        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(environment)
    }

    fn authentication_runtime_error() -> crate::AgentSessionRuntimeError {
        crate::AgentSessionRuntimeError::Authentication {
            target: crate::AuthTarget::provider("configured-provider"),
            message: "provider OAuth credential is required".to_owned(),
            reason: crate::AuthenticationFailureReason::CredentialRequired,
            mcp_authorization: None,
        }
    }

    #[tokio::test]
    async fn wipe_sessions_clears_records_cached_mcp_runtimes_and_publishes_once() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let first = sessions.create().await.unwrap();
        let second = sessions.create().await.unwrap();
        for session_id in [&first.record.id, &second.record.id] {
            let runtime = McpClientRuntime::start(
                crate::mcp::McpConfig::new(),
                "file:///tmp".to_owned(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            sessions
                .mcp_sessions
                .lock()
                .expect("MCP runtime cache lock poisoned")
                .insert(
                    session_id.clone(),
                    CachedMcpRuntime {
                        runtime,
                        fingerprint: super::super::runtime_factory::McpRuntimeFingerprint {
                            trace_propagation: false,
                            connections: Vec::new(),
                        },
                        credentialed_servers: BTreeSet::new(),
                    },
                );
        }
        let mut watch = sessions.watch_sessions();

        sessions.wipe_sessions().await.unwrap();

        assert!(sessions.list().await.unwrap().is_empty());
        assert!(
            sessions
                .mcp_sessions
                .lock()
                .expect("MCP runtime cache lock poisoned")
                .is_empty()
        );
        let update = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
            .await
            .unwrap()
            .expect("wipe should publish");
        assert_eq!(update.sequence, 3);
        assert_eq!(update.event, SessionListEvent::Cleared);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), watch.next())
                .await
                .is_err(),
            "wipe should publish exactly one event"
        );
    }

    #[tokio::test]
    async fn resolved_agent_without_mcp_skips_runtime_configuration_and_start() {
        let directory = tempfile::tempdir().expect("temp directory");
        let mut definitions = crate::AgentDefinitionRegistry::new();
        definitions
            .load_yaml(
                r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: no-mcp }
spec: {}
",
            )
            .expect("load definition");
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            definitions
                .resolve_agent("no-mcp")
                .expect("resolve definition"),
        );
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let record = SessionRecord::new("no-mcp-session");

        let runtime = sessions
            .ensure_mcp_runtime(&record.id, &record, &factory)
            .await
            .expect("disabled MCP should skip invalid configuration");

        assert!(runtime.is_none());
        assert!(
            sessions
                .mcp_sessions
                .lock()
                .expect("MCP runtime cache lock poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn empty_resolution_evicts_cache_while_active_arc_survives() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            mcp_test_agent(),
        )
        .with_mcp_connection_resolver(|_| Box::pin(async { Ok(Vec::new()) }));
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let record = SessionRecord::new("empty-eviction");
        let runtime = McpClientRuntime::start(
            crate::mcp::McpConfig::new(),
            "file:///tmp".to_owned(),
            CancellationToken::new(),
        )
        .await
        .expect("empty MCP runtime should start");
        let active_user = Arc::clone(&runtime);
        sessions
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .insert(
                record.id.clone(),
                CachedMcpRuntime {
                    runtime,
                    fingerprint: super::super::runtime_factory::McpRuntimeFingerprint {
                        trace_propagation: false,
                        connections: Vec::new(),
                    },
                    credentialed_servers: BTreeSet::new(),
                },
            );

        let resolved = sessions
            .ensure_mcp_runtime(&record.id, &record, &factory)
            .await
            .expect("empty resolution succeeds");

        assert!(resolved.is_none());
        assert!(
            sessions
                .mcp_sessions
                .lock()
                .expect("MCP runtime cache lock poisoned")
                .is_empty()
        );
        assert!(!active_user.has_server("missing"));
    }

    #[tokio::test]
    async fn failed_replacement_preserves_but_does_not_return_cached_runtime() {
        let directory = tempfile::tempdir().expect("temp directory");
        let connection = super::super::runtime_factory::McpResolvedConnection {
            key: "broken".to_owned(),
            kind: super::super::runtime_factory::McpConnectionKind::Stdio,
            command: "/definitely/missing/waltr-mcp-server".to_owned(),
            url: String::new(),
        };
        let resolver_connection = connection.clone();
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            mcp_test_agent(),
        )
        .with_mcp_connection_resolver(move |_| {
            let connection = resolver_connection.clone();
            Box::pin(async move { Ok(vec![connection]) })
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let record = SessionRecord::new("failed-replacement");
        let old = McpClientRuntime::start(
            crate::mcp::McpConfig::new(),
            "file:///tmp".to_owned(),
            CancellationToken::new(),
        )
        .await
        .expect("empty MCP runtime should start");
        sessions
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .insert(
                record.id.clone(),
                CachedMcpRuntime {
                    runtime: Arc::clone(&old),
                    fingerprint: super::super::runtime_factory::McpRuntimeFingerprint {
                        trace_propagation: true,
                        connections: Vec::new(),
                    },
                    credentialed_servers: BTreeSet::new(),
                },
            );

        assert!(
            sessions
                .ensure_mcp_runtime(&record.id, &record, &factory)
                .await
                .is_err(),
            "failed reconstruction must not return stale tools"
        );
        let stored = sessions
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .get(&record.id)
            .expect("old cache remains stored")
            .runtime
            .clone();
        assert!(Arc::ptr_eq(&stored, &old));
    }

    #[tokio::test]
    async fn provider_only_inference_update_preserves_and_reuses_cached_mcp_runtime() {
        let directory = tempfile::tempdir().expect("temp directory");
        let connection = super::super::runtime_factory::McpResolvedConnection {
            key: "current".to_owned(),
            kind: super::super::runtime_factory::McpConnectionKind::StreamableHttp,
            command: String::new(),
            url: "https://mcp.example/rpc".to_owned(),
        };
        let resolver_connection = connection.clone();
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            mcp_test_agent(),
        )
        .with_mcp_connection_resolver(move |_| {
            let connection = resolver_connection.clone();
            Box::pin(async move { Ok(vec![connection]) })
        });
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .expect("create session");
        let runtime = McpClientRuntime::start(
            crate::mcp::McpConfig::new(),
            "file:///tmp".to_owned(),
            CancellationToken::new(),
        )
        .await
        .expect("empty MCP runtime should start");
        let active_user = Arc::clone(&runtime);
        sessions
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .insert(
                session.record.id.clone(),
                CachedMcpRuntime {
                    runtime: Arc::clone(&runtime),
                    fingerprint: super::super::runtime_factory::McpRuntimeFingerprint {
                        trace_propagation: true,
                        connections: vec![
                            super::super::runtime_factory::McpRuntimeFingerprintConnection::from_resolved(
                                &connection,
                            ),
                        ],
                    },
                    credentialed_servers: BTreeSet::new(),
                },
            );
        let mut updated_settings = test_inference_settings();
        updated_settings.provider.model_id = "updated-model".to_owned();

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            sessions.set_inference_settings(&session.record.id, Some(updated_settings)),
        )
        .await
        .expect("provider-only update must not wait for active MCP users")
        .expect("update inference settings");

        let stored = sessions
            .mcp_sessions
            .lock()
            .expect("MCP runtime cache lock poisoned")
            .get(&session.record.id)
            .expect("MCP cache remains stored")
            .runtime
            .clone();
        assert!(Arc::ptr_eq(&stored, &runtime));
        let current_record = sessions
            .load(&session.record.id)
            .await
            .expect("load updated session")
            .record;
        let reused = sessions
            .ensure_mcp_runtime(&session.record.id, &current_record, &factory)
            .await
            .expect("reconstruct MCP runtime")
            .expect("MCP runtime remains selected");
        assert!(Arc::ptr_eq(&reused, &runtime));
        assert!(Arc::ptr_eq(&active_user, &runtime));
    }

    #[tokio::test]
    async fn newly_available_mcp_credentials_prevent_anonymous_runtime_reuse() {
        let runtime = McpClientRuntime::start(
            crate::mcp::McpConfig::new(),
            "file:///tmp".to_owned(),
            CancellationToken::new(),
        )
        .await
        .expect("empty MCP runtime should start");
        let fingerprint = super::super::runtime_factory::McpRuntimeFingerprint {
            trace_propagation: true,
            connections: vec![
                super::super::runtime_factory::McpRuntimeFingerprintConnection {
                    key: "protected".to_owned(),
                    kind: super::super::runtime_factory::McpConnectionKind::StreamableHttp,
                    target: "https://mcp.example/rpc".to_owned(),
                },
            ],
        };
        let cached = CachedMcpRuntime {
            runtime,
            fingerprint: fingerprint.clone(),
            credentialed_servers: BTreeSet::new(),
        };
        let authenticated = super::super::runtime_factory::PreparedMcpRuntime {
            fingerprint,
            connections: vec![super::super::runtime_factory::McpResolvedConnection {
                key: "protected".to_owned(),
                kind: super::super::runtime_factory::McpConnectionKind::StreamableHttp,
                command: String::new(),
                url: "https://mcp.example/rpc".to_owned(),
            }],
            config: crate::mcp::McpConfig::new(),
            credentialed_servers: BTreeSet::from(["protected".to_owned()]),
        };

        assert!(
            !cached.can_reuse(&authenticated),
            "anonymous transport must be replaced when bearer capability appears"
        );
        let equivalent = super::super::runtime_factory::PreparedMcpRuntime {
            credentialed_servers: BTreeSet::new(),
            ..authenticated
        };
        assert!(cached.can_reuse(&equivalent));
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
            SessionListEvent::Cleared => panic!("expected upsert, got cleared"),
        }
    }

    #[tokio::test]
    async fn runtime_creation_preserves_agent_definition_bundle() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let bundle = AgentDefinitionBundle::new(
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
            .create_with_inference_settings(None, Some(bundle.clone()), BTreeMap::new())
            .await
            .expect("create session with agent definition");

        assert_eq!(created.record.agent_definition, Some(bundle));
    }

    fn catalog_runtime_sessions() -> SessionCoordinator<MemorySessionStore> {
        let source = r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: alpha }
spec: { systemPrompt: alpha prompt }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: beta }
spec: { systemPrompt: beta prompt }
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: gamma }
spec: { systemPrompt: gamma prompt }
";
        let catalog =
            crate::AgentDefinitionCatalog::from_sources(vec![source.to_owned()], "alpha").unwrap();
        let factory = AgentSessionRuntimeFactory::new(
            std::env::temp_dir(),
            FileCredentialStore::new(std::env::temp_dir().join("waltr-agent-switch-test-auth.json")),
            catalog.resolved("alpha").unwrap().clone(),
        )
        .with_agent_catalog(catalog)
        .with_runtime_provider(|_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                Arc::new(TextOnlyProvider),
            ))
        });
        SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory)
    }

    #[tokio::test]
    async fn switch_is_immediate_but_audit_waits_for_next_cycle_and_coalesces() {
        let sessions = catalog_runtime_sessions();
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .unwrap();
        let id = session.record.id;

        let beta = sessions.switch_agent(&id, "beta").await.unwrap();
        assert_eq!(beta.record.summary().agent_name.as_deref(), Some("beta"));
        sessions.switch_agent(&id, "gamma").await.unwrap();
        assert!(
            sessions
                .timeline(&id, TimelineQuery::forward(None, 20))
                .await
                .unwrap()
                .is_empty()
        );

        let captured = sessions
            .capture_runtime_record_for_cycle(&id)
            .await
            .unwrap();
        assert_eq!(
            captured.agent_definition.as_ref().unwrap().agent_name,
            "gamma"
        );
        assert!(
            captured
                .agent_definition
                .as_ref()
                .unwrap()
                .pending_switch_audit
                .is_none()
        );
        let events = sessions
            .timeline(&id, TimelineQuery::forward(None, 20))
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let TimelineContent::Json { value } = &events[0].content else {
            panic!("switch audit must be JSON");
        };
        assert_eq!(value["kind"], "agent_switch");
        assert_eq!(value["oldAgentName"], "alpha");
        assert_eq!(value["newAgentName"], "gamma");
        assert!(value.get("source").is_none());
        assert!(value.get("prompt").is_none());

        sessions
            .capture_runtime_record_for_cycle(&id)
            .await
            .unwrap();
        assert_eq!(
            sessions
                .timeline(&id, TimelineQuery::forward(None, 20))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn switch_after_capture_does_not_change_captured_cycle_bundle() {
        let sessions = catalog_runtime_sessions();
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .unwrap();
        let id = session.record.id;
        let before = sessions
            .capture_runtime_record_for_cycle(&id)
            .await
            .unwrap();

        sessions.switch_agent(&id, "beta").await.unwrap();

        assert!(before.agent_definition.is_none());
        let after = sessions
            .capture_runtime_record_for_cycle(&id)
            .await
            .unwrap();
        assert_eq!(after.agent_definition.unwrap().agent_name, "beta");
    }

    #[tokio::test]
    async fn missing_catalog_agent_fails_without_mutating_selection() {
        let sessions = catalog_runtime_sessions();
        let session = sessions.create().await.unwrap();

        assert!(
            sessions
                .switch_agent(&session.record.id, "missing")
                .await
                .is_err()
        );
        assert!(
            sessions
                .load(&session.record.id)
                .await
                .unwrap()
                .record
                .agent_definition
                .is_none()
        );
    }

    struct SwitchPermissionProvider;

    impl LlmStream for SwitchPermissionProvider {
        fn name(&self) -> &'static str {
            "switch_permission_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = if request.tool_results().next().is_some() {
                    vec![Ok(CompletionEvent::Finished)]
                } else {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "question-call",
                        "question",
                        json!({
                            "questions": [{
                                "question": "Continue?",
                                "header": "Switch",
                                "options": [{ "label": "Yes", "description": "Continue" }],
                                "multiple": false,
                                "custom": false
                            }]
                        }),
                    )))]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn switched_deny_constrains_retained_allow_always_approval() {
        let source = r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: alpha }
spec:
  permissions: [{ name: question, permission: ask }]
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: beta }
spec:
  permissions: [{ name: question, permission: deny }]
";
        let catalog =
            crate::AgentDefinitionCatalog::from_sources(vec![source.to_owned()], "alpha").unwrap();
        let factory = AgentSessionRuntimeFactory::new(
            std::env::temp_dir(),
            FileCredentialStore::new(
                std::env::temp_dir().join("waltr-agent-switch-permission-auth.json"),
            ),
            catalog.resolved("alpha").unwrap().clone(),
        )
        .with_agent_catalog(catalog)
        .with_runtime_provider(|_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                Arc::new(SwitchPermissionProvider),
            ))
        });
        let sessions =
            SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .unwrap();
        let id = session.record.id;
        let initial = sessions
            .submit_prompt(id.clone(), "ask", RunConflictPolicy::ReplaceActive)
            .await
            .unwrap();
        initial.handle.wait().await.unwrap();
        let record = sessions.load(&id).await.unwrap().record;
        let permission_request = record
            .snapshot()
            .external_requests
            .into_iter()
            .find(|request| request.kind == ExternalRequestKind::Permission)
            .expect("alpha should request whole-tool permission");
        let patterns: Vec<PermissionPattern> =
            serde_json::from_value(permission_request.payload["patterns"].clone()).unwrap();

        sessions.switch_agent(&id, "beta").await.unwrap();
        let resumed = sessions
            .resume_external_response(
                id.clone(),
                permission_request.id,
                json!(PermissionResponse::AllowAlways {
                    patterns: patterns.clone(),
                }),
                RunConflictPolicy::ReplaceActive,
            )
            .await
            .unwrap();
        resumed.handle.wait().await.unwrap();

        let record = sessions.load(&id).await.unwrap().record;
        let snapshot = record.snapshot();
        assert_eq!(
            snapshot.middleware_state["permissions"]["patterns"],
            json!(patterns)
        );
        let beta_runtime = sessions.resolve_runtime(&id).await.unwrap();
        assert_eq!(
            beta_runtime.permission_policy.evaluate_tool("question"),
            PermissionLevel::Deny
        );
        assert!(
            snapshot
                .external_requests
                .iter()
                .all(|request| request.kind != ExternalRequestKind::Question)
        );
        assert!(snapshot.tool_results().any(|result| {
            result.call_id == "question-call"
                && result.is_error
                && result.content.contains("tool is not permitted: question")
        }));
    }

    struct SwitchTaskProvider;

    impl LlmStream for SwitchTaskProvider {
        fn name(&self) -> &'static str {
            "switch_task_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let results = request.tool_results().collect::<Vec<_>>();
                let has_result =
                    |call_id: &str| results.iter().any(|result| result.call_id == call_id);
                let events = match request.prompt {
                    "spawn alpha" if !has_result("alpha-task") => {
                        vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "alpha-task",
                            "task",
                            json!({ "mode": "sync", "prompt": "alpha child" }),
                        )))]
                    }
                    "spawn beta" if !has_result("beta-task") => {
                        vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "beta-task",
                            "task",
                            json!({ "mode": "sync", "prompt": "beta child" }),
                        )))]
                    }
                    "alpha child" | "beta child" => vec![
                        Ok(CompletionEvent::Delta("child done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ],
                    "spawn alpha" | "spawn beta" => vec![Ok(CompletionEvent::Finished)],
                    prompt => panic!("unexpected task request: {prompt:?} {results:?}"),
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn task_children_inherit_the_runtime_captured_agent_bundle() {
        let source = r"
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: alpha }
spec:
  features: [{ task: { maxSubagentDepth: 1, defaultPermissions: true } }]
---
apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: beta }
spec:
  features: [{ task: { maxSubagentDepth: 1, defaultPermissions: true } }]
";
        let catalog =
            crate::AgentDefinitionCatalog::from_sources(vec![source.to_owned()], "alpha").unwrap();
        let factory = AgentSessionRuntimeFactory::new(
            std::env::temp_dir(),
            FileCredentialStore::new(std::env::temp_dir().join("waltr-agent-switch-task-auth.json")),
            catalog.resolved("alpha").unwrap().clone(),
        )
        .with_agent_catalog(catalog)
        .with_runtime_provider(|_| {
            Ok(crate::AgentRuntimeProvider::new(
                ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
                Arc::new(SwitchTaskProvider),
            ))
        });
        let sessions =
            SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
        let parent = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .unwrap();
        let parent_id = parent.record.id;
        let alpha_runtime = sessions.resolve_runtime(&parent_id).await.unwrap();

        sessions.switch_agent(&parent_id, "beta").await.unwrap();
        sessions
            .prompt(&alpha_runtime, parent_id.clone(), "spawn alpha")
            .stream()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        let alpha_child_id = sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| {
                summary
                    .metadata_values
                    .get("waltr.task.tool_call_id")
                    .is_some_and(|value| value == "alpha-task")
            })
            .expect("alpha child summary")
            .id;
        let alpha_child = sessions.load(&alpha_child_id).await.unwrap();
        assert_eq!(
            alpha_child.record.agent_definition.unwrap().agent_name,
            "alpha"
        );

        let beta_runtime = sessions.resolve_runtime(&parent_id).await.unwrap();
        sessions
            .prompt(&beta_runtime, parent_id.clone(), "spawn beta")
            .stream()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<crate::Result<Vec<_>>>()
            .unwrap();
        let beta_child_id = sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|summary| {
                summary
                    .metadata_values
                    .get("waltr.task.tool_call_id")
                    .is_some_and(|value| value == "beta-task")
            })
            .expect("beta child summary")
            .id;
        let beta_child = sessions.load(&beta_child_id).await.unwrap();
        assert_eq!(
            beta_child.record.agent_definition.unwrap().agent_name,
            "beta"
        );
    }

    #[tokio::test]
    async fn mutation_gate_linearizes_runtime_record_capture() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider));
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .unwrap();
        let id = session.record.id;
        let gate = sessions.mutation_gate(&id);
        let held = gate.lock().await;
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
            captured.inference_settings.unwrap().provider.model_id,
            "test-model"
        );

        let captured_after = sessions.capture_runtime_record(&id).await.unwrap();
        assert_eq!(
            captured_after.inference_settings.unwrap().provider.model_id,
            "new-model"
        );
    }

    #[tokio::test]
    async fn runtime_startup_does_not_hold_mutation_gate() {
        let sessions = runtime_sessions(Arc::new(TextOnlyProvider));
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
            .await
            .unwrap();
        let id = session.record.id;

        let _runtime = sessions.resolve_runtime(&id).await.unwrap();
        let gate = sessions.mutation_gate(&id);
        let _guard = tokio::time::timeout(std::time::Duration::from_millis(100), gate.lock())
            .await
            .expect("runtime startup must release the mutation gate");
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
        let sessions = runtime_sessions(provider.clone());
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
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
    async fn asynchronous_provider_authentication_failure_is_durable() {
        let sessions = runtime_sessions(Arc::new(AuthenticationFailingProvider));
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
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

    #[tokio::test]
    async fn queued_runtime_authentication_failure_is_durable() {
        let provider = Arc::new(QueuedPromptProvider::new());
        let resolutions = Arc::new(AtomicU64::new(0));
        let resolution_count = Arc::clone(&resolutions);
        let runtime_provider = Arc::clone(&provider);
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
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
        let sessions =
            SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
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
    async fn continue_authentication_preflight_does_not_append_rollback() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_runtime_provider(|_| Err(authentication_runtime_error()));
        let sessions =
            SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(factory);
        let session = sessions
            .create_with_inference_settings(Some(test_inference_settings()), None, BTreeMap::new())
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
