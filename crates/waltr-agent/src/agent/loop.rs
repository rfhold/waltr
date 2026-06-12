//! Internal agent loop.

use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
};

use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, CompletionTurn, LlmError, LlmStream,
    ModelRef, PermissionClaim, PermissionLevel, PermissionPolicy, Result, ToolCall, ToolDefinition,
    ToolResult, TurnItem,
    middleware::{
        self as middleware, HookOutcome,
        chain::MiddlewareChain,
        context::{
            MiddlewareStateLog, ModelRequestDraft, RequestContext, RunContext, SystemPromptParts,
            ToolCallContext, ToolDraft, ToolResultContext,
        },
        tools::tool_is_visible,
    },
};

use super::{
    AgentEvent, AgentState, AgentTool, AgentTranscriptEvent,
    session::{
        AgentSessionRuntime, EphemeralSessionRun, ExternalRequest, ExternalRequestId,
        ExternalRequestKind, ExternalRequestStatus, RequestId, RequestRecord, ResumeInput,
        Revision, RuntimeResumeOutput, SessionEvent, SessionId, SessionRecord, SessionSnapshot,
        SessionStore, SessionStoreError, Suspension as SessionSuspension,
    },
};

pub(super) struct AgentLoopState<'a, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    provider: &'a P,
    model: ModelRef,
    prompt: String,
    input: Option<ResumeInput>,
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    turns: Vec<CompletionTurn>,
    state: AgentState,
    turn: usize,
    phase: AgentLoopPhase,
    pending: VecDeque<Result<AgentEvent>>,
    permission_policy: PermissionPolicy,
    session_id: SessionId,
    session_revision: Revision,
    session_store: S,
    session_initialized: bool,
    middleware_state: MiddlewareStateLog,
    system_prompt_override: Option<middleware::prompt::SystemPromptOverride>,
}

enum AgentLoopPhase {
    StartTurn,
    Streaming(BoxCompletionStream),
    Done,
}

type ResolvedToolCall = std::result::Result<(usize, ToolDefinition), ToolResult>;

enum PermissionOutcome {
    Allow,
    Ask(Vec<PermissionClaim>),
    Deny(String),
}

impl<'a, P, S> AgentLoopState<'a, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(super) fn from_ephemeral_session(run: EphemeralSessionRun<'a, P, S>) -> Self {
        let prompt = match &run.input {
            ResumeInput::Prompt(prompt) => prompt,
            ResumeInput::ToolResults(_)
            | ResumeInput::ExternalResponse { .. }
            | ResumeInput::Interrupt => "",
        }
        .to_owned();
        let permission_policy = run.permission_policy;
        Self {
            provider: run.provider,
            model: run.model,
            prompt,
            input: Some(run.input),
            tools: run.tools,
            middleware_chain: run.middleware_chain,
            turns: Vec::new(),
            state: AgentState::default(),
            turn: 0,
            phase: AgentLoopPhase::StartTurn,
            pending: VecDeque::new(),
            permission_policy,
            session_id: run.session_id,
            session_revision: Revision::ZERO,
            session_store: run.session_store,
            session_initialized: false,
            middleware_state: MiddlewareStateLog::default(),
            system_prompt_override: run.system_prompt_override,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn next(mut self) -> Option<(Result<AgentEvent>, Self)> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some((event, self));
            }

            match std::mem::replace(&mut self.phase, AgentLoopPhase::Done) {
                AgentLoopPhase::StartTurn => {
                    if let Err(error) = self.initialize_session().await {
                        return Some((Err(error), self));
                    }
                    // Crash recovery is retry-the-turn: the request is journaled before
                    // dispatch and parser progress is intentionally not persisted.
                    let draft = match self.build_provider_request().await {
                        Ok(draft) => draft,
                        Err(error) => return Some((Err(error), self)),
                    };
                    let request = completion_request_from_draft(&draft);
                    let projected = self.provider.project_request(&request);

                    if let Err(error) = self.persist_provider_request(&draft, projected).await {
                        return Some((Err(error), self));
                    }
                    if let Err(error) = self.save_session_snapshot().await {
                        return Some((Err(error), self));
                    }
                    match self.provider.stream_request(request).await {
                        Ok(stream) => self.phase = AgentLoopPhase::Streaming(stream),
                        Err(error) => {
                            self.pending_failed_session_event(&error).await;
                            return Some((Err(error), self));
                        }
                    }
                }
                AgentLoopPhase::Streaming(mut stream) => match stream.next().await {
                    Some(Ok(CompletionEvent::Delta(delta))) => {
                        self.state.events.push(AgentTranscriptEvent::Text {
                            turn: self.turn,
                            text: delta.clone(),
                        });
                        if let Err(error) = self
                            .append_session_event(SessionEvent::ProviderTextDelta {
                                turn: self.turn as u64,
                                delta: delta.clone(),
                            })
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Streaming(stream);
                        return Some((
                            Ok(AgentEvent::TextDelta {
                                turn: self.turn,
                                delta,
                            }),
                            self,
                        ));
                    }
                    Some(Ok(CompletionEvent::ThinkingDelta(delta))) => {
                        self.state.events.push(AgentTranscriptEvent::Thinking {
                            turn: self.turn,
                            text: delta.text.clone(),
                        });
                        if let Err(error) = self
                            .append_session_event(SessionEvent::ProviderThinkingDelta {
                                turn: self.turn as u64,
                                delta: delta.text.clone(),
                            })
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Streaming(stream);
                        return Some((
                            Ok(AgentEvent::ThinkingDelta {
                                turn: self.turn,
                                delta: delta.text,
                            }),
                            self,
                        ));
                    }
                    Some(Ok(CompletionEvent::ThinkingBlock(block))) => {
                        CompletionTurn::entry(&mut self.turns, self.turn as u64)
                            .items
                            .push(TurnItem::Thinking(block.clone()));
                        if let Err(error) = self
                            .append_session_event(SessionEvent::ThinkingBlockRecorded {
                                turn: self.turn as u64,
                                block,
                            })
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Streaming(stream);
                    }
                    Some(Ok(CompletionEvent::ProviderMetadata(metadata))) => {
                        CompletionTurn::entry(&mut self.turns, self.turn as u64).metadata =
                            Some(metadata.clone());
                        if let Err(error) = self
                            .append_session_event(SessionEvent::ProviderTurnMetadataRecorded {
                                turn: self.turn as u64,
                                metadata,
                            })
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Streaming(stream);
                    }
                    Some(Ok(CompletionEvent::ToolCall(call))) => {
                        self.state.events.push(AgentTranscriptEvent::ToolCall {
                            turn: self.turn,
                            call: call.clone(),
                        });
                        CompletionTurn::entry(&mut self.turns, self.turn as u64)
                            .items
                            .push(TurnItem::ToolCall(call.clone()));
                        if let Err(error) = self
                            .append_session_event(SessionEvent::ToolCallRecorded {
                                turn: self.turn as u64,
                                call: call.clone(),
                            })
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        let event = AgentEvent::ToolCall {
                            turn: self.turn,
                            call: call.clone(),
                        };
                        let Some(result) = self.execute_tool(&call).await else {
                            self.phase = AgentLoopPhase::Done;
                            return Some((Ok(event), self));
                        };
                        CompletionTurn::entry(&mut self.turns, self.turn as u64)
                            .items
                            .push(TurnItem::ToolResult(result.clone()));
                        self.state.events.push(AgentTranscriptEvent::ToolResult {
                            turn: self.turn,
                            result: result.clone(),
                        });
                        let mut events = self.middleware_state.drain_pending();
                        events.push(SessionEvent::ToolResultRecorded {
                            turn: self.turn as u64,
                            result: result.clone(),
                        });
                        if let Err(error) = self.append_session_events(events).await {
                            return Some((Err(error), self));
                        }
                        self.pending.push_back(Ok(AgentEvent::ToolResult {
                            turn: self.turn,
                            result,
                        }));
                        if let Err(error) = self
                            .append_session_event(SessionEvent::TurnFinished {
                                turn: self.turn as u64,
                            })
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        self.pending
                            .push_back(Ok(AgentEvent::TurnFinished { turn: self.turn }));
                        if let Err(error) = self.save_session_snapshot().await {
                            return Some((Err(error), self));
                        }
                        self.turn += 1;
                        self.phase = AgentLoopPhase::StartTurn;
                        return Some((Ok(event), self));
                    }
                    Some(Ok(CompletionEvent::Finished)) | None => {
                        let mut events = self.middleware_state.drain_pending();
                        events.push(SessionEvent::TurnFinished {
                            turn: self.turn as u64,
                        });
                        events.push(SessionEvent::Finished);
                        if let Err(error) = self.append_session_events(events).await {
                            return Some((Err(error), self));
                        }
                        self.pending
                            .push_back(Ok(AgentEvent::TurnFinished { turn: self.turn }));
                        let snapshot = match self.save_session_snapshot().await {
                            Ok(snapshot) => snapshot,
                            Err(error) => return Some((Err(error), self)),
                        };
                        self.pending
                            .push_back(Ok(AgentEvent::Finished { snapshot }));
                        self.phase = AgentLoopPhase::Done;
                    }
                    Some(Err(error)) => {
                        self.pending_failed_session_event(&error).await;
                        return Some((Err(error), self));
                    }
                },
                AgentLoopPhase::Done => return None,
            }
        }
    }

    async fn build_provider_request(&mut self) -> Result<ModelRequestDraft> {
        let mut ctx = RequestContext {
            session_id: self.session_id.as_str(),
            revision: self.session_revision.get(),
            model_id: &self.model.model,
            provider_id: Some(self.model.provider),
            turn: self.turn as u64,
            session: &self.state,
            middleware_state: &mut self.middleware_state,
            system_prompt_override: &mut self.system_prompt_override,
        };

        let mut prompt = SystemPromptParts::default();
        self.middleware_chain
            .build_system_prompt(&mut ctx, &mut prompt)
            .await?;

        let mut tools = ToolDraft::default();
        self.middleware_chain
            .list_tools(&mut ctx, &mut tools)
            .await?;
        self.middleware_chain
            .edit_tool_metadata(&mut ctx, &mut tools)
            .await?;

        let mut request = ModelRequestDraft {
            prompt: self.prompt.clone(),
            system_prompt: join_system_prompt(&prompt),
            tools: tools.tools,
            turns: self.turns.clone(),
        };

        match self
            .middleware_chain
            .before_provider_request(&mut ctx, &mut request)
            .await?
        {
            HookOutcome::Continue => Ok(request),
            HookOutcome::Abort(error) => Err(error),
            outcome => Err(unsupported_before_provider_request_outcome(&outcome)),
        }
    }

    fn resolve_tool_call(&self, call: &ToolCall) -> Option<ResolvedToolCall> {
        let tool_index = self
            .tools
            .iter()
            .position(|tool| tool.definition().name == call.name)?;
        let tool_definition = self.tools[tool_index].definition().clone();

        if !tool_is_visible(&self.permission_policy, &tool_definition, true) {
            return Some(Err(error_tool_result(
                call,
                &LlmError::Protocol {
                    provider: "permission",
                    message: format!("tool is not permitted: {}", call.name),
                },
            )));
        }

        Some(Ok((tool_index, tool_definition)))
    }

    async fn execute_tool(&mut self, call: &ToolCall) -> Option<ToolResult> {
        let mut call = call.clone();
        let resolved = match self.resolve_tool_call(&call) {
            Some(Ok(resolved)) => Some(resolved),
            Some(Err(result)) => return Some(result),
            None => None,
        };

        // Claims-producing middleware fills this during before_tool_call; permission
        // evaluation below runs after the full chain so it observes complete claims.
        let mut claims = Vec::new();
        let before_outcome = {
            let mut ctx = ToolCallContext {
                session_id: self.session_id.as_str(),
                revision: self.session_revision.get(),
                turn: self.turn as u64,
                session: &self.state,
                call: &mut call,
                tool_definition: resolved.as_ref().map(|(_, definition)| definition),
                permission_claims: &mut claims,
                middleware_state: &mut self.middleware_state,
            };
            self.middleware_chain.before_tool_call(&mut ctx).await
        };
        match before_outcome {
            Ok(HookOutcome::Continue) => {}
            Ok(outcome) => return self.tool_result_from_hook_outcome(&call, outcome).await,
            Err(error) => return Some(error_tool_result(&call, &error)),
        }

        let Some((tool_index, _)) = resolved else {
            return Some(error_tool_result(
                &call,
                &LlmError::Protocol {
                    provider: "agent",
                    message: format!("unknown tool: {}", call.name),
                },
            ));
        };

        match self.permission_outcome(&call, &claims) {
            PermissionOutcome::Allow => {}
            PermissionOutcome::Deny(message) => {
                return Some(error_tool_result(
                    &call,
                    &LlmError::Protocol {
                        provider: "permission",
                        message,
                    },
                ));
            }
            PermissionOutcome::Ask(claims) => {
                let outcome = HookOutcome::Suspend(permission_suspension(
                    &self.session_id,
                    self.turn as u64,
                    &call,
                    &claims,
                ));
                return self.tool_result_from_hook_outcome(&call, outcome).await;
            }
        }

        let mut result = match self.tools[tool_index].call(&call).await {
            Ok(content) => ToolResult::new(call.id.clone(), call.name.clone(), content),
            Err(error) => error_tool_result(&call, &error),
        };

        let after_outcome = {
            let mut ctx = ToolResultContext {
                session_id: self.session_id.as_str(),
                revision: self.session_revision.get(),
                turn: self.turn as u64,
                session: &self.state,
                call: Some(&call),
                result: &mut result,
                middleware_state: &mut self.middleware_state,
            };
            self.middleware_chain.after_tool_call(&mut ctx).await
        };
        match after_outcome {
            Ok(HookOutcome::Continue) => Some(result),
            Ok(outcome) => self.tool_result_from_hook_outcome(&call, outcome).await,
            Err(error) => Some(error_tool_result(&call, &error)),
        }
    }

    async fn tool_result_from_hook_outcome(
        &mut self,
        call: &ToolCall,
        outcome: HookOutcome,
    ) -> Option<ToolResult> {
        match outcome {
            HookOutcome::Continue => Some(ToolResult::new(call.id.clone(), call.name.clone(), "")),
            HookOutcome::ReplaceToolResult(result) => Some(result),
            HookOutcome::Abort(error) => Some(error_tool_result(call, &error)),
            HookOutcome::Suppress => Some(ToolResult::new(
                call.id.clone(),
                call.name.clone(),
                "Tool call suppressed by middleware.",
            )),
            HookOutcome::Suspend(mut suspension) => {
                let mut ctx = RunContext {
                    session_id: self.session_id.as_str(),
                    revision: self.session_revision.get(),
                    turn: self.turn as u64,
                    session: &self.state,
                    middleware_state: &mut self.middleware_state,
                };
                if let Err(error) = self
                    .middleware_chain
                    .on_suspend(&mut ctx, &mut suspension)
                    .await
                {
                    return Some(error_tool_result(call, &error));
                }
                if let Some(request) = suspension.external_request.clone() {
                    if let Err(error) = self.persist_external_request(&request).await {
                        return Some(error_tool_result(call, &error));
                    }
                    let session_request = external_request_from_draft(&request);
                    let mut events = self.middleware_state.drain_pending();
                    events.push(SessionEvent::Suspended {
                        suspension: SessionSuspension::AwaitingExternalInput {
                            request: Box::new(session_request),
                        },
                    });
                    if let Err(error) = self.append_session_events(events).await {
                        return Some(error_tool_result(call, &error));
                    }
                    if let Err(error) = self.save_session_snapshot().await {
                        return Some(error_tool_result(call, &error));
                    }
                    self.pending.push_back(Ok(AgentEvent::ExternalRequest {
                        turn: self.turn,
                        request,
                    }));
                }
                self.phase = AgentLoopPhase::Done;
                None
            }
        }
    }

    fn permission_outcome(
        &self,
        tool_call: &ToolCall,
        claims: &[PermissionClaim],
    ) -> PermissionOutcome {
        let denied = claims
            .iter()
            .filter(|claim| {
                self.permission_policy
                    .evaluate_claim_effect(&tool_call.name, claim)
                    .level
                    == PermissionLevel::Deny
            })
            .cloned()
            .collect::<Vec<_>>();
        if !denied.is_empty() {
            return PermissionOutcome::Deny(permission_claim_message("permission denied", &denied));
        }

        let ask = claims
            .iter()
            .filter(|claim| {
                self.permission_policy
                    .evaluate_claim_effect(&tool_call.name, claim)
                    .level
                    == PermissionLevel::Ask
            })
            .cloned()
            .collect::<Vec<_>>();
        if ask.is_empty() {
            PermissionOutcome::Allow
        } else {
            PermissionOutcome::Ask(ask)
        }
    }

    async fn initialize_session(&mut self) -> Result<()> {
        if self.session_initialized {
            return Ok(());
        }

        match self.session_store.load(&self.session_id).await {
            Ok(record) => {
                self.apply_resume_input(record).await?;
                self.session_initialized = true;
                return Ok(());
            }
            Err(SessionStoreError::NotFound { .. }) => {}
            Err(error) => return Err(session_store_error(error)),
        }

        let Some(input) = self.input.take() else {
            return Err(session_store_error("missing session input"));
        };
        let ResumeInput::Prompt(prompt) = input else {
            return Err(session_store_error(
                "cannot resume missing session without a prompt input",
            ));
        };
        self.prompt.clone_from(&prompt);
        self.session_store
            .create(SessionRecord::new(self.session_id.clone()))
            .await
            .map_err(session_store_error)?;
        self.session_revision = self
            .session_store
            .append(
                &self.session_id,
                vec![SessionEvent::PromptAppended { prompt }],
                self.session_revision,
            )
            .await
            .map_err(session_store_error)?;
        self.session_initialized = true;
        Ok(())
    }

    async fn apply_resume_input(&mut self, record: SessionRecord) -> Result<()> {
        let Some(input) = self.input.take() else {
            self.hydrate_from_record(record);
            return Ok(());
        };

        let record = match input {
            ResumeInput::Prompt(prompt) => {
                if record.snapshot.current_prompt.as_deref() == Some(prompt.as_str())
                    && record.snapshot.prompts.len() == 1
                    && record.snapshot.turns.is_empty()
                {
                    record
                } else {
                    match AgentSessionRuntime::new(&self.session_store)
                        .resume(&self.session_id, ResumeInput::Prompt(prompt))
                        .await?
                    {
                        RuntimeResumeOutput::Resumed { record }
                        | RuntimeResumeOutput::Suspended { record, .. } => record,
                    }
                }
            }
            ResumeInput::ExternalResponse {
                request_id,
                response,
            } => match AgentSessionRuntime::new(&self.session_store)
                .resume(
                    &self.session_id,
                    ResumeInput::ExternalResponse {
                        request_id,
                        response,
                    },
                )
                .await?
            {
                RuntimeResumeOutput::Resumed { record }
                | RuntimeResumeOutput::Suspended { record, .. } => record,
            },
            ResumeInput::ToolResults(results) => {
                match AgentSessionRuntime::new(&self.session_store)
                    .resume(&self.session_id, ResumeInput::ToolResults(results))
                    .await?
                {
                    RuntimeResumeOutput::Resumed { record }
                    | RuntimeResumeOutput::Suspended { record, .. } => record,
                }
            }
            ResumeInput::Interrupt => match AgentSessionRuntime::new(&self.session_store)
                .resume(&self.session_id, ResumeInput::Interrupt)
                .await?
            {
                RuntimeResumeOutput::Resumed { record }
                | RuntimeResumeOutput::Suspended { record, .. } => record,
            },
        };

        self.hydrate_from_record(record);
        Ok(())
    }

    fn hydrate_from_record(&mut self, record: SessionRecord) {
        let snapshot = record.snapshot;
        self.middleware_state = MiddlewareStateLog::from_snapshot(&snapshot);
        self.prompt = snapshot.current_prompt.unwrap_or_default();
        self.turns = snapshot.turns;
        self.state = snapshot.state;
        self.turn = usize::try_from(snapshot.turn).unwrap_or(usize::MAX);
        self.session_revision = record.revision;
    }

    async fn append_session_event(&mut self, event: SessionEvent) -> Result<()> {
        self.append_session_events(vec![event]).await
    }

    async fn append_session_events(&mut self, events: Vec<SessionEvent>) -> Result<()> {
        self.session_revision = self
            .session_store
            .append(&self.session_id, events, self.session_revision)
            .await
            .map_err(session_store_error)?;
        Ok(())
    }

    // Returns the snapshot exactly as persisted: stores stamp the new revision on save.
    async fn save_session_snapshot(&mut self) -> Result<SessionSnapshot> {
        let record = self
            .session_store
            .load(&self.session_id)
            .await
            .map_err(session_store_error)?;
        let mut snapshot = SessionSnapshot::new(record.id.clone());
        snapshot.apply_events(&record.events);
        self.session_revision = self
            .session_store
            .save_snapshot(&self.session_id, snapshot.clone(), self.session_revision)
            .await
            .map_err(session_store_error)?;
        snapshot.revision = self.session_revision;
        Ok(snapshot)
    }

    async fn pending_failed_session_event(&mut self, error: &LlmError) {
        if self.session_initialized
            && self
                .append_session_event(SessionEvent::Failed {
                    message: error.to_string(),
                })
                .await
                .is_ok()
        {
            drop(self.save_session_snapshot().await);
        }
    }

    async fn persist_provider_request(
        &mut self,
        draft: &ModelRequestDraft,
        projected: Option<Value>,
    ) -> Result<()> {
        let canonical_request = canonical_request_json(draft);
        let request = RequestRecord {
            id: RequestId::new(format!(
                "{}-request-{}",
                self.session_id.as_str(),
                self.turn
            )),
            turn: self.turn as u64,
            model: self.model.clone(),
            canonical_request_hash: stable_request_hash(&canonical_request),
            projected_request: projected.unwrap_or_else(|| canonical_request.clone()),
            canonical_request,
        };

        // State writes that shaped this request must be durable before the request record.
        let events = self.middleware_state.drain_pending();
        if !events.is_empty() {
            self.append_session_events(events).await?;
        }
        self.session_revision = self
            .session_store
            .record_request(&self.session_id, request.clone(), self.session_revision)
            .await
            .map_err(session_store_error)?;
        self.append_session_event(SessionEvent::ProviderRequestRecorded { request })
            .await
    }

    async fn persist_external_request(
        &mut self,
        request: &middleware::ExternalRequestDraft,
    ) -> Result<()> {
        let request = ExternalRequest {
            id: ExternalRequestId::new(request.id.clone()),
            kind: session_external_request_kind(&request.kind),
            tool_call_id: request.tool_call_id.clone(),
            prompt: request.prompt.clone(),
            payload: request.payload.clone(),
            status: session_external_request_status(&request.status),
        };

        let events = self.middleware_state.drain_pending();
        if !events.is_empty() {
            self.append_session_events(events).await?;
        }
        self.session_revision = self
            .session_store
            .record_external_request(&self.session_id, request.clone(), self.session_revision)
            .await
            .map_err(session_store_error)?;
        self.append_session_event(SessionEvent::ExternalRequestCreated { request })
            .await?;
        Ok(())
    }
}

fn external_request_from_draft(request: &middleware::ExternalRequestDraft) -> ExternalRequest {
    ExternalRequest {
        id: ExternalRequestId::new(request.id.clone()),
        kind: session_external_request_kind(&request.kind),
        tool_call_id: request.tool_call_id.clone(),
        prompt: request.prompt.clone(),
        payload: request.payload.clone(),
        status: session_external_request_status(&request.status),
    }
}

fn canonical_request_json(draft: &ModelRequestDraft) -> Value {
    json!({
        "prompt": draft.prompt,
        "system_prompt": draft.system_prompt,
        "tools": draft.tools.iter().map(tool_json).collect::<Vec<_>>(),
        "turns": draft.turns.iter().map(turn_json).collect::<Vec<_>>(),
    })
}

fn turn_json(turn: &CompletionTurn) -> Value {
    let mut value = json!({
        "turn": turn.turn,
        "items": turn.items.iter().map(turn_item_json).collect::<Vec<_>>(),
    });
    if let Some(metadata) = &turn.metadata {
        value["metadata"] = serde_json::to_value(metadata).unwrap_or(Value::Null);
    }
    value
}

fn turn_item_json(item: &TurnItem) -> Value {
    match item {
        TurnItem::Thinking(block) => json!({
            "thinking": {
                "kind": format!("{:?}", block.kind),
                "text": block.text,
                "payload": block.payload,
            }
        }),
        TurnItem::ToolCall(call) => json!({ "tool_call": tool_call_json(call) }),
        TurnItem::ToolResult(result) => json!({ "tool_result": tool_result_json(result) }),
    }
}

fn tool_json(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn tool_call_json(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "name": call.name,
        "arguments": call.arguments,
    })
}

fn tool_result_json(result: &ToolResult) -> Value {
    json!({
        "call_id": result.call_id,
        "name": result.name,
        "content": result.content,
    })
}

fn stable_request_hash(canonical_request: &Value) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical_request.to_string().hash(&mut hasher);
    format!("compat-{:016x}", hasher.finish())
}

fn session_external_request_kind(kind: &middleware::ExternalRequestKind) -> ExternalRequestKind {
    match kind {
        middleware::ExternalRequestKind::Permission => ExternalRequestKind::Permission,
        middleware::ExternalRequestKind::Question => ExternalRequestKind::Question,
        middleware::ExternalRequestKind::Confirmation => ExternalRequestKind::Confirmation,
        middleware::ExternalRequestKind::Custom(value) => {
            ExternalRequestKind::Custom(value.clone())
        }
    }
}

fn session_external_request_status(
    status: &middleware::ExternalRequestStatus,
) -> ExternalRequestStatus {
    match status {
        middleware::ExternalRequestStatus::Pending => ExternalRequestStatus::Pending,
        middleware::ExternalRequestStatus::Answered => {
            ExternalRequestStatus::Responded(super::session::ExternalResponse {
                payload: json!({ "answered": true }),
            })
        }
        middleware::ExternalRequestStatus::Canceled => {
            ExternalRequestStatus::Canceled(super::session::CancelReason::SessionInterrupted)
        }
    }
}

fn permission_suspension(
    session_id: &SessionId,
    turn: u64,
    call: &ToolCall,
    claims: &[PermissionClaim],
) -> middleware::Suspension {
    let summary = permission_claim_message("permission required", claims);
    let payload = json!({
        "tool_call": {
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        },
        "claims": claims
            .iter()
            .map(|claim| {
                json!({
                    "operation": format!("{:?}", claim.operation),
                    "resource": format!("{:?}", claim.resource),
                    "summary": claim.summary,
                })
            })
            .collect::<Vec<_>>(),
    });
    middleware::Suspension::external(
        "permission",
        middleware::ExternalRequestDraft {
            id: format!("permission:{}:{}:{}", session_id.as_str(), turn, call.id),
            kind: middleware::ExternalRequestKind::Permission,
            tool_call_id: Some(call.id.clone()),
            prompt: summary,
            payload,
            status: middleware::ExternalRequestStatus::Pending,
        },
    )
}

fn permission_claim_message(prefix: &str, claims: &[PermissionClaim]) -> String {
    let summaries = claims
        .iter()
        .map(|claim| claim.summary.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    if summaries.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}: {summaries}")
    }
}

fn session_store_error(error: impl std::fmt::Display) -> LlmError {
    LlmError::Protocol {
        provider: "session",
        message: error.to_string(),
    }
}

fn join_system_prompt(prompt: &SystemPromptParts) -> Option<String> {
    if prompt.parts.is_empty() {
        None
    } else {
        Some(prompt.parts.join("\n\n"))
    }
}

fn completion_request_from_draft(draft: &ModelRequestDraft) -> CompletionRequest<'_> {
    CompletionRequest {
        prompt: &draft.prompt,
        system_prompt: draft.system_prompt.as_deref(),
        tools: draft.tools.clone(),
        turns: draft.turns.clone(),
    }
}

fn unsupported_before_provider_request_outcome(outcome: &HookOutcome) -> LlmError {
    LlmError::Protocol {
        provider: "middleware",
        message: format!(
            "unsupported before_provider_request middleware outcome during agent loop request construction: {outcome:?}"
        ),
    }
}

fn error_tool_result(call: &ToolCall, error: &LlmError) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        call.name.clone(),
        format!("Tool error: {error}\nArguments: {}", call.arguments),
    )
}
