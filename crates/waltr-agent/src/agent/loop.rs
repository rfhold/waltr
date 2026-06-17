//! Internal agent loop.

use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
    sync::Arc,
};

use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::permission::{PERMISSION_STATE_NAMESPACE, PermissionRuntimeState};
use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, CompletionTurn, LlmError, LlmStream,
    ModelRef, PendingTimelineEvent, PermissionClaim, PermissionLevel, PermissionPolicy,
    PermissionResponse, Result, TimelineChannel, TimelineContent, TimelineEventKind,
    TimelineItemId, ToolCall, ToolDefinition, ToolExecutionContext, ToolProgressSink, ToolResult,
    TurnItem,
    middleware::{
        self as middleware, HookOutcome,
        chain::MiddlewareChain,
        context::{
            MiddlewareStateLog, ModelRequestDraft, RequestContext, RunContext, SystemPromptParts,
            ToolCallContext, ToolDraft, ToolResultContext,
        },
        tools::tool_is_visible,
    },
    timeline::{AgentTimelineEvent, CHANNEL_EXTERNAL_REQUEST, TimelineStatus},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::tool::ToolFuture;
use super::{
    AgentTool,
    session::{
        AgentSessionRuntime, EphemeralSessionRun, ExternalRequest, ExternalRequestId,
        ExternalRequestKind, ExternalRequestStatus, ExternalResponse, RequestId, RequestRecord,
        ResumeInput, Revision, RuntimeResumeOutput, SessionId, SessionJournalEvent, SessionRecord,
        SessionSnapshot, SessionStore, SessionStoreError, Suspension as SessionSuspension,
    },
};

pub(super) struct AgentLoopState<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    provider: Arc<dyn LlmStream>,
    model: ModelRef,
    prompt: String,
    input: Option<ResumeInput>,
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    turns: Vec<CompletionTurn>,
    turn: usize,
    phase: AgentLoopPhase,
    pending: VecDeque<Result<AgentTimelineEvent>>,
    permission_policy: PermissionPolicy,
    session_id: SessionId,
    session_revision: Revision,
    session_store: S,
    session_initialized: bool,
    middleware_state: MiddlewareStateLog,
    system_prompt_override: Option<middleware::prompt::SystemPromptOverride>,
    cancellation_token: CancellationToken,
}

enum AgentLoopPhase {
    StartTurn,
    Streaming(BoxCompletionStream),
    ExecuteTool(ToolCall),
    RunningTool(RunningTool),
    Done,
}

struct RunningTool {
    call: ToolCall,
    result: ToolFuture,
    progress_rx: Option<mpsc::UnboundedReceiver<PendingTimelineEvent>>,
}

type ResolvedToolCall = std::result::Result<(usize, ToolDefinition), ToolResult>;

enum PermissionOutcome {
    Allow,
    Ask(Vec<PermissionClaim>),
    Deny(String),
}

enum ToolExecution {
    Running(RunningTool),
    Finished(Option<ToolResult>),
}

impl<S> AgentLoopState<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(super) fn from_ephemeral_session(run: EphemeralSessionRun<S>) -> Self {
        let prompt = match &run.input {
            ResumeInput::Prompt(prompt) => prompt,
            ResumeInput::ToolResults(_) | ResumeInput::ExternalResponse { .. } => "",
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
            cancellation_token: run.cancellation_token,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn next(mut self) -> Option<(Result<AgentTimelineEvent>, Self)> {
        loop {
            if self.cancellation_token.is_cancelled() {
                if let Err(error) = self.record_interrupted().await {
                    return Some((Err(error), self));
                }
                self.phase = AgentLoopPhase::Done;
                return None;
            }

            if let Some(event) = self.pending.pop_front() {
                return Some((event, self));
            }

            match std::mem::replace(&mut self.phase, AgentLoopPhase::Done) {
                AgentLoopPhase::StartTurn => {
                    match self.initialize_session().await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => return Some((Err(error), self)),
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
                    let stream_result = tokio::select! {
                        biased;

                        () = self.cancellation_token.cancelled() => {
                            if let Err(error) = self.record_interrupted().await {
                                return Some((Err(error), self));
                            }
                            self.phase = AgentLoopPhase::Done;
                            return None;
                        }
                        result = self.provider.stream_request(request) => result,
                    };
                    match stream_result {
                        Ok(stream) => self.phase = AgentLoopPhase::Streaming(stream),
                        Err(error) => {
                            self.pending_failed_session_event(&error).await;
                            return Some((Err(error), self));
                        }
                    }
                }
                AgentLoopPhase::Streaming(mut stream) => match tokio::select! {
                    biased;

                    () = self.cancellation_token.cancelled() => {
                        if let Err(error) = self.record_interrupted().await {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Done;
                        return None;
                    }
                    event = stream.next() => event,
                } {
                    Some(Ok(CompletionEvent::Delta(delta))) => {
                        let event = match self.append_assistant_delta(delta.clone()).await {
                            Ok(event) => event,
                            Err(error) => return Some((Err(error), self)),
                        };
                        CompletionTurn::entry(&mut self.turns, self.turn as u64)
                            .append_assistant_text(&delta);
                        self.phase = AgentLoopPhase::Streaming(stream);
                        return Some((Ok(event), self));
                    }
                    Some(Ok(CompletionEvent::ThinkingDelta(delta))) => {
                        let event = match self.append_thinking_delta(delta.text.clone()).await {
                            Ok(event) => event,
                            Err(error) => return Some((Err(error), self)),
                        };
                        self.phase = AgentLoopPhase::Streaming(stream);
                        return Some((Ok(event), self));
                    }
                    Some(Ok(CompletionEvent::ThinkingBlock(block))) => {
                        CompletionTurn::entry(&mut self.turns, self.turn as u64)
                            .items
                            .push(TurnItem::Thinking(block.clone()));
                        if let Err(error) = self
                            .append_journal_event(
                                SessionJournalEvent::ProviderThinkingBlockRecorded {
                                    turn: self.turn as u64,
                                    block,
                                },
                            )
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
                            .append_journal_event(
                                SessionJournalEvent::ProviderTurnMetadataRecorded {
                                    turn: self.turn as u64,
                                    metadata,
                                },
                            )
                            .await
                        {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Streaming(stream);
                    }
                    Some(Ok(CompletionEvent::ToolCall(call))) => {
                        CompletionTurn::entry(&mut self.turns, self.turn as u64)
                            .items
                            .push(TurnItem::ToolCall(call.clone()));
                        let event = match self
                            .append_timeline_event(tool_call_timeline_event(
                                &self.session_id,
                                self.turn as u64,
                                call.clone(),
                            ))
                            .await
                        {
                            Ok(event) => event,
                            Err(error) => return Some((Err(error), self)),
                        };
                        self.phase = AgentLoopPhase::ExecuteTool(call);
                        return Some((Ok(event), self));
                    }
                    Some(Ok(CompletionEvent::Finished)) | None => {
                        let mut events = self.middleware_state.drain_pending();
                        events.push(SessionJournalEvent::TurnFinished {
                            turn: self.turn as u64,
                        });
                        events.push(SessionJournalEvent::Finished);
                        if let Err(error) = self.append_journal_events(events).await {
                            return Some((Err(error), self));
                        }
                        let status = match self.append_finished_status_event().await {
                            Ok(event) => event,
                            Err(error) => return Some((Err(error), self)),
                        };
                        self.pending.push_back(Ok(status));
                        self.phase = AgentLoopPhase::Done;
                    }
                    Some(Err(error)) => {
                        self.pending_failed_session_event(&error).await;
                        return Some((Err(error), self));
                    }
                },
                AgentLoopPhase::ExecuteTool(call) => match self.start_tool(call).await {
                    Ok(ToolExecution::Running(running)) => {
                        self.phase = AgentLoopPhase::RunningTool(running);
                        continue;
                    }
                    Ok(ToolExecution::Finished(Some(result))) => {
                        if let Err(error) = self.finish_tool_turn(result).await {
                            return Some((Err(error), self));
                        }
                        continue;
                    }
                    Ok(ToolExecution::Finished(None)) => continue,
                    Err(error) => return Some((Err(error), self)),
                },
                AgentLoopPhase::RunningTool(mut running) => {
                    if let Some(rx) = running.progress_rx.as_mut() {
                        tokio::select! {
                            biased;

                            progress = rx.recv() => {
                                match progress {
                                    Some(progress) => {
                                        let event = match self.append_timeline_event(progress).await {
                                            Ok(event) => event,
                                            Err(error) => return Some((Err(error), self)),
                                        };
                                        self.phase = AgentLoopPhase::RunningTool(running);
                                        return Some((Ok(event), self));
                                    }
                                    None => {
                                        running.progress_rx = None;
                                        self.phase = AgentLoopPhase::RunningTool(running);
                                        continue;
                                    }
                                }
                            }
                            result = &mut running.result => {
                                if let Some(progress_rx) = running.progress_rx.as_mut() {
                                    match self.append_timeline_events(drain_progress_events(progress_rx)).await {
                                        Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
                                        Err(error) => return Some((Err(error), self)),
                                    }
                                }
                                if let Err(error) = self.finish_running_tool(running.call, result).await {
                                    return Some((Err(error), self));
                                }
                                continue;
                            }
                            () = self.cancellation_token.cancelled() => {
                                if let Err(error) = self.record_interrupted().await {
                                    return Some((Err(error), self));
                                }
                                self.phase = AgentLoopPhase::Done;
                                return None;
                            }
                        }
                    } else {
                        let result = tokio::select! {
                            biased;

                            () = self.cancellation_token.cancelled() => {
                                if let Err(error) = self.record_interrupted().await {
                                    return Some((Err(error), self));
                                }
                                self.phase = AgentLoopPhase::Done;
                                return None;
                            }
                            result = running.result => result,
                        };
                        if let Err(error) = self.finish_running_tool(running.call, result).await {
                            return Some((Err(error), self));
                        }
                        continue;
                    }
                }
                AgentLoopPhase::Done => return None,
            }
        }
    }

    async fn build_provider_request(&mut self) -> Result<ModelRequestDraft> {
        let mut ctx = RequestContext {
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

    async fn start_tool(&mut self, mut call: ToolCall) -> Result<ToolExecution> {
        let resolved = match self.resolve_tool_call(&call) {
            Some(Ok(resolved)) => Some(resolved),
            Some(Err(result)) => return Ok(ToolExecution::Finished(Some(result))),
            None => None,
        };

        // Claims-producing middleware fills this during before_tool_call; permission
        // evaluation below runs after the full chain so it observes complete claims.
        let mut claims = Vec::new();
        let before_outcome = {
            let mut ctx = ToolCallContext {
                session_id: self.session_id.as_str(),
                turn: self.turn as u64,
                call: &mut call,
                permission_claims: &mut claims,
                middleware_state: &mut self.middleware_state,
            };
            self.middleware_chain.before_tool_call(&mut ctx).await
        };
        match before_outcome {
            Ok(HookOutcome::Continue) => {}
            Ok(outcome) => {
                return self
                    .tool_result_from_hook_outcome(&call, outcome)
                    .await
                    .map(ToolExecution::Finished);
            }
            Err(error) => {
                return Ok(ToolExecution::Finished(Some(error_tool_result(
                    &call, &error,
                ))));
            }
        }

        let Some((tool_index, _)) = resolved else {
            return Ok(ToolExecution::Finished(Some(error_tool_result(
                &call,
                &LlmError::Protocol {
                    provider: "agent",
                    message: format!("unknown tool: {}", call.name),
                },
            ))));
        };

        match self.permission_outcome(&call, &claims) {
            Ok(PermissionOutcome::Allow) => {
                self.flush_middleware_state().await?;
            }
            Ok(PermissionOutcome::Deny(message)) => {
                return Ok(ToolExecution::Finished(Some(error_tool_result(
                    &call,
                    &LlmError::Protocol {
                        provider: "permission",
                        message,
                    },
                ))));
            }
            Ok(PermissionOutcome::Ask(claims)) => {
                let outcome = HookOutcome::Suspend(permission_suspension(
                    &self.session_id,
                    self.turn as u64,
                    &call,
                    &claims,
                ));
                return self
                    .tool_result_from_hook_outcome(&call, outcome)
                    .await
                    .map(ToolExecution::Finished);
            }
            Err(error) => {
                return Ok(ToolExecution::Finished(Some(error_tool_result(
                    &call, &error,
                ))));
            }
        }

        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let progress = ToolProgressSink::timeline(
            self.session_id.clone(),
            self.turn as u64,
            tool_call_item_id(&call.id),
            move |event| {
                drop(progress_tx.send(event));
                Ok(())
            },
        );
        let result =
            self.tools[tool_index].call(ToolExecutionContext::with_progress_and_cancellation(
                call.clone(),
                progress,
                self.cancellation_token.clone(),
            ));
        Ok(ToolExecution::Running(RunningTool {
            call,
            result,
            progress_rx: Some(progress_rx),
        }))
    }

    async fn finish_running_tool(
        &mut self,
        call: ToolCall,
        result: Result<ToolResult>,
    ) -> Result<()> {
        let mut result = match result {
            Ok(result) => result,
            Err(error) => error_tool_result(&call, &error),
        };

        let after_outcome = {
            let mut ctx = ToolResultContext {
                call: Some(&call),
                result: &mut result,
                middleware_state: &mut self.middleware_state,
            };
            self.middleware_chain.after_tool_call(&mut ctx).await
        };
        match after_outcome {
            Ok(HookOutcome::Continue) => self.finish_tool_turn(result).await,
            Ok(outcome) => {
                if let Some(result) = self.tool_result_from_hook_outcome(&call, outcome).await? {
                    self.finish_tool_turn(result).await?;
                }
                Ok(())
            }
            Err(error) => {
                self.finish_tool_turn(error_tool_result(&call, &error))
                    .await
            }
        }
    }

    async fn finish_tool_turn(&mut self, result: ToolResult) -> Result<()> {
        CompletionTurn::entry(&mut self.turns, self.turn as u64)
            .items
            .push(TurnItem::ToolResult(result.clone()));
        let events = self.middleware_state.drain_pending();
        self.append_journal_events(events).await?;
        let event = self
            .append_timeline_event(tool_result_timeline_event(
                &self.session_id,
                self.turn as u64,
                result,
            ))
            .await?;
        self.append_journal_event(SessionJournalEvent::TurnFinished {
            turn: self.turn as u64,
        })
        .await?;
        self.turn += 1;
        self.phase = AgentLoopPhase::StartTurn;
        self.pending.push_back(Ok(event));
        Ok(())
    }

    async fn tool_result_from_hook_outcome(
        &mut self,
        call: &ToolCall,
        outcome: HookOutcome,
    ) -> Result<Option<ToolResult>> {
        match outcome {
            HookOutcome::Continue => Ok(Some(ToolResult::new(
                call.id.clone(),
                call.name.clone(),
                "",
            ))),
            HookOutcome::ReplaceToolResult(result) => Ok(Some(result)),
            HookOutcome::Suspend(mut suspension) => {
                let mut ctx = RunContext {
                    middleware_state: &mut self.middleware_state,
                };
                if let Err(error) = self
                    .middleware_chain
                    .on_suspend(&mut ctx, &mut suspension)
                    .await
                {
                    return Ok(Some(error_tool_result(call, &error)));
                }
                if let Some(request) = suspension.external_request.clone() {
                    let event = match self.persist_external_request(&request).await {
                        Ok(event) => event,
                        Err(error) => return Ok(Some(error_tool_result(call, &error))),
                    };
                    let session_request = external_request_from_draft(&request);
                    let mut events = self.middleware_state.drain_pending();
                    events.push(SessionJournalEvent::Suspended {
                        suspension: SessionSuspension::AwaitingExternalInput {
                            request: Box::new(session_request),
                        },
                    });
                    if let Err(error) = self.append_journal_events(events).await {
                        return Ok(Some(error_tool_result(call, &error)));
                    }
                    self.pending.push_back(Ok(event));
                }
                self.phase = AgentLoopPhase::Done;
                Ok(None)
            }
        }
    }

    fn permission_outcome(
        &mut self,
        tool_call: &ToolCall,
        claims: &[PermissionClaim],
    ) -> Result<PermissionOutcome> {
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
            return Ok(PermissionOutcome::Deny(permission_claim_message(
                "permission denied",
                &denied,
            )));
        }

        let mut state = self.permission_runtime_state()?;
        let unmatched = claims
            .iter()
            .filter(|claim| !state.patterns.iter().any(|pattern| pattern.matches(claim)))
            .cloned()
            .collect::<Vec<_>>();
        if unmatched.is_empty() {
            return Ok(PermissionOutcome::Allow);
        }

        if state.consume_one_shot(&tool_call.id) {
            self.set_permission_runtime_state(&state)?;
            return Ok(PermissionOutcome::Allow);
        }

        let ask = unmatched
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
            Ok(PermissionOutcome::Allow)
        } else {
            Ok(PermissionOutcome::Ask(ask))
        }
    }

    fn permission_runtime_state(&self) -> Result<PermissionRuntimeState> {
        self.middleware_state
            .get(PERMISSION_STATE_NAMESPACE)
            .map(|state| serde_json::from_value(state.clone()))
            .transpose()
            .map_err(|error| session_store_error(error.to_string()))
            .map(Option::unwrap_or_default)
    }

    fn set_permission_runtime_state(&mut self, state: &PermissionRuntimeState) -> Result<()> {
        self.middleware_state.set(
            PERMISSION_STATE_NAMESPACE,
            serde_json::to_value(state).map_err(|error| session_store_error(error.to_string()))?,
        );
        Ok(())
    }

    async fn initialize_session(&mut self) -> Result<bool> {
        if self.session_initialized {
            return Ok(true);
        }

        match self.session_store.load(&self.session_id).await {
            Ok(record) => {
                let continue_start_turn = self.apply_resume_input(record).await?;
                self.session_initialized = true;
                return Ok(continue_start_turn);
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
            .create(self.session_id.clone())
            .await
            .map_err(session_store_error)?;
        self.append_journal_event(SessionJournalEvent::PromptAppended { prompt })
            .await?;
        self.session_initialized = true;
        Ok(true)
    }

    async fn apply_resume_input(&mut self, record: SessionRecord) -> Result<bool> {
        let Some(input) = self.input.take() else {
            self.hydrate_from_record(record);
            return Ok(true);
        };

        let mut continue_start_turn = true;
        let record = match input {
            ResumeInput::Prompt(prompt) => {
                let snapshot = record.snapshot();
                if snapshot.current_prompt.as_deref() == Some(prompt.as_str())
                    && snapshot.prompts.len() == 1
                    && snapshot.turns.is_empty()
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
            } => {
                let retry = permission_retry_tool_call(&record.snapshot(), &request_id, &response)?;
                let cancel =
                    permission_cancel_response(&record.snapshot(), &request_id, &response)?;
                let record = match AgentSessionRuntime::new(&self.session_store)
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
                };
                if let Some(call) = retry {
                    self.phase = AgentLoopPhase::ExecuteTool(call);
                    continue_start_turn = false;
                } else if cancel {
                    self.phase = AgentLoopPhase::Done;
                    continue_start_turn = false;
                }
                record
            }
            ResumeInput::ToolResults(results) => {
                match AgentSessionRuntime::new(&self.session_store)
                    .resume(&self.session_id, ResumeInput::ToolResults(results))
                    .await?
                {
                    RuntimeResumeOutput::Resumed { record }
                    | RuntimeResumeOutput::Suspended { record, .. } => record,
                }
            }
        };

        self.hydrate_from_record(record);
        Ok(continue_start_turn)
    }

    fn hydrate_from_record(&mut self, record: SessionRecord) {
        let snapshot = record.snapshot();
        self.middleware_state = MiddlewareStateLog::from_snapshot(&snapshot);
        self.prompt = snapshot.current_prompt.unwrap_or_default();
        self.turns = snapshot.turns;
        self.turn = usize::try_from(snapshot.turn).unwrap_or(usize::MAX);
        self.session_revision = record.revision;
    }

    async fn append_journal_event(&mut self, event: SessionJournalEvent) -> Result<()> {
        self.append_journal_events(vec![event]).await
    }

    async fn append_journal_events(&mut self, events: Vec<SessionJournalEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.session_revision = self
            .session_store
            .append_journal(&self.session_id, self.session_revision, events)
            .await
            .map_err(session_store_error)?;
        Ok(())
    }

    async fn flush_middleware_state(&mut self) -> Result<()> {
        let events = self.middleware_state.drain_pending();
        self.append_journal_events(events).await
    }

    async fn record_interrupted(&mut self) -> Result<()> {
        if !self.session_initialized {
            return Ok(());
        }
        let record = AgentSessionRuntime::new(&self.session_store)
            .interrupt(&self.session_id)
            .await?;
        self.hydrate_from_record(record);
        Ok(())
    }

    async fn append_timeline_event(
        &mut self,
        event: PendingTimelineEvent,
    ) -> Result<AgentTimelineEvent> {
        let mut events = self.append_timeline_events(vec![event]).await?;
        events
            .pop()
            .ok_or_else(|| session_store_error("timeline append returned no assigned event"))
    }

    async fn append_timeline_events(
        &mut self,
        events: Vec<PendingTimelineEvent>,
    ) -> Result<Vec<AgentTimelineEvent>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let assigned = self
            .session_store
            .append_timeline(&self.session_id, self.session_revision, events)
            .await
            .map_err(session_store_error)?;
        self.session_revision = self.session_revision.next();
        Ok(assigned)
    }

    async fn append_assistant_delta(&mut self, delta: String) -> Result<AgentTimelineEvent> {
        self.append_timeline_event(PendingTimelineEvent::assistant_text_delta(
            self.session_id.clone(),
            self.turn as u64,
            assistant_text_item_id(self.turn as u64),
            delta,
        ))
        .await
    }

    async fn append_thinking_delta(&mut self, delta: String) -> Result<AgentTimelineEvent> {
        self.append_timeline_event(PendingTimelineEvent::thinking_delta(
            self.session_id.clone(),
            self.turn as u64,
            thinking_item_id(self.turn as u64),
            delta,
        ))
        .await
    }

    async fn append_finished_status_event(&mut self) -> Result<AgentTimelineEvent> {
        let mut event = PendingTimelineEvent::new(
            self.session_id.clone(),
            self.turn as u64,
            session_status_item_id(),
            TimelineEventKind::SessionStatusChanged,
            None,
            TimelineContent::Empty,
        );
        event.status = Some(TimelineStatus::Finished);
        self.append_timeline_event(event).await
    }

    async fn pending_failed_session_event(&mut self, error: &LlmError) {
        if self.session_initialized {
            drop(
                self.append_journal_event(SessionJournalEvent::Failed {
                    message: error.to_string(),
                })
                .await,
            );
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
            self.append_journal_events(events).await?;
        }
        self.append_journal_event(SessionJournalEvent::ProviderRequestRecorded { request })
            .await
    }

    async fn persist_external_request(
        &mut self,
        request: &middleware::ExternalRequestDraft,
    ) -> Result<AgentTimelineEvent> {
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
            self.append_journal_events(events).await?;
        }
        self.append_timeline_event(external_request_timeline_event(
            &self.session_id,
            self.turn as u64,
            request,
            TimelineEventKind::ExternalRequestOpened,
        ))
        .await
    }
}

fn assistant_text_item_id(turn: u64) -> TimelineItemId {
    TimelineItemId::new(format!("assistant-turn-{turn}"))
}

fn thinking_item_id(turn: u64) -> TimelineItemId {
    TimelineItemId::new(format!("thinking-turn-{turn}"))
}

fn tool_call_item_id(call_id: &str) -> TimelineItemId {
    TimelineItemId::new(format!("tool-call:{call_id}"))
}

fn tool_result_item_id(call_id: &str) -> TimelineItemId {
    TimelineItemId::new(format!("tool-result:{call_id}"))
}

fn external_request_item_id(request_id: &ExternalRequestId) -> TimelineItemId {
    TimelineItemId::new(format!("external-request:{}", request_id.as_str()))
}

fn drain_progress_events(
    progress_rx: &mut mpsc::UnboundedReceiver<PendingTimelineEvent>,
) -> Vec<PendingTimelineEvent> {
    let mut events = Vec::new();
    while let Ok(event) = progress_rx.try_recv() {
        events.push(event);
    }
    events
}

fn session_status_item_id() -> TimelineItemId {
    TimelineItemId::new("session-status")
}

fn tool_call_timeline_event(
    session_id: &SessionId,
    turn: u64,
    call: ToolCall,
) -> PendingTimelineEvent {
    PendingTimelineEvent::tool_call(session_id.clone(), turn, tool_call_item_id(&call.id), call)
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
        tool_result_item_id(&call_id),
        tool_call_item_id(&call_id),
        result,
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
        external_request_item_id(&request.id),
        kind,
        Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
        TimelineContent::ExternalRequest { request },
    )
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
        TurnItem::AssistantText(text) => json!({ "assistant_text": text }),
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

fn permission_retry_tool_call(
    snapshot: &SessionSnapshot,
    request_id: &ExternalRequestId,
    response: &ExternalResponse,
) -> Result<Option<ToolCall>> {
    let Some(request) = snapshot
        .external_requests
        .iter()
        .rev()
        .find(|request| request.id == *request_id)
    else {
        return Ok(None);
    };
    if request.kind != ExternalRequestKind::Permission {
        return Ok(None);
    }
    let response = PermissionResponse::from_value(response.payload.clone())?;
    if !matches!(
        response,
        PermissionResponse::AllowOnce | PermissionResponse::AllowAlways { .. }
    ) {
        return Ok(None);
    }
    let Some(tool_call_id) = request.tool_call_id.as_deref() else {
        return Err(session_store_error(
            "permission request missing tool call id",
        ));
    };
    snapshot
        .pending_work
        .iter()
        .rev()
        .find(|work| work.id == tool_call_id)
        .and_then(|work| work.tool_call.clone())
        .map(Some)
        .ok_or_else(|| {
            session_store_error(format!(
                "pending permission work not found for tool call: {tool_call_id}"
            ))
        })
}

fn permission_cancel_response(
    _snapshot: &SessionSnapshot,
    _request_id: &ExternalRequestId,
    response: &ExternalResponse,
) -> Result<bool> {
    match PermissionResponse::from_value(response.payload.clone()) {
        Ok(PermissionResponse::Deny {
            mode: crate::PermissionDenyMode::CancelWork,
            ..
        }) => Ok(true),
        Ok(_) | Err(_) => Ok(false),
    }
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
