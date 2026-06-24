//! Durable session runtime helpers.

#![allow(missing_docs)]

use crate::{
    LlmError, PermissionDenyMode, PermissionResponse, Result, ToolResult,
    permission::{PERMISSION_STATE_NAMESPACE, PermissionRuntimeState},
    timeline::{
        CHANNEL_EXTERNAL_REQUEST, PendingTimelineEvent, TimelineChannel, TimelineContent,
        TimelineEventKind, TimelineItemId,
    },
};

use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestKind, ExternalRequestStatus,
    ExternalResponse, PendingWorkStatus, ResumeInput, SessionId, SessionJournalEvent,
    SessionListBroadcaster, SessionListEvent, SessionRecord, SessionSnapshot, SessionStatus,
    SessionStore, SessionSummary, SessionTimelineBroadcaster, Suspension,
};

/// Runtime facade for durable session resume operations.
#[derive(Debug)]
pub struct AgentSessionRuntime<'a, S>
where
    S: SessionStore + ?Sized,
{
    store: &'a S,
    sessions: Option<SessionListBroadcaster>,
    timeline: Option<SessionTimelineBroadcaster>,
}

impl<'a, S> AgentSessionRuntime<'a, S>
where
    S: SessionStore + ?Sized,
{
    /// Create a runtime over a session store.
    pub const fn new(store: &'a S) -> Self {
        Self {
            store,
            sessions: None,
            timeline: None,
        }
    }

    pub(crate) fn with_session_broadcaster(mut self, sessions: SessionListBroadcaster) -> Self {
        self.sessions = Some(sessions);
        self
    }

    pub(crate) fn with_timeline_broadcaster(
        mut self,
        timeline: SessionTimelineBroadcaster,
    ) -> Self {
        self.timeline = Some(timeline);
        self
    }

    /// Create a new durable session record.
    ///
    /// # Errors
    ///
    /// Returns an error when the store rejects the new record.
    pub async fn create_session(&self, id: SessionId) -> Result<SessionRecord> {
        self.store.create(id).await.map_err(session_store_error)
    }

    /// Create a new durable session record with initial host-defined metadata values.
    ///
    /// # Errors
    ///
    /// Returns an error when the store rejects the new record.
    pub async fn create_session_with_metadata_values(
        &self,
        id: SessionId,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<SessionRecord> {
        self.store
            .create_with_metadata_values(id, metadata_values)
            .await
            .map_err(session_store_error)
    }

    /// Load a durable session record.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn load_session(&self, id: &SessionId) -> Result<SessionRecord> {
        self.store.load(id).await.map_err(session_store_error)
    }

    /// Delete a durable session record if present.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot complete the delete operation.
    pub async fn delete_session(&self, id: &SessionId) -> Result<()> {
        self.store.delete(id).await.map_err(session_store_error)
    }

    /// List durable sessions, newest activity first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot list sessions.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        self.store.list().await.map_err(session_store_error)
    }

    /// Apply resume input to a persisted session and return the durable checkpoint reached.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is stale or invalid, or when the store fails.
    pub async fn resume(&self, id: &SessionId, input: ResumeInput) -> Result<RuntimeResumeOutput> {
        let record = self.load_session(id).await?;
        match input {
            ResumeInput::Prompt(prompt) => self.resume_prompt(record, prompt).await,
            ResumeInput::ExternalResponse {
                request_id,
                response,
            } => {
                self.resume_external_response(record, request_id, response)
                    .await
            }
            ResumeInput::ToolResults(results) => self.resume_tool_results(record, results).await,
        }
    }

    /// Persist an interruption for a durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or interruption state cannot be saved.
    pub async fn interrupt(&self, id: &SessionId) -> Result<SessionRecord> {
        let record = self.load_session(id).await?;
        self.interrupt_record(record).await
    }

    /// Set a host-defined metadata value.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn set_metadata_value(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
    ) -> Result<SessionRecord> {
        self.store
            .set_metadata_value(id, key, value)
            .await
            .map_err(session_store_error)
    }

    /// Remove a host-defined metadata value.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn remove_metadata_value(
        &self,
        id: &SessionId,
        key: String,
    ) -> Result<SessionRecord> {
        self.store
            .remove_metadata_value(id, key)
            .await
            .map_err(session_store_error)
    }

    async fn interrupt_record(&self, record: SessionRecord) -> Result<SessionRecord> {
        let snapshot = record.snapshot();
        if matches!(
            snapshot.status,
            SessionStatus::Idle
                | SessionStatus::Finished
                | SessionStatus::Failed { .. }
                | SessionStatus::Suspended(Suspension::Interrupted { .. })
        ) {
            return Ok(record);
        }

        let mut journal = Vec::new();
        let mut timeline = Vec::new();
        for request in pending_external_requests(&snapshot) {
            let reason = CancelReason::SessionInterrupted;
            journal.push(SessionJournalEvent::ExternalRequestCanceled {
                request_id: request.id.clone(),
                reason: reason.clone(),
            });
            let mut request = request.clone();
            request.status = ExternalRequestStatus::Canceled(reason);
            timeline.push(external_request_timeline_event(
                &record.id,
                snapshot.turn,
                request,
                TimelineEventKind::ExternalRequestResolved,
            ));
        }
        for work_id in pending_work_ids(&snapshot) {
            journal.push(SessionJournalEvent::WorkCanceled {
                work_id,
                reason: CancelReason::SessionInterrupted,
            });
        }
        journal.push(SessionJournalEvent::Interrupted);
        timeline.push(interrupted_status_timeline_event(&record.id, snapshot.turn));

        let mut record = self.append_journal_and_reload(record, journal).await?;
        record = self.append_timeline_and_reload(record, timeline).await?;
        Ok(record)
    }

    async fn resume_prompt(
        &self,
        record: SessionRecord,
        prompt: String,
    ) -> Result<RuntimeResumeOutput> {
        let snapshot = record.snapshot();
        let mut journal = Vec::new();
        let mut timeline = Vec::new();
        for request in pending_external_requests(&snapshot) {
            let reason = CancelReason::SupersededByUserPrompt;
            journal.push(SessionJournalEvent::ExternalRequestCanceled {
                request_id: request.id.clone(),
                reason: reason.clone(),
            });
            let mut request = request.clone();
            request.status = ExternalRequestStatus::Canceled(reason);
            timeline.push(external_request_timeline_event(
                &record.id,
                snapshot.turn,
                request,
                TimelineEventKind::ExternalRequestResolved,
            ));
        }
        for work_id in pending_work_ids(&snapshot) {
            journal.push(SessionJournalEvent::WorkCanceled {
                work_id,
                reason: CancelReason::SupersededByUserPrompt,
            });
        }
        timeline.push(user_prompt_timeline_event(
            &record.id,
            snapshot.turn,
            prompt.clone(),
        ));
        journal.push(SessionJournalEvent::PromptAppended { prompt });

        let mut record = self.append_journal_and_reload(record, journal).await?;
        if !timeline.is_empty() {
            record = self.append_timeline_and_reload(record, timeline).await?;
        }
        Ok(RuntimeResumeOutput::Resumed { record })
    }

    async fn resume_external_response(
        &self,
        record: SessionRecord,
        request_id: ExternalRequestId,
        response: ExternalResponse,
    ) -> Result<RuntimeResumeOutput> {
        let snapshot = record.snapshot();
        let request = validate_pending_external_response(&snapshot, &request_id)?;
        let turn = snapshot.turn;
        let mut record = self
            .append_journal_and_reload(
                record,
                vec![SessionJournalEvent::ExternalRequestResponded {
                    request_id: request_id.clone(),
                    response: response.clone(),
                }],
            )
            .await?;

        let mut resolved_request = request.clone();
        resolved_request.status = ExternalRequestStatus::Responded(response.clone());
        let session_id = record.id.clone();
        record = self
            .append_timeline_and_reload(
                record,
                vec![external_request_timeline_event(
                    &session_id,
                    turn,
                    resolved_request,
                    TimelineEventKind::ExternalRequestResolved,
                )],
            )
            .await?;

        match &request.kind {
            ExternalRequestKind::Permission => {
                record = self
                    .apply_permission_response(record, &snapshot, &request, &request_id, response)
                    .await?;
            }
            ExternalRequestKind::Question
            | ExternalRequestKind::ToolInput
            | ExternalRequestKind::Custom(_) => {
                if let Some(tool_call_id) = request.tool_call_id {
                    let result = ToolResult::new(
                        tool_call_id,
                        request_tool_name(&request.kind),
                        external_response_content(&response),
                    );
                    let session_id = record.id.clone();
                    record = self
                        .append_timeline_and_reload(
                            record,
                            vec![tool_result_timeline_event(&session_id, turn, result)],
                        )
                        .await?;
                    record = self
                        .append_journal_and_reload(
                            record,
                            vec![SessionJournalEvent::TurnFinished { turn }],
                        )
                        .await?;
                }
            }
        }

        Ok(RuntimeResumeOutput::Resumed { record })
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_permission_response(
        &self,
        record: SessionRecord,
        snapshot: &SessionSnapshot,
        request: &ExternalRequest,
        request_id: &ExternalRequestId,
        response: ExternalResponse,
    ) -> Result<SessionRecord> {
        let response = PermissionResponse::from_value(response.payload)?;
        let Some(tool_call_id) = request.tool_call_id.as_deref() else {
            return Err(runtime_error("permission request missing tool call id"));
        };
        let turn = snapshot.turn;

        match response {
            PermissionResponse::AllowOnce => {
                let mut state = permission_state(snapshot)?;
                state.allow_once(request_id, tool_call_id);
                self.append_journal_and_reload(record, vec![permission_state_event(&state)])
                    .await
            }
            PermissionResponse::AllowAlways { patterns } => {
                let mut state = permission_state(snapshot)?;
                state.allow_patterns(patterns);
                state.allow_once(request_id, tool_call_id);
                self.append_journal_and_reload(record, vec![permission_state_event(&state)])
                    .await
            }
            PermissionResponse::Deny { message, mode } => match mode {
                PermissionDenyMode::ReturnToolResult => {
                    let result = ToolResult::new(
                        tool_call_id.to_owned(),
                        "permission",
                        message.unwrap_or_else(|| "permission denied".to_owned()),
                    );
                    let session_id = record.id.clone();
                    let record = self
                        .append_timeline_and_reload(
                            record,
                            vec![tool_result_timeline_event(&session_id, turn, result)],
                        )
                        .await?;
                    self.append_journal_and_reload(
                        record,
                        vec![SessionJournalEvent::TurnFinished { turn }],
                    )
                    .await
                }
                PermissionDenyMode::CancelWork => {
                    self.append_journal_and_reload(
                        record,
                        vec![SessionJournalEvent::WorkCanceled {
                            work_id: tool_call_id.to_owned(),
                            reason: CancelReason::ToolUnavailable,
                        }],
                    )
                    .await
                }
            },
        }
    }

    async fn resume_tool_results(
        &self,
        record: SessionRecord,
        results: Vec<ToolResult>,
    ) -> Result<RuntimeResumeOutput> {
        let turn = record.snapshot().turn;
        let mut record = record;
        if !results.is_empty() {
            let events = results
                .into_iter()
                .map(|result| tool_result_timeline_event(&record.id, turn, result))
                .collect();
            record = self.append_timeline_and_reload(record, events).await?;
            record = self
                .append_journal_and_reload(record, vec![SessionJournalEvent::TurnFinished { turn }])
                .await?;
        }
        Ok(RuntimeResumeOutput::Resumed { record })
    }

    async fn append_journal_and_reload(
        &self,
        record: SessionRecord,
        events: Vec<SessionJournalEvent>,
    ) -> Result<SessionRecord> {
        let id = record.id.clone();
        self.store
            .append_journal(&id, record.revision, events)
            .await
            .map_err(session_store_error)?;
        let record = self.load_session(&id).await?;
        self.publish_session_summary(&record);
        Ok(record)
    }

    async fn append_timeline_and_reload(
        &self,
        record: SessionRecord,
        events: Vec<PendingTimelineEvent>,
    ) -> Result<SessionRecord> {
        let id = record.id.clone();
        let assigned = self
            .store
            .append_timeline(&id, record.revision, events)
            .await
            .map_err(session_store_error)?;
        if let Some(timeline) = &self.timeline {
            timeline.publish_all(assigned);
        }
        let record = self.load_session(&id).await?;
        self.publish_session_summary(&record);
        Ok(record)
    }

    fn publish_session_summary(&self, record: &SessionRecord) {
        if let Some(sessions) = &self.sessions {
            sessions.publish(SessionListEvent::Upsert(record.summary()));
        }
    }
}

/// Result of applying a runtime resume operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeResumeOutput {
    /// Runtime accepted input and can continue building a provider request.
    Resumed { record: SessionRecord },
    /// Runtime remains suspended.
    Suspended {
        record: SessionRecord,
        suspension: Suspension,
    },
}

fn validate_pending_external_response(
    snapshot: &SessionSnapshot,
    request_id: &ExternalRequestId,
) -> Result<ExternalRequest> {
    let request = snapshot
        .external_requests
        .iter()
        .rev()
        .find(|request| request.id == *request_id)
        .ok_or_else(|| {
            runtime_error(format!(
                "external request not found: {}",
                request_id.as_str()
            ))
        })?;

    match &request.status {
        ExternalRequestStatus::Pending => Ok(request.clone()),
        ExternalRequestStatus::Responded(_) => Err(runtime_error(format!(
            "external request already answered: {}",
            request_id.as_str()
        ))),
        ExternalRequestStatus::Canceled(reason) => Err(runtime_error(format!(
            "external request canceled: {} ({reason:?})",
            request_id.as_str()
        ))),
    }
}

fn pending_external_requests(snapshot: &SessionSnapshot) -> Vec<&ExternalRequest> {
    snapshot
        .external_requests
        .iter()
        .filter(|request| matches!(request.status, ExternalRequestStatus::Pending))
        .collect()
}

fn pending_work_ids(snapshot: &SessionSnapshot) -> Vec<String> {
    snapshot
        .pending_work
        .iter()
        .filter(|work| matches!(work.status, PendingWorkStatus::Pending))
        .map(|work| work.id.clone())
        .collect()
}

fn permission_state(snapshot: &SessionSnapshot) -> Result<PermissionRuntimeState> {
    snapshot
        .middleware_state
        .get(PERMISSION_STATE_NAMESPACE)
        .map(|state| serde_json::from_value(state.clone()))
        .transpose()
        .map_err(|error| runtime_error(error.to_string()))
        .map(Option::unwrap_or_default)
}

fn permission_state_event(state: &PermissionRuntimeState) -> SessionJournalEvent {
    SessionJournalEvent::MiddlewareStateUpdated {
        namespace: PERMISSION_STATE_NAMESPACE.to_owned(),
        state: serde_json::to_value(state).expect("permission runtime state serializes"),
    }
}

fn tool_result_timeline_event(
    session_id: &SessionId,
    turn: u64,
    result: ToolResult,
) -> PendingTimelineEvent {
    let call_id = result.call_id.clone();
    PendingTimelineEvent::tool_result(
        session_id.clone(),
        turn,
        format!("tool-result:{call_id}"),
        format!("tool-call:{call_id}"),
        result,
    )
}

fn user_prompt_timeline_event(
    session_id: &SessionId,
    turn: u64,
    prompt: String,
) -> PendingTimelineEvent {
    PendingTimelineEvent::user_text_snapshot(
        session_id.clone(),
        turn,
        TimelineItemId::new(format!("user-turn-{turn}")),
        prompt,
    )
}

fn external_request_timeline_event(
    session_id: &SessionId,
    turn: u64,
    request: ExternalRequest,
    kind: TimelineEventKind,
) -> PendingTimelineEvent {
    PendingTimelineEvent::new(
        session_id.clone(),
        turn,
        format!("external-request:{}", request.id.as_str()),
        kind,
        Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
        TimelineContent::ExternalRequest { request },
    )
}

fn interrupted_status_timeline_event(session_id: &SessionId, turn: u64) -> PendingTimelineEvent {
    let mut event = PendingTimelineEvent::new(
        session_id.clone(),
        turn,
        "session-status",
        TimelineEventKind::SessionStatusChanged,
        None,
        TimelineContent::Empty,
    );
    event.status = Some(crate::timeline::TimelineStatus::Interrupted);
    event
}

const fn request_tool_name(kind: &super::ExternalRequestKind) -> &'static str {
    match kind {
        super::ExternalRequestKind::Question => "question",
        super::ExternalRequestKind::Permission => "permission",
        super::ExternalRequestKind::ToolInput | super::ExternalRequestKind::Custom(_) => {
            "external_request"
        }
    }
}

fn external_response_content(response: &ExternalResponse) -> String {
    response
        .payload
        .as_str()
        .map_or_else(|| response.payload.to_string(), str::to_owned)
}

fn session_store_error(error: impl std::fmt::Display) -> LlmError {
    runtime_error(error.to_string())
}

fn runtime_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "session-runtime",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ToolCall;
    use crate::agent::session::{ExternalRequestKind, MemorySessionStore, Revision, SessionStore};

    fn session_id() -> SessionId {
        SessionId::new("runtime-session")
    }

    fn external_request(id: &str) -> ExternalRequest {
        ExternalRequest {
            id: ExternalRequestId::new(id),
            kind: ExternalRequestKind::Question,
            tool_call_id: Some("call-question".to_owned()),
            prompt: "Pick one".to_owned(),
            payload: json!({ "questions": [] }),
            status: ExternalRequestStatus::Pending,
        }
    }

    async fn suspended_question_session(store: &MemorySessionStore) -> SessionId {
        let id = session_id();
        store.create(id.clone()).await.unwrap();
        let revision = store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "ask".to_owned(),
                }],
            )
            .await
            .unwrap();
        let revision = {
            store
                .append_timeline(
                    &id,
                    revision,
                    vec![
                        PendingTimelineEvent::tool_call(
                            id.clone(),
                            0,
                            "tool-call:call-question",
                            ToolCall::new("call-question", "question", json!({})),
                        ),
                        external_request_timeline_event(
                            &id,
                            0,
                            external_request("question-1"),
                            TimelineEventKind::ExternalRequestOpened,
                        ),
                    ],
                )
                .await
                .unwrap();
            Revision::new(2)
        };
        let revision = store
            .append_journal(
                &id,
                revision,
                vec![SessionJournalEvent::Suspended {
                    suspension: Suspension::AwaitingExternalInput {
                        request: Box::new(external_request("question-1")),
                    },
                }],
            )
            .await
            .unwrap();
        assert_eq!(revision, Revision::new(3));
        id
    }

    #[tokio::test]
    async fn create_session_persists_empty_record() {
        let store = MemorySessionStore::new();
        let runtime = AgentSessionRuntime::new(&store);

        let record = runtime.create_session(session_id()).await.unwrap();

        assert_eq!(record.id, session_id());
        assert_eq!(record.revision, Revision::ZERO);
        assert!(record.journal.is_empty());
    }

    #[tokio::test]
    async fn external_response_records_answer_and_tool_result() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);

        let output = runtime
            .resume(
                &id,
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new("question-1"),
                    response: ExternalResponse {
                        payload: json!({ "answers": [{ "header": "Choice" }] }),
                    },
                },
            )
            .await
            .unwrap();

        let RuntimeResumeOutput::Resumed { record } = output else {
            panic!("external response should resume");
        };
        let snapshot = record.snapshot();
        assert!(matches!(
            snapshot.external_requests[0].status,
            ExternalRequestStatus::Responded(_)
        ));
        let tool_results: Vec<_> = snapshot.tool_results().collect();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0].call_id, "call-question");
        assert_eq!(tool_results[0].name, "question");
        assert_eq!(snapshot.turns[0].turn, 0);
        assert_eq!(
            snapshot.turn, 1,
            "answering the suspended call should finish the turn"
        );
    }

    #[tokio::test]
    async fn stale_external_response_is_rejected_deterministically() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);

        runtime
            .resume(
                &id,
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new("question-1"),
                    response: ExternalResponse {
                        payload: json!("A"),
                    },
                },
            )
            .await
            .unwrap();
        let error = runtime
            .resume(
                &id,
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new("question-1"),
                    response: ExternalResponse {
                        payload: json!("B"),
                    },
                },
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("already answered"));
    }

    #[tokio::test]
    async fn prompt_resume_cancels_pending_external_request_and_work() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);

        let output = runtime
            .resume(&id, ResumeInput::Prompt("new prompt".to_owned()))
            .await
            .unwrap();

        let RuntimeResumeOutput::Resumed { record } = output else {
            panic!("prompt should resume");
        };
        let snapshot = record.snapshot();
        assert_eq!(snapshot.current_prompt.as_deref(), Some("new prompt"));
        assert_eq!(
            snapshot.external_requests[0].status,
            ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt)
        );
        assert_eq!(
            snapshot.pending_work[0].status,
            PendingWorkStatus::Canceled(CancelReason::SupersededByUserPrompt)
        );
    }

    #[tokio::test]
    async fn interrupt_cancels_pending_external_request_and_work() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);

        let record = runtime.interrupt(&id).await.unwrap();

        let snapshot = record.snapshot();
        assert!(matches!(
            snapshot.status,
            SessionStatus::Suspended(Suspension::Interrupted { .. })
        ));
        assert_eq!(
            snapshot.external_requests[0].status,
            ExternalRequestStatus::Canceled(CancelReason::SessionInterrupted)
        );
        assert_eq!(
            snapshot.pending_work[0].status,
            PendingWorkStatus::Canceled(CancelReason::SessionInterrupted)
        );
        assert!(
            record
                .journal
                .iter()
                .any(|event| matches!(event, SessionJournalEvent::Interrupted))
        );
    }

    #[tokio::test]
    async fn interrupt_is_idempotent() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);

        let first = runtime.interrupt(&id).await.unwrap();
        let second = runtime.interrupt(&id).await.unwrap();

        assert_eq!(second.revision, first.revision);
        assert_eq!(
            second
                .journal
                .iter()
                .filter(|event| matches!(event, SessionJournalEvent::Interrupted))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn interrupted_external_response_is_rejected_deterministically() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);
        runtime.interrupt(&id).await.unwrap();

        let error = runtime
            .resume(
                &id,
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new("question-1"),
                    response: ExternalResponse {
                        payload: json!("late"),
                    },
                },
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("canceled"));
    }

    #[tokio::test]
    async fn canceled_external_response_is_rejected_deterministically() {
        let store = MemorySessionStore::new();
        let id = suspended_question_session(&store).await;
        let runtime = AgentSessionRuntime::new(&store);
        runtime
            .resume(&id, ResumeInput::Prompt("replacement".to_owned()))
            .await
            .unwrap();

        let error = runtime
            .resume(
                &id,
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new("question-1"),
                    response: ExternalResponse {
                        payload: json!("late"),
                    },
                },
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("canceled"));
    }
}
