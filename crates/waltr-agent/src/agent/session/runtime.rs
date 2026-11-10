//! Durable session runtime helpers.

#![allow(missing_docs)]

use crate::{
    AuthenticationFailure, LlmError, Result, ToolResult,
    timeline::{
        PendingTimelineEvent, PendingTimelineItemId, TimelineContent, TimelineCursor,
        TimelineEventKind, TimelineItemId,
    },
};

use std::sync::Arc;

use serde_json::Value;

use super::{
    CancelReason, InferenceSettings, MetadataSetCondition, MetadataSetOutcome, PendingWork,
    PendingWorkStatus, ResumeInput, SessionCreateRequest, SessionId, SessionIncarnation,
    SessionJournalEvent, SessionRecord, SessionRollbackTarget, SessionRunContext, SessionSnapshot,
    SessionStatus, SessionStore, SessionSummary, Suspension,
};

/// Runtime facade for durable session resume operations.
pub struct AgentSessionRuntime<'a, S>
where
    S: SessionStore + ?Sized,
{
    store: &'a S,
    run_context: Option<SessionRunContext>,
    session_incarnation: Option<SessionIncarnation>,
    event_sink: Option<Arc<dyn crate::SessionEventSink>>,
}

impl<S> std::fmt::Debug for AgentSessionRuntime<'_, S>
where
    S: SessionStore + ?Sized,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionRuntime")
            .field("run_context", &self.run_context.is_some())
            .field("event_sink", &self.event_sink.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a, S> AgentSessionRuntime<'a, S>
where
    S: SessionStore + ?Sized,
{
    /// Create a runtime over a session store.
    pub const fn new(store: &'a S) -> Self {
        Self {
            store,
            run_context: None,
            session_incarnation: None,
            event_sink: None,
        }
    }

    pub(crate) fn with_session_run_context(
        mut self,
        context: SessionRunContext,
        incarnation: SessionIncarnation,
    ) -> Self {
        self.run_context = Some(context);
        self.session_incarnation = Some(incarnation);
        self
    }

    /// Configure semantic event delivery for prompt appends performed by this runtime.
    #[must_use]
    pub fn with_session_event_sink(mut self, sink: Arc<dyn crate::SessionEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Create a durable session from complete structured input.
    ///
    /// # Errors
    ///
    /// Returns an error when the store rejects the new record.
    pub(crate) async fn create(&self, request: SessionCreateRequest) -> Result<SessionRecord> {
        let initial_messages = match &request.start {
            super::SessionStart::Fresh { messages }
            | super::SessionStart::Copy { messages, .. } => messages
                .iter()
                .map(|message| message.text().to_owned())
                .collect::<Vec<_>>(),
        };
        let record = self
            .store
            .create(request)
            .await
            .map_err(session_store_error)?;
        if !initial_messages.is_empty() {
            let prompt_count = record.snapshot().prompts.len();
            let first_prompt_index = prompt_count.saturating_sub(initial_messages.len());
            let journal_indices = record
                .journal
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    matches!(event, SessionJournalEvent::PromptAppended { .. }).then_some(index)
                })
                .collect::<Vec<_>>();
            let first = journal_indices.len().saturating_sub(initial_messages.len());
            self.publish_user_message_events(
                &record,
                first_prompt_index,
                journal_indices
                    .into_iter()
                    .skip(first)
                    .zip(initial_messages)
                    .collect(),
            );
        }
        Ok(record)
    }

    /// Load a durable session record.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn load_session(&self, id: &SessionId) -> Result<SessionRecord> {
        let record = self.store.load(id).await.map_err(session_store_error)?;
        if let (Some(context), Some(incarnation)) = (&self.run_context, self.session_incarnation) {
            context.register_loaded_summary(incarnation, record.summary());
        }
        Ok(record)
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
        self.resume_record(record, input).await
    }

    pub(crate) async fn resume_record(
        &self,
        record: SessionRecord,
        input: ResumeInput,
    ) -> Result<RuntimeResumeOutput> {
        match input {
            ResumeInput::Continue => Ok(RuntimeResumeOutput::Resumed { record }),
            ResumeInput::Prompt(prompt) => self.resume_prompt(record, prompt).await,
            ResumeInput::Prompts(prompts) => self.resume_prompts(record, prompts).await,
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

    pub(crate) async fn rollback_to_turn(
        &self,
        id: &SessionId,
        turn: u64,
    ) -> Result<RuntimeRollbackOutput> {
        let record = self.load_session(id).await?;
        let after_cursor = record
            .timeline_events()
            .filter(|event| event.turn <= turn)
            .last()
            .map(|event| event.cursor);
        self.rollback_to_visible_cursor(record, after_cursor, turn.saturating_add(1))
            .await
    }

    pub(crate) async fn rollback_to_cursor(
        &self,
        id: &SessionId,
        cursor: TimelineCursor,
    ) -> Result<RuntimeRollbackOutput> {
        let record = self.load_session(id).await?;
        let Some(event) = record
            .timeline_events()
            .find(|event| event.cursor == cursor)
            .cloned()
        else {
            return Err(session_store_error(format!(
                "timeline cursor not found: {}",
                cursor.get()
            )));
        };
        self.rollback_to_visible_cursor(record, Some(event.cursor), event.turn.saturating_add(1))
            .await
    }

    pub(crate) async fn rollback_before_cursor(
        &self,
        id: &SessionId,
        cursor: TimelineCursor,
    ) -> Result<RuntimeRollbackOutput> {
        let record = self.load_session(id).await?;
        let mut after_cursor = None;
        let mut next_turn = 0;
        let mut found = false;
        for event in record.timeline_events() {
            if event.cursor == cursor {
                found = true;
                break;
            }
            after_cursor = Some(event.cursor);
            next_turn = event.turn.saturating_add(1);
        }
        if !found {
            return Err(session_store_error(format!(
                "timeline cursor not found: {}",
                cursor.get()
            )));
        }
        self.rollback_to_visible_cursor(record, after_cursor, next_turn)
            .await
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

    /// Atomically set a host-defined metadata value according to `condition`.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store cannot apply the write.
    pub async fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: String,
        value: Value,
        condition: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome> {
        self.store
            .set_metadata_value_conditionally(id, key, value, condition)
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

    /// Set the first-class inference settings for a durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn set_inference_settings(
        &self,
        id: &SessionId,
        inference_settings: Option<InferenceSettings>,
    ) -> Result<SessionRecord> {
        self.store
            .set_inference_settings(id, inference_settings)
            .await
            .map_err(session_store_error)
    }

    /// Set the first-class agent-definition bundle for a durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn set_agent_definition(
        &self,
        id: &SessionId,
        agent_definition: Option<serde_json::Value>,
    ) -> Result<SessionRecord> {
        self.store
            .set_agent_definition(id, agent_definition)
            .await
            .map_err(session_store_error)
    }

    pub(crate) async fn fail_run(
        &self,
        id: &SessionId,
        message: String,
        authentication_failure: Option<AuthenticationFailure>,
    ) -> Result<SessionRecord> {
        let record = self.load_session(id).await?;
        if matches!(
            record.snapshot().status,
            SessionStatus::Failed { .. } | SessionStatus::Suspended(_)
        ) {
            return Ok(record);
        }
        self.append_journal_and_reload(
            record,
            vec![SessionJournalEvent::Failed {
                message,
                authentication_failure,
            }],
        )
        .await
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
        let mut cancellation_timeline = Vec::new();
        let mut status_timeline = Vec::new();
        for work in pending_work(&snapshot) {
            if let Some(result) = interrupted_tool_result(work) {
                let parent_id = tool_call_item_id(&record, &result.call_id)?;
                cancellation_timeline.push(tool_result_timeline_event(
                    &record.id, work.turn, parent_id, result,
                ));
            }
            journal.push(SessionJournalEvent::WorkCanceled {
                work_id: work.id.clone(),
                reason: CancelReason::SessionInterrupted,
            });
        }
        journal.push(SessionJournalEvent::Interrupted);
        if open_prompt_turn(&snapshot).is_some() {
            journal.push(SessionJournalEvent::TurnInterrupted {
                turn: snapshot.turn,
            });
        }
        status_timeline.push(interrupted_status_timeline_event(&record.id, snapshot.turn));

        let mut record = if cancellation_timeline.is_empty() {
            record
        } else {
            self.append_timeline_and_reload(record, cancellation_timeline)
                .await?
        };
        record = self.append_journal_and_reload(record, journal).await?;
        record = self
            .append_timeline_and_reload(record, status_timeline)
            .await?;
        Ok(record)
    }

    async fn rollback_to_visible_cursor(
        &self,
        record: SessionRecord,
        after_cursor: Option<TimelineCursor>,
        next_turn: u64,
    ) -> Result<RuntimeRollbackOutput> {
        let journal_len = rollback_journal_len(&record, after_cursor)?;
        let target = SessionRollbackTarget {
            journal_len,
            after_cursor,
            next_turn,
            remove_prompt_index: None,
        };
        let after_cursor = target.after_cursor;
        let record = self
            .append_journal_and_reload(record, vec![SessionJournalEvent::Rollback { target }])
            .await?;
        Ok(RuntimeRollbackOutput {
            record,
            after_cursor,
        })
    }

    async fn resume_prompt(
        &self,
        record: SessionRecord,
        prompt: String,
    ) -> Result<RuntimeResumeOutput> {
        self.resume_prompts(record, vec![prompt]).await
    }

    async fn resume_prompts(
        &self,
        record: SessionRecord,
        prompts: Vec<String>,
    ) -> Result<RuntimeResumeOutput> {
        if prompts.is_empty() {
            return Err(session_store_error("prompt batch must not be empty"));
        }
        let snapshot = record.snapshot();
        let mut journal = Vec::new();
        let mut cancellation_timeline = Vec::new();
        let mut prompt_timeline = Vec::new();
        for work in pending_work(&snapshot) {
            if let Some(result) = interrupted_tool_result(work) {
                let parent_id = tool_call_item_id(&record, &result.call_id)?;
                cancellation_timeline.push(tool_result_timeline_event(
                    &record.id, work.turn, parent_id, result,
                ));
            }
            journal.push(SessionJournalEvent::WorkCanceled {
                work_id: work.id.clone(),
                reason: CancelReason::SupersededByUserPrompt,
            });
        }
        let turn = if open_prompt_turn(&snapshot).is_some() {
            journal.push(SessionJournalEvent::Interrupted);
            journal.push(SessionJournalEvent::TurnInterrupted {
                turn: snapshot.turn,
            });
            snapshot.turn + 1
        } else {
            snapshot.turn
        };
        let mut created_prompts = Vec::with_capacity(prompts.len());
        for (prompt_index, prompt) in prompts.into_iter().enumerate() {
            prompt_timeline.push(user_prompt_timeline_event(
                &record.id,
                turn,
                snapshot.prompts.len() + prompt_index,
                prompt.clone(),
            ));
            created_prompts.push((journal.len(), prompt.clone()));
            journal.push(SessionJournalEvent::PromptAppended { prompt });
        }

        let mut record = if cancellation_timeline.is_empty() {
            record
        } else {
            self.append_timeline_and_reload(record, cancellation_timeline)
                .await?
        };
        let journal_start = record.journal.len();
        for (journal_index, _) in &mut created_prompts {
            *journal_index = journal_start.saturating_add(*journal_index);
        }
        record = self.append_journal_and_reload(record, journal).await?;
        if !prompt_timeline.is_empty() {
            record = self
                .append_timeline_and_reload(record, prompt_timeline)
                .await?;
        }
        self.publish_user_message_events(&record, snapshot.prompts.len(), created_prompts);
        Ok(RuntimeResumeOutput::Resumed { record })
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
                .map(|result| {
                    let parent_id = tool_call_item_id(&record, &result.call_id)?;
                    Ok(tool_result_timeline_event(
                        &record.id, turn, parent_id, result,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            record = self.append_timeline_and_reload(record, events).await?;
            record = self
                .append_journal_and_reload(record, vec![SessionJournalEvent::TurnFinished { turn }])
                .await?;
        }
        Ok(RuntimeResumeOutput::Resumed { record })
    }

    async fn append_journal_and_reload(
        &self,
        mut record: SessionRecord,
        events: Vec<SessionJournalEvent>,
    ) -> Result<SessionRecord> {
        let id = record.id.clone();
        let receipt = self
            .store
            .append_journal(
                &id,
                record.execution_token().map_err(session_store_error)?,
                events.clone(),
            )
            .await
            .map_err(session_store_error)?;
        record.adopt_commit(receipt);
        record.append_journal_events(events);
        self.publish_session_summary(&record);
        Ok(record)
    }

    async fn append_timeline_and_reload(
        &self,
        mut record: SessionRecord,
        events: Vec<PendingTimelineEvent>,
    ) -> Result<SessionRecord> {
        let id = record.id.clone();
        let receipt = self
            .store
            .append_timeline(
                &id,
                record.execution_token().map_err(session_store_error)?,
                events,
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
        if let Some(context) = &self.run_context {
            context.timeline().publish_all(receipt.events);
        }
        self.publish_session_summary(&record);
        Ok(record)
    }

    fn publish_session_summary(&self, record: &SessionRecord) {
        if let (Some(context), Some(incarnation)) = (&self.run_context, self.session_incarnation) {
            context.publish_run_summary(incarnation, record.summary());
        }
    }

    fn publish_user_message_events(
        &self,
        record: &SessionRecord,
        first_prompt_index: usize,
        prompts: Vec<(usize, String)>,
    ) {
        let Some(sink) = self
            .run_context
            .as_ref()
            .and_then(SessionRunContext::event_sink)
            .or(self.event_sink.as_ref())
        else {
            return;
        };
        for (offset, (journal_index, text)) in prompts.into_iter().enumerate() {
            let prompt_index =
                u64::try_from(first_prompt_index.saturating_add(offset)).unwrap_or(u64::MAX);
            crate::agent::event::spawn_session_event(
                Arc::clone(sink),
                crate::SessionEvent::UserMessageCreated {
                    id: crate::agent::event::user_message_event_id(&record.id, journal_index),
                    session_id: record.id.clone(),
                    incarnation: record.metadata.incarnation.clone(),
                    prompt_index,
                    user_message_count: prompt_index.saturating_add(1),
                    text,
                    metadata: record.metadata.values.clone(),
                    revision: record.revision,
                    cursor: record.last_timeline_cursor(),
                },
            );
        }
    }
}

/// Result of applying a runtime resume operation.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeResumeOutput {
    /// Runtime accepted input and can continue building a provider request.
    Resumed { record: SessionRecord },
    /// Runtime remains suspended.
    Suspended {
        record: SessionRecord,
        suspension: Suspension,
    },
}

/// Result of applying an append-only runtime rollback barrier.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeRollbackOutput {
    /// Durable record after rollback replay state is applied.
    pub record: SessionRecord,
    /// Last visible cursor that remains after truncation.
    pub after_cursor: Option<TimelineCursor>,
}

fn rollback_journal_len(
    record: &SessionRecord,
    after_cursor: Option<TimelineCursor>,
) -> Result<u64> {
    let Some(after_cursor) = after_cursor else {
        return Ok(0);
    };
    let Some(index) = record.journal.iter().position(|event| {
        matches!(event, SessionJournalEvent::Timeline(event) if event.cursor == after_cursor)
    }) else {
        return Err(session_store_error(format!(
            "timeline cursor not found: {}",
            after_cursor.get()
        )));
    };
    u64::try_from(index.saturating_add(1)).map_err(session_store_error)
}

fn pending_work(snapshot: &SessionSnapshot) -> Vec<&PendingWork> {
    snapshot
        .pending_work
        .iter()
        .filter(|work| matches!(work.status, PendingWorkStatus::Pending))
        .collect()
}

fn interrupted_tool_result(work: &PendingWork) -> Option<ToolResult> {
    let call = work.tool_call.as_ref()?;
    Some(ToolResult::new(call.id.clone(), call.name.clone(), "interrupted").with_error())
}

fn tool_call_item_id(record: &SessionRecord, call_id: &str) -> Result<TimelineItemId> {
    record
        .timeline_events()
        .filter_map(|event| match &event.content {
            TimelineContent::ToolCall { call } if call.id == call_id => Some(event.item_id.clone()),
            _ => None,
        })
        .last()
        .ok_or_else(|| runtime_error(format!("timeline item missing for tool call: {call_id}")))
}

fn tool_result_timeline_event(
    session_id: &SessionId,
    turn: u64,
    parent_id: TimelineItemId,
    result: ToolResult,
) -> PendingTimelineEvent {
    PendingTimelineEvent::tool_result(
        session_id.clone(),
        turn,
        PendingTimelineItemId::New,
        parent_id,
        result,
    )
}

fn user_prompt_timeline_event(
    session_id: &SessionId,
    turn: u64,
    _prompt_index: usize,
    prompt: String,
) -> PendingTimelineEvent {
    PendingTimelineEvent::user_text_snapshot(
        session_id.clone(),
        turn,
        PendingTimelineItemId::New,
        prompt,
    )
}

fn open_prompt_turn(snapshot: &SessionSnapshot) -> Option<u64> {
    let prompt_turn = snapshot.prompt_turns.last()?;
    (prompt_turn.turn == snapshot.turn).then_some(prompt_turn.turn)
}

fn interrupted_status_timeline_event(session_id: &SessionId, turn: u64) -> PendingTimelineEvent {
    let mut event = PendingTimelineEvent::new(
        session_id.clone(),
        turn,
        PendingTimelineItemId::New,
        TimelineEventKind::SessionStatusChanged,
        None,
        TimelineContent::Empty,
    );
    event.status = Some(crate::timeline::TimelineStatus::Interrupted);
    event
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

    use super::*;
    use crate::agent::session::{MemorySessionStore, Revision};

    fn session_id() -> SessionId {
        SessionId::new("runtime-session")
    }

    #[tokio::test]
    async fn create_persists_empty_record() {
        let store = MemorySessionStore::new();
        let runtime = AgentSessionRuntime::new(&store);

        let record = runtime
            .create(SessionCreateRequest::new(session_id()))
            .await
            .unwrap();

        assert_eq!(record.id, session_id());
        assert_eq!(record.revision, Revision::ZERO);
        assert!(record.journal.is_empty());
    }

    #[tokio::test]
    async fn interrupt_is_idempotent() {
        let store = MemorySessionStore::new();
        let runtime = AgentSessionRuntime::new(&store);
        let id = runtime
            .create(SessionCreateRequest::new(session_id()))
            .await
            .unwrap()
            .id;
        runtime
            .resume(&id, ResumeInput::Prompt("active work".into()))
            .await
            .unwrap();

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
        assert_eq!(
            second
                .journal
                .iter()
                .filter(|event| matches!(event, SessionJournalEvent::TurnInterrupted { .. }))
                .count(),
            1
        );
    }
}
