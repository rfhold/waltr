//! Durable session runtime helpers.

#![allow(missing_docs)]

use crate::{LlmError, Result, ToolResult};

use super::{
    CancelReason, ExternalRequest, ExternalRequestId, ExternalRequestStatus, ExternalResponse,
    PendingWorkStatus, ResumeInput, SessionEvent, SessionId, SessionRecord, SessionSnapshot,
    SessionStore, Suspension,
};

/// Runtime facade for durable session resume operations.
#[derive(Debug)]
pub struct AgentSessionRuntime<'a, S>
where
    S: SessionStore + ?Sized,
{
    store: &'a S,
}

impl<'a, S> AgentSessionRuntime<'a, S>
where
    S: SessionStore + ?Sized,
{
    /// Create a runtime over a session store.
    pub const fn new(store: &'a S) -> Self {
        Self { store }
    }

    /// Create a new durable session record.
    pub async fn create_session(&self, id: SessionId) -> Result<SessionRecord> {
        let record = SessionRecord::new(id.clone());
        self.store
            .create(record)
            .await
            .map_err(session_store_error)?;
        self.load_session(&id).await
    }

    /// Load a durable session record.
    pub async fn load_session(&self, id: &SessionId) -> Result<SessionRecord> {
        self.store.load(id).await.map_err(session_store_error)
    }

    /// Apply resume input to a persisted session and return the durable checkpoint reached.
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
            ResumeInput::Interrupt => self.interrupt(record).await,
        }
    }

    async fn resume_prompt(
        &self,
        record: SessionRecord,
        prompt: String,
    ) -> Result<RuntimeResumeOutput> {
        let mut events = Vec::new();
        for request in pending_external_requests(&record.snapshot) {
            events.push(SessionEvent::ExternalRequestCanceled {
                request_id: request.id.clone(),
                reason: CancelReason::SupersededByUserPrompt,
            });
        }
        for work_id in pending_work_ids(&record.snapshot) {
            events.push(SessionEvent::WorkCanceled {
                work_id,
                reason: CancelReason::SupersededByUserPrompt,
            });
        }
        events.push(SessionEvent::PromptAppended { prompt });

        let record = self.append_and_snapshot(record, events).await?;
        Ok(RuntimeResumeOutput::Resumed { record })
    }

    async fn resume_external_response(
        &self,
        record: SessionRecord,
        request_id: ExternalRequestId,
        response: ExternalResponse,
    ) -> Result<RuntimeResumeOutput> {
        let request = validate_pending_external_response(&record.snapshot, &request_id)?;
        let mut events = vec![SessionEvent::ExternalRequestResponded {
            request_id: request_id.clone(),
            response: response.clone(),
        }];

        if let Some(tool_call_id) = request.tool_call_id {
            let turn = record.snapshot.turn;
            events.push(SessionEvent::ToolResultRecorded {
                turn,
                result: ToolResult::new(
                    tool_call_id,
                    request_tool_name(&request.kind),
                    external_response_content(&response),
                ),
            });
            // The external response completes the suspended tool call, so the
            // turn finishes here just like the inline tool execution path.
            events.push(SessionEvent::TurnFinished { turn });
        }

        let record = self.append_and_snapshot(record, events).await?;
        Ok(RuntimeResumeOutput::Resumed { record })
    }

    async fn resume_tool_results(
        &self,
        record: SessionRecord,
        results: Vec<ToolResult>,
    ) -> Result<RuntimeResumeOutput> {
        let turn = record.snapshot.turn;
        let mut events: Vec<SessionEvent> = results
            .into_iter()
            .map(|result| SessionEvent::ToolResultRecorded { turn, result })
            .collect();
        if !events.is_empty() {
            events.push(SessionEvent::TurnFinished { turn });
        }
        let record = self.append_and_snapshot(record, events).await?;
        Ok(RuntimeResumeOutput::Resumed { record })
    }

    async fn interrupt(&self, record: SessionRecord) -> Result<RuntimeResumeOutput> {
        let record = self
            .append_and_snapshot(record, vec![SessionEvent::Interrupted])
            .await?;
        Ok(RuntimeResumeOutput::Suspended {
            record,
            suspension: Suspension::Interrupted { work: Vec::new() },
        })
    }

    async fn append_and_snapshot(
        &self,
        record: SessionRecord,
        events: Vec<SessionEvent>,
    ) -> Result<SessionRecord> {
        let revision = self
            .store
            .append(&record.id, events, record.revision)
            .await
            .map_err(session_store_error)?;
        let mut record = self
            .store
            .load(&record.id)
            .await
            .map_err(session_store_error)?;
        let mut snapshot = SessionSnapshot::new(record.id.clone());
        snapshot.apply_events(&record.events);
        let revision = self
            .store
            .save_snapshot(&record.id, snapshot, revision)
            .await
            .map_err(session_store_error)?;
        record = self
            .store
            .load(&record.id)
            .await
            .map_err(session_store_error)?;
        debug_assert_eq!(record.revision, revision);
        Ok(record)
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

fn request_tool_name(kind: &super::ExternalRequestKind) -> &'static str {
    match kind {
        super::ExternalRequestKind::Question => "question",
        super::ExternalRequestKind::Permission => "permission",
        super::ExternalRequestKind::ToolInput
        | super::ExternalRequestKind::Confirmation
        | super::ExternalRequestKind::Custom(_) => "external_request",
    }
}

fn external_response_content(response: &ExternalResponse) -> String {
    if let Some(text) = response.payload.as_str() {
        text.to_owned()
    } else {
        response.payload.to_string()
    }
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
        store.create(SessionRecord::new(id.clone())).await.unwrap();
        store
            .append(
                &id,
                vec![
                    SessionEvent::PromptAppended {
                        prompt: "ask".to_owned(),
                    },
                    SessionEvent::ToolCallRecorded {
                        turn: 0,
                        call: ToolCall::new("call-question", "question", json!({})),
                    },
                    SessionEvent::ExternalRequestCreated {
                        request: external_request("question-1"),
                    },
                    SessionEvent::Suspended {
                        suspension: Suspension::AwaitingExternalInput {
                            request: external_request("question-1"),
                        },
                    },
                ],
                Revision::ZERO,
            )
            .await
            .unwrap();
        let record = store.load(&id).await.unwrap();
        let mut snapshot = SessionSnapshot::new(id.clone());
        snapshot.apply_events(&record.events);
        store
            .save_snapshot(&id, snapshot, record.revision)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn create_session_persists_empty_record() {
        let store = MemorySessionStore::new();
        let runtime = AgentSessionRuntime::new(&store);

        let record = runtime.create_session(session_id()).await.unwrap();

        assert_eq!(record.id, session_id());
        assert_eq!(record.revision, Revision::ZERO);
        assert!(record.events.is_empty());
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
        assert!(matches!(
            record.snapshot.external_requests[0].status,
            ExternalRequestStatus::Responded(_)
        ));
        let tool_results: Vec<_> = record.snapshot.tool_results().collect();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(tool_results[0].call_id, "call-question");
        assert_eq!(tool_results[0].name, "question");
        assert_eq!(record.snapshot.turns[0].turn, 0);
        assert_eq!(
            record.snapshot.turn, 1,
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
        assert_eq!(
            record.snapshot.current_prompt.as_deref(),
            Some("new prompt")
        );
        assert_eq!(
            record.snapshot.external_requests[0].status,
            ExternalRequestStatus::Canceled(CancelReason::SupersededByUserPrompt)
        );
        assert_eq!(
            record.snapshot.pending_work[0].status,
            PendingWorkStatus::Canceled(CancelReason::SupersededByUserPrompt)
        );
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
