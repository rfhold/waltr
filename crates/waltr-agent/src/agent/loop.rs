//! Internal agent loop.

#![allow(clippy::significant_drop_tightening)]

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt::Write as _,
    hash::{Hash, Hasher},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, CompletionTurn, LlmError, LlmStream,
    MAX_TOOL_RESULT_IMAGE_BYTES, ModelRef, PendingTimelineEvent, PendingTimelineItemId, PromptTurn,
    Result, TimelineChannel, TimelineContent, TimelineEventKind, TimelineItemId, ToolCall,
    ToolDefinition, ToolExecutionContext, ToolProgressSink, ToolResult, ToolResultContentBlock,
    TurnItem,
    middleware::{
        self as middleware, HookOutcome,
        chain::MiddlewareChain,
        context::{
            MiddlewareStateLog, ModelRequestDraft, RequestContext, SystemPromptParts, ToolDraft,
            ToolResultContext,
        },
    },
    timeline::{AgentTimelineEvent, CHANNEL_METRICS, TimelineStatus},
};
use tokio::{sync::mpsc, time::Sleep};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, Span};

use super::completion::{
    CompletionDecision, CompletionProposal, CompletionToolEvidence, CompletionValidation,
    MAX_COMPLETION_FEEDBACK_BYTES, validation_error, wait_deadline,
};
use super::{
    AgentTool,
    session::{
        AgentSessionRuntime, EphemeralSessionRun, PendingWorkStatus, RequestId, RequestRecord,
        ResumeInput, Revision, RuntimeResumeOutput, SessionContextMode, SessionCreateRequest,
        SessionId, SessionIncarnation, SessionJournalEvent, SessionOperation, SessionRecord,
        SessionRollbackTarget, SessionRunContext, SessionStart, SessionStartMessage, SessionStatus,
        SessionStore, SessionStoreError, Suspension as SessionSuspension,
    },
};
use super::{ProviderRetryConfig, tool::ToolFuture};

pub(super) struct AgentLoopState<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    provider: Arc<dyn LlmStream>,
    model: ModelRef,
    model_cost: Option<crate::ModelCost>,
    prompt: String,
    prompt_turns: Vec<PromptTurn>,
    input: Option<ResumeInput>,
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    turns: Vec<CompletionTurn>,
    turn: usize,
    phase: AgentLoopPhase,
    pending: VecDeque<Result<AgentTimelineEvent>>,
    retry_config: ProviderRetryConfig,
    provider_attempt: u32,
    retry_checkpoint: Option<RetryCheckpoint>,
    session_id: SessionId,
    session_revision: Revision,
    session_execution_token: Option<crate::ExecutionToken>,
    session_journal_len: u64,
    last_timeline_cursor: Option<crate::timeline::TimelineCursor>,
    session_store: S,
    session_record: Option<SessionRecord>,
    session_run_context: Option<SessionRunContext>,
    session_incarnation: Option<SessionIncarnation>,
    session_initialized: bool,
    template_submission_journal_len: Option<usize>,
    session_metadata_values: BTreeMap<String, Value>,
    middleware_state: MiddlewareStateLog,
    host_effects: Option<Arc<super::host_effect::HostEffectJournal<S>>>,
    system_prompt_override: Option<middleware::prompt::SystemPromptOverride>,
    cancellation_token: CancellationToken,
    max_provider_turns: Option<u64>,
    host_context: Option<Arc<dyn crate::AgentHostContext>>,
    provider_turns_dispatched: u64,
    turn_span: Option<Span>,
    assistant_item_id: Option<(u64, TimelineItemId)>,
    thinking_item_id: Option<(u64, TimelineItemId)>,
    tool_item_ids: HashMap<String, TimelineItemId>,
    event_sink: Option<Arc<dyn crate::SessionEventSink>>,
    completion_validation: Option<CompletionValidation>,
    completion_evidence: Vec<CompletionToolEvidence>,
    completion_evidence_bytes: usize,
    completion_effective_calls: std::collections::BTreeMap<String, ToolCall>,
    direct_tool: bool,
}

#[derive(Debug, Clone)]
struct RetryCheckpoint {
    journal_len: u64,
    after_cursor: Option<crate::timeline::TimelineCursor>,
    turn: u64,
}

const PENDING_USER_TEMPLATE: &str = "pending_user_template";

#[derive(Serialize, Deserialize)]
struct PendingUserTemplate {
    prompts: Vec<String>,
    prompt_count: usize,
    submission: u64,
    #[serde(default)]
    denied: bool,
}

enum AgentLoopPhase {
    StartTurn,
    Streaming(StreamingTurn),
    RetryDelay(Pin<Box<Sleep>>),
    ExecuteTools(ToolBatch),
    RunningTool(RunningTool, ToolBatch),
    Done,
}

struct StreamingTurn {
    stream: BoxCompletionStream,
    tool_calls: Vec<ToolCall>,
}

struct ToolBatch {
    calls: VecDeque<ToolCall>,
}

struct RunningTool {
    result_metadata: super::tool::ToolResultMetadata,
    call: ToolCall,
    result: ToolFuture,
    progress_rx: Option<mpsc::UnboundedReceiver<PendingTimelineEvent>>,
    host_dispatched: bool,
}

type ResolvedToolCall = (usize, ToolDefinition);

enum ToolExecution {
    Running(RunningTool),
    Finished(Option<ToolResult>),
}

impl<S> AgentLoopState<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(super) async fn direct_tool_stream(
        run: EphemeralSessionRun<S>,
        call: ToolCall,
    ) -> Result<super::BoxAgentTimelineStream<'static>> {
        if call.name.is_empty()
            || call.id.is_empty()
            || call.id.len() > 256
            || serde_json::to_vec(&call.arguments)
                .map_err(|_| validation_error("invalid host tool arguments"))?
                .len()
                > 65_536
        {
            return Err(validation_error("invalid host tool request"));
        }
        let mut state = Self::from_ephemeral_session(run);
        if !state.initialize_session().await? {
            return Err(validation_error("host tool session unavailable"));
        }
        if state
            .middleware_state
            .get("host-tool-execution")
            .and_then(|value| value.get("active"))
            .and_then(Value::as_bool)
            == Some(true)
            || state
                .turns
                .iter()
                .flat_map(|turn| &turn.items)
                .any(|item| matches!(item, TurnItem::ToolCall(prior) if prior.id == call.id))
        {
            return Err(validation_error(
                "host tool call already recorded; never replay a dispatch",
            ));
        }
        state.direct_tool = true;
        state.middleware_state.set(
            "host-tool-execution",
            json!({"active": true, "callId": call.id}),
        );
        state.flush_middleware_state().await?;
        CompletionTurn::entry(&mut state.turns, state.turn as u64)
            .items
            .push(TurnItem::ToolCall(call.clone()));
        let event = state
            .append_timeline_event(tool_call_timeline_event(
                &state.session_id,
                state.turn as u64,
                call.clone(),
            ))
            .await?;
        state
            .tool_item_ids
            .insert(call.id.clone(), event.item_id.clone());
        state.pending.push_back(Ok(event));
        state.phase = AgentLoopPhase::ExecuteTools(ToolBatch {
            calls: vec![call].into(),
        });
        Ok(Box::pin(futures_util::stream::unfold(state, Self::next)))
    }

    pub(super) fn from_ephemeral_session(run: EphemeralSessionRun<S>) -> Self {
        let prompt = match &run.input {
            ResumeInput::Prompt(prompt) => prompt,
            ResumeInput::Prompts(prompts) => prompts.last().map_or("", String::as_str),
            ResumeInput::Continue | ResumeInput::ToolResults(_) => "",
        }
        .to_owned();
        Self {
            provider: run.provider,
            model: run.model,
            model_cost: run.model_cost,
            prompt,
            prompt_turns: Vec::new(),
            input: Some(run.input),
            tools: run.tools,
            middleware_chain: run.middleware_chain,
            turns: Vec::new(),
            turn: 0,
            phase: AgentLoopPhase::StartTurn,
            pending: VecDeque::new(),
            retry_config: run.retry_config,
            provider_attempt: 1,
            retry_checkpoint: None,
            session_id: run.session_id,
            session_revision: Revision::ZERO,
            session_execution_token: None,
            session_journal_len: 0,
            last_timeline_cursor: None,
            session_store: run.session_store,
            session_record: None,
            session_run_context: run.session_run_context,
            session_incarnation: None,
            session_initialized: false,
            template_submission_journal_len: None,
            session_metadata_values: BTreeMap::new(),
            middleware_state: MiddlewareStateLog::default(),
            host_effects: None,
            system_prompt_override: run.system_prompt_override,
            cancellation_token: run.cancellation_token,

            max_provider_turns: run.max_provider_turns,
            host_context: run.host_context,
            provider_turns_dispatched: 0,
            turn_span: None,
            assistant_item_id: None,
            thinking_item_id: None,
            tool_item_ids: HashMap::new(),
            event_sink: run.event_sink,
            completion_validation: run.completion_validation,
            completion_evidence: Vec::new(),
            completion_evidence_bytes: 0,
            completion_effective_calls: std::collections::BTreeMap::new(),
            direct_tool: false,
        }
    }

    pub(super) async fn next(self) -> Option<(Result<AgentTimelineEvent>, Self)> {
        let (result, mut state) = self.next_event().await?;
        if let Err(error) = &result {
            tracing::error!(session_id = %state.session_id.as_str(), %error, "agent run failed");
            if let Err(persistence_error) = state.record_run_failure(error).await {
                tracing::error!(session_id = %state.session_id.as_str(), %error, %persistence_error, "failed to persist agent run failure");
            }
            state.pending.clear();
            state.phase = AgentLoopPhase::Done;
        }
        Some((result, state))
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::needless_continue, clippy::single_match_else)]
    async fn next_event(mut self) -> Option<(Result<AgentTimelineEvent>, Self)> {
        macro_rules! bounded {
            ($future:expr) => {{
                let deadline = self
                    .completion_validation
                    .as_ref()
                    .map(|validation| validation.deadline);
                let cancellation = self.cancellation_token.clone();
                let result = if deadline.is_some() {
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => None,
                        () = wait_deadline(deadline) => None,
                        result = $future => Some(result),
                    }
                } else {
                    Some($future.await)
                };
                match result {
                    Some(result) => result,
                    None => {
                        return Some((
                            Err(validation_error(
                                "helper cancelled or total deadline exceeded",
                            )),
                            self,
                        ));
                    }
                }
            }};
        }
        loop {
            if self
                .completion_validation
                .as_ref()
                .is_some_and(|validation| tokio::time::Instant::now() >= validation.deadline)
            {
                return Some((Err(validation_error("total deadline exceeded")), self));
            }
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
                    self.ensure_turn_span();
                    match bounded!(self.initialize_session()) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => return Some((Err(error), self)),
                    }
                    if !self.pending.is_empty() {
                        self.phase = AgentLoopPhase::StartTurn;
                        continue;
                    }
                    if self.provider_attempt == 1
                        && self
                            .max_provider_turns
                            .is_some_and(|limit| self.provider_turns_dispatched >= limit)
                    {
                        match self.record_provider_turn_limit_failure().await {
                            Ok(event) => {
                                self.phase = AgentLoopPhase::Done;
                                return Some((Ok(event), self));
                            }
                            Err(error) => return Some((Err(error), self)),
                        }
                    }
                    // Crash recovery is retry-the-turn: the request is journaled before
                    // dispatch and parser progress is intentionally not persisted.
                    let draft = match bounded!(self.build_provider_request()) {
                        Ok(Some(draft)) => draft,
                        Ok(None) => continue,
                        Err(error) => return Some((Err(error), self)),
                    };
                    let request = completion_request_from_draft(&draft);
                    let projected = self.provider.project_request(&request);
                    let retry_checkpoint = self.retry_checkpoint();

                    if let Err(error) = self.persist_provider_request(&draft, projected).await {
                        return Some((Err(error), self));
                    }
                    if self.provider_attempt == 1 {
                        self.provider_turns_dispatched =
                            self.provider_turns_dispatched.saturating_add(1);
                    }
                    self.retry_checkpoint = Some(retry_checkpoint);
                    let provider_span = {
                        let _guard = self.turn_span.as_ref().map(Span::enter);
                        tracing::info_span!(
                            "agent.provider_request",
                            session_id = %self.session_id.as_str(),
                            turn = self.turn as u64,
                            provider = self.provider.name(),
                            model_provider = ?self.model.provider,
                            model_api = ?self.model.api,
                            model = %self.model.model.as_str(),
                            tool_count = request.tools.len(),
                            turn_count = request.turns.len(),
                        )
                    };
                    self.in_turn_span(|| tracing::info!("provider request started"));
                    let stream_result = tokio::select! {
                        biased;

                        () = self.cancellation_token.cancelled() => {
                            if let Err(error) = self.record_interrupted().await {
                                return Some((Err(error), self));
                            }
                            self.phase = AgentLoopPhase::Done;
                            return None;
                        }
                        result = self.provider.stream_request(request).instrument(provider_span) => result,
                        () = wait_deadline(self.completion_validation.as_ref().map(|validation| validation.deadline)) => return Some((Err(validation_error("total deadline exceeded")), self)),
                    };
                    match stream_result {
                        Ok(stream) => {
                            self.in_turn_span(|| tracing::info!("provider stream opened"));
                            self.phase = AgentLoopPhase::Streaming(StreamingTurn {
                                stream,
                                tool_calls: Vec::new(),
                            });
                        }
                        Err(error) => {
                            self.in_turn_span(|| tracing::warn!(%error, "provider request failed"));
                            if let Err(error) = self.retry_or_fail_provider_error(error).await {
                                return Some((Err(error), self));
                            }
                            continue;
                        }
                    }
                }
                AgentLoopPhase::Streaming(mut streaming) => {
                    let next = tokio::select! {
                    biased;

                    () = self.cancellation_token.cancelled() => {
                        if let Err(error) = self.record_interrupted().await {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Done;
                        return None;
                    }
                    event = streaming.stream.next() => event,
                    () = wait_deadline(self.completion_validation.as_ref().map(|validation| validation.deadline)) => return Some((Err(validation_error("total deadline exceeded")), self)),
                    };
                    match next {
                        Some(Ok(CompletionEvent::Delta(delta))) => {
                            let event = match self.append_assistant_delta(delta.clone()).await {
                                Ok(event) => event,
                                Err(error) => return Some((Err(error), self)),
                            };
                            CompletionTurn::entry(&mut self.turns, self.turn as u64)
                                .append_assistant_text(&delta);
                            self.phase = AgentLoopPhase::Streaming(streaming);
                            return Some((Ok(event), self));
                        }
                        Some(Ok(CompletionEvent::ThinkingDelta(delta))) => {
                            let event = match self.append_thinking_delta(delta.text.clone()).await {
                                Ok(event) => event,
                                Err(error) => return Some((Err(error), self)),
                            };
                            self.phase = AgentLoopPhase::Streaming(streaming);
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
                            self.phase = AgentLoopPhase::Streaming(streaming);
                        }
                        Some(Ok(CompletionEvent::ProviderMetadata(mut metadata))) => {
                            if metadata.cost.is_none() {
                                metadata.cost = metadata.usage.and_then(|usage| {
                                    self.model_cost.as_ref()?.cost_for_usage(usage)
                                });
                            }
                            let event = provider_metrics_timeline_event(
                                &self.session_id,
                                self.turn as u64,
                                &metadata,
                            );
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
                            let event = match self.append_timeline_event(event).await {
                                Ok(event) => event,
                                Err(error) => return Some((Err(error), self)),
                            };
                            self.phase = AgentLoopPhase::Streaming(streaming);
                            return Some((Ok(event), self));
                        }
                        Some(Ok(CompletionEvent::ToolCall(call))) => {
                            self.in_turn_span(|| tracing::info!(tool_name = %call.name, tool_call_id = %call.id, "tool call requested"));
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
                            self.tool_item_ids
                                .insert(call.id.clone(), event.item_id.clone());
                            streaming.tool_calls.push(call);
                            self.phase = AgentLoopPhase::Streaming(streaming);
                            return Some((Ok(event), self));
                        }
                        Some(Ok(CompletionEvent::Finished)) | None => {
                            self.provider_attempt = 1;
                            self.retry_checkpoint = None;
                            if !streaming.tool_calls.is_empty() {
                                self.in_turn_span(|| {
                                    tracing::info!(
                                        tool_count = streaming.tool_calls.len(),
                                        "provider turn requested tools"
                                    );
                                });
                                self.phase = AgentLoopPhase::ExecuteTools(ToolBatch {
                                    calls: streaming.tool_calls.into(),
                                });
                                continue;
                            }
                            match bounded!(self.validate_completion()) {
                                Ok(true) => {}
                                Ok(false) => continue,
                                Err(error) => return Some((Err(error), self)),
                            }
                            if self.provider.transient_input().is_some() {
                                self.middleware_state.set(
                                    "transient-input",
                                    json!({"active": false, "replayable": false}),
                                );
                            }
                            match self.append_final_assistant_snapshot().await {
                                Ok(Some(event)) => self.pending.push_back(Ok(event)),
                                Ok(None) => {}
                                Err(error) => return Some((Err(error), self)),
                            }
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
                            self.close_turn_span("finished");
                            self.phase = AgentLoopPhase::Done;
                        }
                        Some(Err(error)) => {
                            self.in_turn_span(|| tracing::warn!(%error, "provider stream failed"));
                            if let Err(error) = self.retry_or_fail_provider_error(error).await {
                                return Some((Err(error), self));
                            }
                            continue;
                        }
                    }
                }
                AgentLoopPhase::RetryDelay(mut sleep) => {
                    tokio::select! {
                        biased;

                        () = self.cancellation_token.cancelled() => {
                            if let Err(error) = self.record_interrupted().await {
                                return Some((Err(error), self));
                            }
                            self.phase = AgentLoopPhase::Done;
                            return None;
                        }
                        () = &mut sleep => {
                            self.phase = AgentLoopPhase::StartTurn;
                        }
                        () = wait_deadline(self.completion_validation.as_ref().map(|validation| validation.deadline)) => return Some((Err(validation_error("total deadline exceeded")), self)),
                    }
                }
                AgentLoopPhase::ExecuteTools(mut batch) => {
                    let Some(call) = batch.calls.pop_front() else {
                        if let Err(error) = bounded!(self.finish_tool_batch()) {
                            return Some((Err(error), self));
                        }
                        continue;
                    };
                    let execution = bounded!(self.start_tool(call));
                    if self.cancellation_token.is_cancelled() {
                        if let Err(error) = self.record_interrupted().await {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Done;
                        return None;
                    }
                    match execution {
                        Ok(ToolExecution::Running(running)) => {
                            self.phase = AgentLoopPhase::RunningTool(running, batch);
                            continue;
                        }
                        Ok(ToolExecution::Finished(Some(result))) => {
                            if let Err(error) = bounded!(self.finish_tool_result(result)) {
                                return Some((Err(error), self));
                            }
                            self.phase = AgentLoopPhase::ExecuteTools(batch);
                            continue;
                        }
                        Ok(ToolExecution::Finished(None)) => continue,
                        Err(error) => return Some((Err(error), self)),
                    }
                }
                AgentLoopPhase::RunningTool(mut running, batch) => {
                    if let Some(rx) = running.progress_rx.as_mut() {
                        tokio::select! {
                            biased;

                            () = self.cancellation_token.cancelled() => {
                                if let Err(error) = self.record_interrupted().await {
                                    return Some((Err(error), self));
                                }
                                self.phase = AgentLoopPhase::Done;
                                return None;
                            }
                            progress = rx.recv() => {
                                match progress {
                                    Some(progress) => {
                                        let event = match self.append_timeline_event(progress).await {
                                            Ok(event) => event,
                                            Err(error) => return Some((Err(error), self)),
                                        };
                                        self.phase = AgentLoopPhase::RunningTool(running, batch);
                                        return Some((Ok(event), self));
                                    }
                                    None => {
                                        running.progress_rx = None;
                                        self.phase = AgentLoopPhase::RunningTool(running, batch);
                                        continue;
                                    }
                                }
                            }
                            () = wait_deadline(self.completion_validation.as_ref().map(|validation| validation.deadline)) => return Some((Err(validation_error("total deadline exceeded")), self)),
                            result = &mut running.result => {
                                if let Some(progress_rx) = running.progress_rx.as_mut() {
                                    match self.append_timeline_events(drain_progress_events(progress_rx)).await {
                                        Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
                                        Err(error) => return Some((Err(error), self)),
                                    }
                                }
                                 match bounded!(self.finish_running_tool(running.call, result, running.host_dispatched, running.result_metadata)) {
                                    Ok(true) => self.phase = AgentLoopPhase::ExecuteTools(batch),
                                    Ok(false) => {}
                                    Err(error) => return Some((Err(error), self)),
                                }
                                continue;
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
                            () = wait_deadline(self.completion_validation.as_ref().map(|validation| validation.deadline)) => return Some((Err(validation_error("total deadline exceeded")), self)),
                        };
                        match self
                            .finish_running_tool(
                                running.call,
                                result,
                                running.host_dispatched,
                                running.result_metadata,
                            )
                            .await
                        {
                            Ok(true) => self.phase = AgentLoopPhase::ExecuteTools(batch),
                            Ok(false) => {}
                            Err(error) => return Some((Err(error), self)),
                        }
                        continue;
                    }
                }
                AgentLoopPhase::Done => return None,
            }
        }
    }

    async fn build_provider_request(&mut self) -> Result<Option<ModelRequestDraft>> {
        self.prepare_host_effects().await?;
        let prompt_turns = self.prompt_turns();
        let middleware_chain = self.middleware_chain.clone();
        let mut ctx = RequestContext {
            middleware_state: &mut self.middleware_state,
            session_metadata_values: self.session_metadata_values.clone(),
            system_prompt_override: &mut self.system_prompt_override,
            cancellation_token: self.cancellation_token.clone(),
            session_id: self.session_id.as_str(),
            turn: self.turn as u64,
        };

        let mut prompt = SystemPromptParts::default();
        match middleware_chain
            .build_system_prompt(&mut ctx, &mut prompt)
            .await?
        {
            HookOutcome::Continue => {}
            #[cfg(test)]
            outcome @ HookOutcome::ReplaceToolResult(_) => {
                return Err(unsupported_build_system_prompt_outcome(&outcome));
            }
        }

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
            prompt_turns,
            turns: self.turns.clone(),
        };

        match self
            .middleware_chain
            .before_provider_request(&mut ctx, &mut request)
            .await?
        {
            HookOutcome::Continue => Ok(Some(request)),
            #[cfg(test)]
            outcome => Err(unsupported_before_provider_request_outcome(&outcome)),
        }
    }

    fn resolve_tool_call(&self, call: &ToolCall) -> Option<ResolvedToolCall> {
        let tool_index = self.tools.iter().position(|tool| {
            (self.direct_tool || tool.model_exposure) && tool.definition().name == call.name
        })?;
        Some((tool_index, self.tools[tool_index].definition().clone()))
    }

    #[allow(clippy::too_many_lines)]
    async fn start_tool(&mut self, call: ToolCall) -> Result<ToolExecution> {
        if self.session_record.as_ref().is_some_and(|record| record.journal.iter().any(|event| matches!(event,
            SessionJournalEvent::MiddlewareStateUpdated { namespace, state }
            if namespace == "host-dispatch" && state.get("callId").and_then(Value::as_str) == Some(call.id.as_str())))) {
            return Err(validation_error("host call identity was already dispatched"));
        }
        if self.completion_validation.is_some() {
            self.completion_effective_calls
                .insert(call.id.clone(), call.clone());
        }
        let resolved = self.resolve_tool_call(&call);
        if let Some((index, _)) = &resolved
            && let Some(guard) = self.tools[*index].execution_guard.clone()
        {
            guard(self.cancellation_token.clone())
                .await
                .map_err(|_| validation_error("host execution authority ended"))?;
        }
        if self.cancellation_token.is_cancelled() {
            return Ok(ToolExecution::Finished(None));
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

        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        let parent_id = self.tool_item_id(&call.id)?;
        let progress = ToolProgressSink::timeline(
            self.session_id.clone(),
            self.turn as u64,
            parent_id,
            move |event| {
                drop(progress_tx.send(event));
                Ok(())
            },
        );
        let mut context = ToolExecutionContext::with_progress_and_cancellation(
            call.clone(),
            progress,
            self.cancellation_token.clone(),
        )
        .with_session_context(self.session_id.clone(), self.turn as u64);
        context.session_incarnation = self
            .session_record
            .as_ref()
            .and_then(|record| record.metadata.incarnation.clone());
        if let Some(prepare) = self.tools[tool_index].host_dispatch.clone() {
            let record = self.session_record.as_ref().ok_or_else(|| {
                validation_error("host dispatch requires durable session storage")
            })?;
            let incarnation =
                record.metadata.incarnation.clone().ok_or_else(|| {
                    validation_error("host dispatch requires a durable incarnation")
                })?;
            if record.journal.iter().any(|event| matches!(event,
                SessionJournalEvent::MiddlewareStateUpdated { namespace, state }
                if namespace == "host-dispatch" && state.get("callId").and_then(Value::as_str) == Some(call.id.as_str()))) {
                return Err(validation_error("host call identity was already dispatched"));
            }
            let binding = prepare(&call)?;
            if [&binding.invocation_id, &binding.source_revision]
                .iter()
                .any(|value| {
                    value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
                })
            {
                return Err(validation_error("invalid host dispatch association"));
            }
            if record.journal.iter().any(|event| matches!(event,
                SessionJournalEvent::MiddlewareStateUpdated { namespace, state }
                if namespace == "host-dispatch" && state.get("invocationId").and_then(Value::as_str) == Some(binding.invocation_id.as_str()))) {
                return Err(validation_error("host invocation identity was already dispatched"));
            }
            self.middleware_state.set("host-dispatch", json!({"active":true,"callId":call.id,"turn":self.turn,"incarnation":incarnation,"invocationId":binding.invocation_id,"sourceRevision":binding.source_revision}));
            self.flush_middleware_state().await?;
            context.host_dispatch = Some(binding);
            context.session_incarnation = Some(incarnation);
        }
        let host_dispatched = context.host_dispatch.is_some();
        let result_metadata = context.result_metadata.clone();
        result_metadata.hydrate_state(self.middleware_state.host_snapshot());
        let result = self.tools[tool_index].call(context);
        if self.completion_validation.is_some() {
            self.completion_effective_calls
                .insert(call.id.clone(), call.clone());
        }
        let result = self.instrument_tool_future(&call, result);
        Ok(ToolExecution::Running(RunningTool {
            result_metadata,
            call,
            result,
            progress_rx: Some(progress_rx),
            host_dispatched,
        }))
    }

    async fn finish_running_tool(
        &mut self,
        call: ToolCall,
        result: Result<ToolResult>,
        host_dispatched: bool,
        result_metadata: super::tool::ToolResultMetadata,
    ) -> Result<bool> {
        let call = result_metadata.take_effective_call().unwrap_or(call);
        if self.completion_validation.is_some() {
            self.completion_effective_calls
                .insert(call.id.clone(), call.clone());
        }
        for (namespace, value) in result_metadata.take_state() {
            self.middleware_state.set(namespace, value);
        }
        self.synchronize_context_updates().await?;
        self.flush_middleware_state().await?;
        if host_dispatched && result.is_err() {
            return Err(validation_error(
                "host dispatch outcome uncertain; automatic replay forbidden",
            ));
        }
        let completed = result.is_ok();
        let mut result = match result {
            Ok(result) => {
                self.in_turn_span(|| tracing::info!(tool_name = %call.name, tool_call_id = %call.id, "tool call completed"));
                result
            }
            Err(error) => {
                self.in_turn_span(|| tracing::warn!(tool_name = %call.name, tool_call_id = %call.id, %error, "tool call failed"));
                error_tool_result(&call, &error)
            }
        };
        if result.call_id != call.id || result.name != call.name {
            return Err(validation_error("host tool result does not match its call"));
        }

        let metadata = if completed {
            result_metadata.take()
        } else {
            Default::default()
        };
        let original_result = result.clone();
        let after_outcome = {
            let mut ctx = ToolResultContext {
                call: Some(&call),
                result: &mut result,
                middleware_state: &mut self.middleware_state,
                cancellation_token: self.cancellation_token.clone(),
                session_id: self.session_id.as_str(),
                turn: self.turn as u64,
            };
            self.middleware_chain.after_tool_call(&mut ctx).await
        };
        match after_outcome {
            Ok(HookOutcome::Continue) => {
                if result.call_id != call.id || result.name != call.name {
                    return Err(validation_error(
                        "host result processing changed call association",
                    ));
                }
                let metadata = if original_result == result {
                    metadata
                } else {
                    Default::default()
                };
                self.finish_tool_result_with_metadata(result, metadata)
                    .await?;
                Ok(true)
            }
            #[cfg(test)]
            Ok(outcome) => {
                if let Some(result) = self.tool_result_from_hook_outcome(&call, outcome).await? {
                    self.finish_tool_result_with_metadata(result, Default::default())
                        .await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(error) => {
                self.finish_tool_result_with_metadata(
                    error_tool_result(&call, &error),
                    Default::default(),
                )
                .await?;
                Ok(true)
            }
        }
    }

    async fn finish_tool_result(&mut self, result: ToolResult) -> Result<()> {
        self.finish_tool_result_with_metadata(result, Default::default())
            .await
    }

    async fn finish_tool_result_with_metadata(
        &mut self,
        result: ToolResult,
        metadata: serde_json::Map<String, Value>,
    ) -> Result<()> {
        let parent_id = self.tool_item_id(&result.call_id)?;
        self.account_completion_result(&result, parent_id.clone(), self.turn as u64)?;
        CompletionTurn::entry(&mut self.turns, self.turn as u64)
            .items
            .push(TurnItem::ToolResult(result.clone()));
        let events = self.middleware_state.drain_pending();
        self.append_journal_events(events).await?;
        let completed_call = result.call_id.clone();
        let mut pending =
            tool_result_timeline_event(&self.session_id, self.turn as u64, parent_id, result);
        pending.metadata = Value::Object(metadata);
        let event = self.append_timeline_event(pending).await?;
        self.pending.push_back(Ok(event));
        if let Some(mut dispatch) = self.middleware_state.get("host-dispatch").cloned()
            && dispatch.get("callId").and_then(Value::as_str) == Some(completed_call.as_str())
        {
            let unsettled = dispatch.clone();
            dispatch["active"] = json!(false);
            self.middleware_state.set("host-dispatch", dispatch);
            if let Err(error) = self.flush_middleware_state().await {
                // A missing acknowledgement is not permission to retry settlement.
                self.middleware_state.discard_pending("host-dispatch");
                self.middleware_state
                    .merge_committed("host-dispatch".into(), unsettled);
                return Err(error);
            }
        }
        Ok(())
    }

    fn account_completion_result(
        &mut self,
        result: &ToolResult,
        parent_id: TimelineItemId,
        turn: u64,
    ) -> Result<()> {
        if let Some(encoded_byte_limit) = self
            .completion_validation
            .as_ref()
            .map(|validation| validation.encoded_byte_limit)
        {
            let call = self
                .completion_effective_calls
                .remove(&result.call_id)
                .or_else(|| {
                    self.turns
                        .iter()
                        .flat_map(|turn| &turn.items)
                        .find_map(|item| match item {
                            TurnItem::ToolCall(call) if call.id == result.call_id => {
                                Some(call.clone())
                            }
                            _ => None,
                        })
                })
                .ok_or_else(|| validation_error("missing evidence call"))?;
            let evidence = CompletionToolEvidence {
                target: self
                    .resolve_tool_call(&call)
                    .and_then(|(index, _)| self.tools[index].identity().map(str::to_owned)),
                call,
                result: result.clone(),
                item_id: parent_id.clone(),
                turn,
            };
            let bytes = serde_json::to_vec(&evidence)
                .map_err(|_| validation_error("invalid evidence"))?
                .len();
            self.completion_evidence_bytes = self.completion_evidence_bytes.saturating_add(bytes);
            if self.completion_evidence_bytes > encoded_byte_limit
                || self.completion_evidence.len() >= 256
            {
                return Err(validation_error("cumulative evidence limit exceeded"));
            }
            self.completion_evidence.push(evidence);
        }
        Ok(())
    }

    async fn finish_tool_batch(&mut self) -> Result<()> {
        if self.direct_tool {
            self.middleware_state
                .set("host-tool-execution", json!({"active": false}));
            let mut events = self.middleware_state.drain_pending();
            events.extend([
                SessionJournalEvent::TurnFinished {
                    turn: self.turn as u64,
                },
                SessionJournalEvent::Finished,
            ]);
            self.append_journal_events(events).await?;
            self.phase = AgentLoopPhase::Done;
            return Ok(());
        }
        self.append_journal_event(SessionJournalEvent::TurnFinished {
            turn: self.turn as u64,
        })
        .await?;
        self.close_turn_span("tools_finished");
        self.turn += 1;
        self.phase = AgentLoopPhase::StartTurn;
        Ok(())
    }

    async fn validate_completion(&mut self) -> Result<bool> {
        let Some(validation) = self.completion_validation.clone() else {
            return Ok(true);
        };
        let response = self
            .turns
            .iter()
            .find(|turn| turn.turn == self.turn as u64)
            .map(|turn| turn.assistant_texts().collect::<String>())
            .unwrap_or_default();
        let proposal = CompletionProposal {
            session_id: self.session_id.clone(),
            proposal_id: format!("proposal:{}", self.turn),
            turn: self.turn as u64,
            response,
            evidence: self.completion_evidence.clone(),
        };
        if serde_json::to_vec(&proposal)
            .map_err(|_| validation_error("invalid proposal"))?
            .len()
            > validation.encoded_byte_limit
        {
            return Err(validation_error("proposal limit exceeded"));
        }
        let decision = tokio::select! {
            biased;
            () = self.cancellation_token.cancelled() => return Err(validation_error("validation canceled")),
            () = tokio::time::sleep_until(validation.deadline) => return Err(validation_error("total deadline exceeded")),
            result = validation.observer.observe(proposal.clone(), self.cancellation_token.clone()) => result.map_err(|_| validation_error("completion observer failed or returned an invalid verdict"))?,
        };
        match &decision {
            CompletionDecision::Accept => {
                self.middleware_state.set("completion-validation", json!({"active": false, "proposalId": proposal.proposal_id, "decision": "accept"}));
                Ok(true)
            }
            CompletionDecision::Reject { feedback } => {
                if feedback.proposal_id != proposal.proposal_id
                    || feedback.message.trim().is_empty()
                    || feedback.message.len() > MAX_COMPLETION_FEEDBACK_BYTES
                    || feedback.call_id.as_ref().is_some_and(|id| {
                        !proposal
                            .evidence
                            .iter()
                            .any(|evidence| &evidence.call.id == id)
                    })
                {
                    return Err(validation_error("invalid completion feedback"));
                }
                let prompt = serde_json::to_string(&json!({"completionValidation": decision}))
                    .map_err(|_| validation_error("invalid completion feedback"))?;
                self.append_journal_events(vec![
                    SessionJournalEvent::TurnFinished {
                        turn: self.turn as u64,
                    },
                    SessionJournalEvent::PromptAppended {
                        prompt: prompt.clone(),
                    },
                ])
                .await?;
                self.turn += 1;
                self.prompt_turns.push(PromptTurn {
                    turn: self.turn as u64,
                    prompt: prompt.clone(),
                });
                self.prompt = prompt;
                self.assistant_item_id = None;
                self.thinking_item_id = None;
                self.phase = AgentLoopPhase::StartTurn;
                self.close_turn_span("completion_rejected");
                Ok(false)
            }
        }
    }

    #[cfg(test)]
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
            #[cfg(test)]
            HookOutcome::ReplaceToolResult(result) => Ok(Some(result)),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn initialize_session(&mut self) -> Result<bool> {
        if self.session_initialized {
            return Ok(true);
        }

        let operation = match &self.session_run_context {
            Some(context) => Some(context.operation().await),
            None => None,
        };
        let incarnation = operation.as_ref().map(SessionOperation::incarnation);

        match self.session_store.load(&self.session_id).await {
            Ok(record) => {
                if let Some(expected) = self
                    .session_run_context
                    .as_ref()
                    .and_then(SessionRunContext::execution_token)
                {
                    let actual = record.execution_token().map_err(session_store_error)?;
                    if &actual != expected {
                        return Err(session_store_error(SessionStoreError::StaleExecution {
                            expected: expected.clone(),
                            actual,
                        }));
                    }
                }
                if let Some(operation) = &operation {
                    operation.register_loaded_summary(record.summary());
                }
                self.session_incarnation = incarnation;
                let record = self.prepare_initial_record(record).await?;
                let continue_start_turn = self.apply_resume_input(record).await?;
                self.session_initialized = true;
                return Ok(continue_start_turn);
            }
            Err(error @ SessionStoreError::NotFound { .. }) => {
                if self
                    .session_run_context
                    .as_ref()
                    .and_then(SessionRunContext::execution_token)
                    .is_some()
                {
                    return Err(session_store_error(error));
                }
            }
            Err(error) => return Err(session_store_error(error)),
        }

        if self.provider.transient_input().is_some() {
            return Err(validation_error(
                "transient input requires an existing durable helper task",
            ));
        }
        if !matches!(
            self.input,
            Some(ResumeInput::Prompt(_) | ResumeInput::Prompts(_))
        ) {
            return Err(session_store_error(
                "cannot resume missing session without a prompt input",
            ));
        }
        let plain_prompts = match self.input.as_ref() {
            Some(ResumeInput::Prompt(prompt)) if !may_contain_template(prompt) => {
                Some(vec![prompt.clone()])
            }
            Some(ResumeInput::Prompts(prompts))
                if !prompts.is_empty()
                    && prompts.iter().all(|prompt| !may_contain_template(prompt)) =>
            {
                Some(prompts.clone())
            }
            _ => None,
        };
        let mut request = SessionCreateRequest::new(self.session_id.clone());
        if let Some(prompts) = &plain_prompts {
            request.start = SessionStart::Fresh {
                messages: prompts
                    .iter()
                    .cloned()
                    .map(SessionStartMessage::user)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| session_store_error(error.to_string()))?,
            };
        }
        let record = self
            .session_store
            .create(request)
            .await
            .map_err(session_store_error)?;
        if let Some(operation) = &operation {
            operation.publish_created(record.summary());
        }
        self.session_incarnation = incarnation;
        let record = self.prepare_initial_record(record).await?;
        if let Some(prompts) = plain_prompts {
            self.input = None;
            self.hydrate_from_record(&record)?;
            let events = record.timeline_events().cloned().collect::<Vec<_>>();
            if let Some(context) = &self.session_run_context {
                context.timeline().publish_all(events.clone());
            }
            self.publish_user_message_events(0, 0, &prompts);
            self.pending.extend(events.into_iter().map(Ok));
            self.session_initialized = true;
            return Ok(true);
        }
        let continue_start_turn = self.apply_resume_input(record).await?;
        self.session_initialized = true;
        Ok(continue_start_turn)
    }

    async fn check_host_execution_guards(&mut self) -> Result<()> {
        let mut checked = Vec::new();
        for guard in self
            .tools
            .iter()
            .filter_map(|tool| tool.execution_guard.clone())
        {
            if checked.iter().any(|prior| Arc::ptr_eq(prior, &guard)) {
                continue;
            }
            guard(self.cancellation_token.clone())
                .await
                .map_err(|_| validation_error("host execution authority ended"))?;
            checked.push(guard);
        }
        Ok(())
    }

    async fn prepare_initial_record(&mut self, record: SessionRecord) -> Result<SessionRecord> {
        self.check_host_execution_guards().await?;
        // Logical rollback cannot erase an external dispatch fence.
        if super::tool::host_dispatch_unsettled(&record) {
            return Err(validation_error(
                "interrupted host dispatch cannot resume; reconcile the recorded invocation",
            ));
        }
        let snapshot = record.snapshot();
        if snapshot.middleware_state.contains_key("concrete-execution") {
            return Err(validation_error(
                "legacy concrete execution records are unsupported",
            ));
        }
        for namespace in [
            "live-consent",
            "completion-validation",
            "host-tool-execution",
            "transient-input",
        ] {
            if snapshot
                .middleware_state
                .get(namespace)
                .and_then(|value| value.get("active"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                return Err(validation_error(
                    "interrupted consumer execution cannot resume; start a new invocation",
                ));
            }
        }
        if let Some(input) = self.provider.transient_input() {
            input.preflight()?;
            if !matches!(self.input, Some(ResumeInput::Continue)) {
                return Err(validation_error(
                    "transient input requires a durable initial helper task",
                ));
            }
            let target = input.target();
            let context = record.materialize_context(SessionContextMode::Effective);
            let task_event = record
                .timeline_events()
                .filter(|event| {
                    event.channel.as_ref().is_some_and(|channel| {
                        channel.as_str() == crate::timeline::CHANNEL_USER_TEXT
                    })
                })
                .last();
            if context.prompt_turns.len().checked_sub(1) != Some(target.prompt_index)
                || context
                    .prompt_turns
                    .last()
                    .is_none_or(|prompt| prompt.turn != target.turn)
                || context.next_turn != target.turn
                || !task_event.is_some_and(|event| {
                    event.cursor == target.cursor
                        && event.turn == target.turn
                        && matches!(&event.content, crate::timeline::TimelineContent::TextSnapshot { text }
                            if context.prompt_turns.last().is_some_and(|prompt| &prompt.prompt == text))
                })
            {
                return Err(validation_error(
                    "transient input initial task identity mismatch",
                ));
            }
        }
        self.hydrate_from_record(&record)?;

        if self.provider.transient_input().is_some() {
            self.middleware_state.set(
                "transient-input",
                json!({"active": true, "replayable": false}),
            );
            self.flush_middleware_state().await?;
        }
        if self.completion_validation.is_some() {
            self.middleware_state
                .set("completion-validation", json!({"active": true}));
            self.flush_middleware_state().await?;
        }
        Ok(self.session_record.clone().expect("loaded session"))
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_resume_input(&mut self, record: SessionRecord) -> Result<bool> {
        self.check_host_execution_guards().await?;
        let Some(input) = self.input.take() else {
            self.hydrate_from_record(&record)?;
            return Ok(true);
        };

        // A previous process may have died after sending Execute, or an older
        // record may have no dispatch fence at all. Neither can be retried safely.
        let snapshot = record.snapshot();
        self.hydrate_from_record(&record)?;
        let pending_sandbox = snapshot
            .pending_work
            .iter()
            .filter(|work| {
                matches!(work.status, PendingWorkStatus::Pending)
                    && work.tool_call.as_ref().is_some_and(|call| {
                        snapshot.sandbox_calls_recorded.iter().any(|dispatch| {
                            dispatch.turn == work.turn && dispatch.tool_call_id == call.id
                        })
                    })
            })
            .collect::<Vec<_>>();
        let externally_resolved = matches!(&input, ResumeInput::ToolResults(results)
            if snapshot.pending_work.iter()
                .filter(|work| matches!(work.status, PendingWorkStatus::Pending))
                .all(|work| results.iter().any(|result| result.call_id == work.id)));
        if !matches!(input, ResumeInput::Prompt(_) | ResumeInput::Prompts(_))
            && !pending_sandbox.is_empty()
            && !externally_resolved
        {
            self.hydrate_from_record(&record)?;
            self.suspend_pending_sandbox_work().await?;
            return Ok(false);
        }

        let continue_start_turn = true;
        let new_prompts = matches!(input, ResumeInput::Prompt(_) | ResumeInput::Prompts(_));
        let record = match input {
            ResumeInput::Continue => record,
            ResumeInput::Prompt(prompt) => {
                if may_contain_template(&prompt) {
                    self.submit_template_prompts(vec![prompt]).await?;
                    self.session_record
                        .as_ref()
                        .expect("submitted template")
                        .clone()
                } else if snapshot.current_prompt.as_deref() == Some(prompt.as_str())
                    && snapshot.prompts.len() == 1
                    && snapshot.turns.is_empty()
                    && self.pending_user_template()?.is_none()
                {
                    record
                } else {
                    match self
                        .session_runtime()
                        .resume_record(record, ResumeInput::Prompt(prompt))
                        .await?
                    {
                        RuntimeResumeOutput::Resumed { record }
                        | RuntimeResumeOutput::Suspended { record, .. } => record,
                    }
                }
            }
            ResumeInput::Prompts(prompts) => {
                if prompts.iter().any(|prompt| may_contain_template(prompt)) {
                    self.submit_template_prompts(prompts).await?;
                    self.session_record
                        .as_ref()
                        .expect("submitted template")
                        .clone()
                } else {
                    match self
                        .session_runtime()
                        .resume_record(record, ResumeInput::Prompts(prompts))
                        .await?
                    {
                        RuntimeResumeOutput::Resumed { record }
                        | RuntimeResumeOutput::Suspended { record, .. } => record,
                    }
                }
            }
            ResumeInput::ToolResults(results) => {
                match self
                    .session_runtime()
                    .resume_record(record, ResumeInput::ToolResults(results))
                    .await?
                {
                    RuntimeResumeOutput::Resumed { record }
                    | RuntimeResumeOutput::Suspended { record, .. } => record,
                }
            }
        };

        self.hydrate_from_record(&record)?;
        if continue_start_turn && !self.render_pending_template_prompts(new_prompts).await? {
            return Ok(false);
        }
        Ok(continue_start_turn)
    }

    async fn submit_template_prompts(&mut self, prompts: Vec<String>) -> Result<()> {
        if prompts.is_empty() {
            return Err(session_store_error("prompt batch must not be empty"));
        }
        let prompt_count = self
            .session_record
            .as_ref()
            .expect("loaded session")
            .snapshot()
            .prompts
            .len();
        self.middleware_state.set(
            PENDING_USER_TEMPLATE,
            serde_json::to_value(PendingUserTemplate {
                prompts,
                prompt_count,
                submission: self.session_journal_len,
                denied: false,
            })
            .map_err(session_store_error)?,
        );
        self.flush_middleware_state().await?;
        self.template_submission_journal_len = self
            .session_record
            .as_ref()
            .map(|record| record.journal.len());
        Ok(())
    }

    async fn render_pending_template_prompts(&mut self, superseding_prompt: bool) -> Result<bool> {
        let pending = self.pending_user_template()?;
        let Some(pending) = pending else {
            return Ok(true);
        };
        if pending.denied && !superseding_prompt {
            self.phase = AgentLoopPhase::Done;
            return Ok(false);
        }
        let snapshot = self
            .session_record
            .as_ref()
            .expect("loaded session")
            .snapshot();
        if snapshot.prompts.len() == pending.prompt_count {
            let Some((prompts, tool_ids)) = self.render_prompt_templates(pending.prompts).await?
            else {
                return Ok(false);
            };
            self.flush_middleware_state().await?;
            let record = self
                .session_record
                .clone()
                .expect("committed template state");
            let output = self
                .session_runtime()
                .resume_record(record, ResumeInput::Prompts(prompts))
                .await?;
            let record = match output {
                RuntimeResumeOutput::Resumed { record }
                | RuntimeResumeOutput::Suspended { record, .. } => record,
            };
            self.hydrate_from_record(&record)?;
            // Earlier prompts in this batch may have completed before a later one suspended.
            if let Some(host) = &self.host_context {
                host.finish_prompt(&mut self.middleware_state, &tool_ids)?;
            }
        }
        self.middleware_state
            .set(PENDING_USER_TEMPLATE, Value::Null);
        self.flush_middleware_state().await?;
        Ok(true)
    }

    fn pending_user_template(&self) -> Result<Option<PendingUserTemplate>> {
        self.middleware_state
            .get(PENDING_USER_TEMPLATE)
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()).map_err(session_store_error))
            .transpose()
    }

    async fn suspend_pending_sandbox_work(&mut self) -> Result<()> {
        let snapshot = self
            .session_record
            .as_ref()
            .expect("initialized session")
            .snapshot();
        if !matches!(
            snapshot.status,
            super::session::SessionStatus::Suspended(SessionSuspension::AwaitingToolResults { .. })
        ) {
            self.append_journal_event(SessionJournalEvent::Suspended {
                suspension: SessionSuspension::AwaitingToolResults {
                    work: snapshot.pending_work,
                },
            })
            .await?;
        }
        self.phase = AgentLoopPhase::Done;
        self.close_turn_span("sandbox_dispatch_uncertain");
        Ok(())
    }

    fn hydrate_from_record(&mut self, record: &SessionRecord) -> Result<()> {
        let snapshot = record.snapshot();
        let context = record.materialize_context(SessionContextMode::Effective);
        self.middleware_state = MiddlewareStateLog::from_snapshot(&snapshot);
        self.middleware_state.effects = self
            .host_effects
            .as_ref()
            .map(|effects| effects.clone() as Arc<dyn crate::HostEffectExecutor>);
        self.session_metadata_values = record.metadata.values.clone();
        self.prompt = context.current_prompt.unwrap_or_default();
        self.prompt_turns = context.prompt_turns;
        self.turns = context.turns;
        self.turn = usize::try_from(snapshot.turn).unwrap_or(usize::MAX);
        self.session_revision = record.revision;
        self.session_execution_token = Some(record.execution_token().map_err(session_store_error)?);
        self.session_journal_len = record.journal.len() as u64;
        self.last_timeline_cursor = record.timeline_events().last().map(|event| event.cursor);
        self.session_record = Some(record.clone());
        let current_turn = self.turn as u64;
        self.assistant_item_id = None;
        self.thinking_item_id = None;
        self.tool_item_ids.clear();
        for event in record.timeline_events() {
            match &event.content {
                TimelineContent::ToolCall { call } => {
                    self.tool_item_ids
                        .insert(call.id.clone(), event.item_id.clone());
                }
                TimelineContent::TextDelta { .. } | TimelineContent::TextSnapshot { .. }
                    if event.turn == current_turn
                        && event.channel.as_ref().is_some_and(|channel| {
                            channel.as_str() == crate::timeline::CHANNEL_ASSISTANT_TEXT
                        }) =>
                {
                    self.assistant_item_id = Some((event.turn, event.item_id.clone()));
                }
                TimelineContent::TextDelta { .. }
                    if event.turn == current_turn
                        && event.channel.as_ref().is_some_and(|channel| {
                            channel.as_str() == crate::timeline::CHANNEL_THINKING
                        }) =>
                {
                    self.thinking_item_id = Some((event.turn, event.item_id.clone()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn prompt_turns(&self) -> Vec<PromptTurn> {
        if self.prompt_turns.is_empty() && !self.prompt.is_empty() {
            vec![PromptTurn {
                turn: self.turn as u64,
                prompt: self.prompt.clone(),
            }]
        } else {
            self.prompt_turns.clone()
        }
    }

    #[allow(clippy::option_if_let_else)]
    async fn render_prompt_templates(
        &mut self,
        prompts: Vec<String>,
    ) -> Result<Option<(Vec<String>, Vec<String>)>> {
        self.prepare_host_effects().await?;
        let host = self.host_context.clone();
        let mut rendered = Vec::with_capacity(prompts.len());
        let mut tool_ids = Vec::new();
        let prompt_count = self
            .session_record
            .as_ref()
            .map_or(0, |record| record.snapshot().prompts.len());
        let submission = self
            .middleware_state
            .get(PENDING_USER_TEMPLATE)
            .and_then(|value| value.get("submission"))
            .and_then(Value::as_u64)
            .unwrap_or(self.session_journal_len);
        for (index, prompt) in prompts.into_iter().enumerate() {
            let Some(host) = &host else {
                rendered.push(prompt);
                continue;
            };
            let outcome = host
                .render_prompt(
                    crate::HostContext {
                        session_id: self.session_id.as_str(),
                        turn: self.turn as u64,
                        cancellation: self.cancellation_token.clone(),
                        metadata: None,
                        state: &mut self.middleware_state,
                    },
                    &prompt,
                    submission,
                    prompt_count + index,
                )
                .await?;
            tool_ids.extend(outcome.tool_ids);
            rendered.push(outcome.prompt);
        }
        Ok(Some((rendered, tool_ids)))
    }

    async fn append_journal_event(&mut self, event: SessionJournalEvent) -> Result<()> {
        self.append_journal_events(vec![event]).await
    }

    async fn append_journal_events(&mut self, events: Vec<SessionJournalEvent>) -> Result<()> {
        self.synchronize_host_effects().await?;
        if events.is_empty() {
            return Ok(());
        }
        let event_count = events.len() as u64;
        let projected_events = events.clone();
        let receipt = self
            .session_store
            .append_journal(&self.session_id, self.execution_token()?, events)
            .await
            .map_err(session_store_error)?;
        self.adopt_commit(receipt);
        let committed_facts = projected_events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                SessionJournalEvent::TurnFinished { turn } => {
                    Some((self.session_journal_len.saturating_add(index as u64), super::event::SessionSemanticKind::TurnCompleted { turn: *turn }))
                }
                SessionJournalEvent::Finished => Some((self.session_journal_len.saturating_add(index as u64), super::event::SessionSemanticKind::RunFinished)),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.session_journal_len = self.session_journal_len.saturating_add(event_count);
        if let Some(record) = &mut self.session_record {
            record.append_journal_events(projected_events);
        }
        self.publish_session_summary();
        for (index, kind) in committed_facts {
            self.notify_semantic(
                kind,
                format!("j{index}"),
                self.last_timeline_cursor,
            );
        }
        Ok(())
    }

    async fn flush_middleware_state(&mut self) -> Result<()> {
        let events = self.middleware_state.drain_pending();
        if let Err(error) = self.append_journal_events(events.clone()).await {
            self.middleware_state.restore_pending(events);
            return Err(error);
        }
        Ok(())
    }

    async fn synchronize_context_updates(&mut self) -> Result<()> {
        let Some(previous) = self.session_record.as_ref() else {
            return Ok(());
        };
        let current = self
            .session_store
            .load(&self.session_id)
            .await
            .map_err(session_store_error)?;
        if current.metadata.incarnation != previous.metadata.incarnation
            || current.agent_definition != previous.agent_definition
            || current.inference_settings != previous.inference_settings
            || !current.journal.starts_with(&previous.journal)
            || current.journal[previous.journal.len()..]
                .iter()
                .any(|event| !matches!(event, SessionJournalEvent::MasksMutated { .. }))
            || current.execution_sequence.get()
                != previous.execution_sequence.get().saturating_add(
                    current.journal.len().saturating_sub(previous.journal.len()) as u64,
                )
        {
            return Err(session_store_error(
                "session changed outside the live context boundary",
            ));
        }
        if current.revision == self.session_revision
            && current.execution_sequence == previous.execution_sequence
        {
            return Ok(());
        }
        self.session_revision = current.revision;
        self.session_execution_token =
            Some(current.execution_token().map_err(session_store_error)?);
        self.session_journal_len = current.journal.len() as u64;
        self.session_record = Some(current);
        Ok(())
    }

    async fn prepare_host_effects(&mut self) -> Result<()> {
        self.flush_middleware_state().await?;
        self.synchronize_host_effects().await?;
        if self.host_effects.is_none() {
            let record = self
                .session_record
                .as_ref()
                .ok_or_else(|| validation_error("host context requires durable storage"))?;
            self.host_effects = Some(Arc::new(super::host_effect::HostEffectJournal::new(
                self.session_store.clone(),
                record,
                self.cancellation_token.clone(),
            )?));
        }
        self.middleware_state.effects = self
            .host_effects
            .as_ref()
            .map(|effects| effects.clone() as Arc<dyn crate::HostEffectExecutor>);
        Ok(())
    }

    async fn synchronize_host_effects(&mut self) -> Result<()> {
        let Some(effects) = &self.host_effects else {
            return Ok(());
        };
        let record = self
            .session_record
            .as_ref()
            .ok_or_else(|| validation_error("host context requires durable storage"))?;
        let (receipt, events) = effects
            .synchronize(record.commit_receipt().map_err(session_store_error)?)
            .await?;
        self.adopt_commit(receipt);
        self.session_journal_len = self.session_journal_len.saturating_add(events.len() as u64);
        for event in &events {
            if let SessionJournalEvent::MiddlewareStateUpdated { namespace, state } = event {
                self.middleware_state
                    .merge_committed(namespace.clone(), state.clone());
            }
        }
        if let Some(record) = &mut self.session_record {
            record.append_journal_events(events);
        }
        self.publish_session_summary();
        Ok(())
    }

    async fn record_interrupted(&mut self) -> Result<()> {
        self.cancellation_token.cancel();
        self.synchronize_host_effects().await?;
        if !self.session_initialized {
            return Ok(());
        }
        let operation = match &self.session_run_context {
            Some(context) => Some(context.operation().await),
            None => None,
        };
        if operation
            .as_ref()
            .is_some_and(|operation| Some(operation.incarnation()) != self.session_incarnation)
        {
            return Ok(());
        }
        let record = self.session_runtime().interrupt(&self.session_id).await?;
        self.hydrate_from_record(&record)?;
        self.close_turn_span("interrupted");
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
        self.synchronize_host_effects().await?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let receipt = self
            .session_store
            .append_timeline(&self.session_id, self.execution_token()?, events)
            .await
            .map_err(session_store_error)?;
        self.adopt_commit(receipt.commit);
        let assigned = receipt.events;
        self.session_journal_len = self
            .session_journal_len
            .saturating_add(assigned.len() as u64);
        self.last_timeline_cursor = assigned
            .last()
            .map(|event| event.cursor)
            .or(self.last_timeline_cursor);
        if let Some(record) = &mut self.session_record {
            record
                .append_journal_events(assigned.iter().cloned().map(SessionJournalEvent::Timeline));
        }
        if let Some(context) = &self.session_run_context {
            context.timeline().publish_all(assigned.clone());
        }
        self.publish_session_summary();
        for event in &assigned {
            if let TimelineContent::ToolResult { result } = &event.content {
                self.notify_semantic(
                    super::event::SessionSemanticKind::ToolResultCommitted {
                        call_id: result.call_id.clone(),
                        tool_name: result.name.clone(),
                        succeeded: !result.is_error,
                    },
                    format!("c{}", event.cursor.get()),
                    Some(event.cursor),
                );
            }
        }
        Ok(assigned)
    }

    fn execution_token(&self) -> Result<crate::ExecutionToken> {
        self.session_execution_token
            .clone()
            .ok_or_else(|| validation_error("execution requires a durable session token"))
    }

    fn adopt_commit(&mut self, receipt: crate::SessionCommitReceipt) {
        self.session_revision = receipt.revision;
        self.session_execution_token = Some(receipt.execution_token.clone());
        self.session_metadata_values = receipt.metadata.values.clone();
        if let Some(record) = &mut self.session_record {
            record.adopt_commit(receipt);
        }
    }

    fn notify_semantic(
        &self,
        kind: super::event::SessionSemanticKind,
        commit: String,
        cursor: Option<crate::TimelineCursor>,
    ) {
        let Some(sink) = self
            .session_run_context
            .as_ref()
            .and_then(|context| context.event_sink())
            .or(self.event_sink.as_ref())
        else {
            return;
        };
        let metadata = self.session_record.as_ref().map(|record| &record.metadata);
        let incarnation = metadata.and_then(|metadata| metadata.incarnation.clone());
        sink.semantic(super::event::SessionSemanticEvent {
            id: format!(
                "semantic:{}:{}:{commit}",
                self.session_id.as_str(),
                incarnation.as_deref().unwrap_or("legacy")
            ),
            session_id: self.session_id.clone(),
            incarnation,
            revision: self.session_revision,
            cursor,
            kind,
        });
    }

    fn tool_item_id(&self, call_id: &str) -> Result<TimelineItemId> {
        self.tool_item_ids.get(call_id).cloned().ok_or_else(|| {
            session_store_error(format!("timeline item missing for tool call: {call_id}"))
        })
    }

    async fn append_assistant_delta(&mut self, delta: String) -> Result<AgentTimelineEvent> {
        let turn = self.turn as u64;
        let item_id = self
            .assistant_item_id
            .as_ref()
            .filter(|(item_turn, _)| *item_turn == turn)
            .map_or(PendingTimelineItemId::New, |(_, item_id)| {
                PendingTimelineItemId::Existing(item_id.clone())
            });
        let event = self
            .append_timeline_event(PendingTimelineEvent::assistant_text_delta(
                self.session_id.clone(),
                turn,
                item_id,
                delta,
            ))
            .await?;
        self.assistant_item_id = Some((turn, event.item_id.clone()));
        Ok(event)
    }

    async fn append_thinking_delta(&mut self, delta: String) -> Result<AgentTimelineEvent> {
        let turn = self.turn as u64;
        let item_id = self
            .thinking_item_id
            .as_ref()
            .filter(|(item_turn, _)| *item_turn == turn)
            .map_or(PendingTimelineItemId::New, |(_, item_id)| {
                PendingTimelineItemId::Existing(item_id.clone())
            });
        let event = self
            .append_timeline_event(PendingTimelineEvent::thinking_delta(
                self.session_id.clone(),
                turn,
                item_id,
                delta,
            ))
            .await?;
        self.thinking_item_id = Some((turn, event.item_id.clone()));
        Ok(event)
    }

    async fn append_final_assistant_snapshot(&mut self) -> Result<Option<AgentTimelineEvent>> {
        let text = self
            .turns
            .iter()
            .find(|turn| turn.turn == self.turn as u64)
            .map(|turn| turn.assistant_texts().collect::<String>())
            .unwrap_or_default();
        if text.is_empty() {
            return Ok(None);
        }
        let turn = self.turn as u64;
        let item_id = self
            .assistant_item_id
            .as_ref()
            .filter(|(item_turn, _)| *item_turn == turn)
            .map_or(PendingTimelineItemId::New, |(_, item_id)| {
                PendingTimelineItemId::Existing(item_id.clone())
            });
        let event = self
            .append_timeline_event(PendingTimelineEvent::assistant_text_snapshot(
                self.session_id.clone(),
                turn,
                item_id,
                text,
            ))
            .await?;
        self.assistant_item_id = Some((turn, event.item_id.clone()));
        Ok(Some(event))
    }

    async fn append_finished_status_event(&mut self) -> Result<AgentTimelineEvent> {
        let mut event = PendingTimelineEvent::new(
            self.session_id.clone(),
            self.turn as u64,
            PendingTimelineItemId::New,
            TimelineEventKind::SessionStatusChanged,
            None,
            TimelineContent::Empty,
        );
        event.status = Some(TimelineStatus::Finished);
        self.append_timeline_event(event).await
    }

    async fn record_provider_turn_limit_failure(&mut self) -> Result<AgentTimelineEvent> {
        const MESSAGE: &str = "maximum provider turn limit exceeded";
        self.append_journal_event(SessionJournalEvent::Failed {
            message: MESSAGE.to_owned(),
            authentication_failure: None,
        })
        .await?;
        let mut event = PendingTimelineEvent::new(
            self.session_id.clone(),
            self.turn as u64,
            PendingTimelineItemId::New,
            TimelineEventKind::SessionStatusChanged,
            None,
            TimelineContent::TextSnapshot {
                text: MESSAGE.to_owned(),
            },
        );
        event.status = Some(TimelineStatus::Failed);
        let event = self.append_timeline_event(event).await?;
        self.close_turn_span("provider_turn_limit");
        Ok(event)
    }

    async fn record_run_failure(&mut self, error: &LlmError) -> Result<()> {
        let was_cancelled = self.cancellation_token.is_cancelled();
        self.cancellation_token.cancel();
        self.synchronize_host_effects().await?;
        self.flush_middleware_state().await?;
        self.close_turn_span("error");
        if was_cancelled {
            return Ok(());
        }
        let operation = match &self.session_run_context {
            Some(context) => Some(context.operation().await),
            None => None,
        };
        if was_cancelled
            || operation
                .as_ref()
                .is_some_and(|operation| Some(operation.incarnation()) != self.session_incarnation)
        {
            return Ok(());
        }
        // Resume can append a prompt before a later initialization step fails.
        if !self.session_initialized && self.session_record.is_some() {
            let record = self
                .session_store
                .load(&self.session_id)
                .await
                .map_err(session_store_error)?;
            self.hydrate_from_record(&record)?;
        }
        let Some(record) = &self.session_record else {
            return Ok(());
        };
        // Template submission retains the prior status until rendering appends a prompt.
        // Preserve only terminal outcomes established after this submission.
        let inherited_outcome = !self.session_initialized
            && self.template_submission_journal_len.is_some_and(|start| {
                !record.journal.iter().skip(start).any(|event| match event {
                    SessionJournalEvent::Finished
                    | SessionJournalEvent::Failed { .. }
                    | SessionJournalEvent::Suspended { .. }
                    | SessionJournalEvent::Interrupted
                    | SessionJournalEvent::TurnInterrupted { .. } => true,
                    SessionJournalEvent::Timeline(event)
                        if event.kind == TimelineEventKind::SessionStatusChanged =>
                    {
                        matches!(
                            event.status,
                            Some(
                                TimelineStatus::Finished
                                    | TimelineStatus::Failed
                                    | TimelineStatus::AwaitingToolResults
                                    | TimelineStatus::Interrupted
                            )
                        )
                    }
                    _ => false,
                })
            });
        if !inherited_outcome
            && matches!(
                record.snapshot().status,
                SessionStatus::Finished
                    | SessionStatus::Failed { .. }
                    | SessionStatus::Suspended(_)
            )
        {
            return Ok(());
        }
        self.append_journal_event(SessionJournalEvent::Failed {
            message: error.to_string(),
            authentication_failure: error.authentication_failure().cloned(),
        })
        .await
    }

    async fn retry_or_fail_provider_error(&mut self, error: LlmError) -> Result<()> {
        let Some(decision) = self.retry_config.classify(&error, self.provider_attempt) else {
            return Err(error);
        };

        let checkpoint = self.retry_checkpoint.clone().ok_or_else(|| {
            session_store_error("provider retry requested without rollback checkpoint")
        })?;
        self.rollback_to_retry_checkpoint(checkpoint).await?;
        let event = self
            .append_retrying_status_event(
                decision.attempt,
                decision.max_attempts,
                decision.delay,
                decision.message,
            )
            .await?;
        self.pending.push_back(Ok(event));
        self.provider_attempt = self.provider_attempt.saturating_add(1);
        self.phase = AgentLoopPhase::RetryDelay(Box::pin(tokio::time::sleep(decision.delay)));

        Ok(())
    }

    const fn retry_checkpoint(&self) -> RetryCheckpoint {
        RetryCheckpoint {
            journal_len: self.session_journal_len,
            after_cursor: self.last_timeline_cursor,
            turn: self.turn as u64,
        }
    }

    async fn rollback_to_retry_checkpoint(&mut self, checkpoint: RetryCheckpoint) -> Result<()> {
        let target = SessionRollbackTarget {
            journal_len: checkpoint.journal_len,
            after_cursor: checkpoint.after_cursor,
            next_turn: checkpoint.turn,
            remove_prompt_index: None,
        };
        self.append_journal_event(SessionJournalEvent::Rollback { target })
            .await?;
        if let Some(context) = &self.session_run_context {
            context
                .timeline()
                .publish_truncation(self.session_id.clone(), checkpoint.after_cursor);
        }
        let record = self
            .session_record
            .clone()
            .ok_or_else(|| validation_error("rollback requires a durable session record"))?;
        self.hydrate_from_record(&record)?;
        Ok(())
    }

    async fn append_retrying_status_event(
        &mut self,
        attempt: u32,
        max_attempts: u32,
        delay: Duration,
        message: String,
    ) -> Result<AgentTimelineEvent> {
        let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        let mut event = PendingTimelineEvent::new(
            self.session_id.clone(),
            self.turn as u64,
            PendingTimelineItemId::New,
            TimelineEventKind::SessionStatusChanged,
            None,
            TimelineContent::Json {
                value: json!({
                    "attempt": attempt,
                    "maxAttempts": max_attempts,
                    "delayMs": delay_ms,
                    "message": message,
                }),
            },
        );
        event.status = Some(TimelineStatus::RetryingProvider);
        self.append_timeline_event(event).await
    }

    fn ensure_turn_span(&mut self) {
        if self.turn_span.is_some() {
            return;
        }
        let span = tracing::info_span!(
            "agent.turn",
            session_id = %self.session_id.as_str(),
            turn = self.turn as u64,
            provider = self.provider.name(),
            model_provider = ?self.model.provider,
            model_api = ?self.model.api,
            model = %self.model.model.as_str(),
            terminal_state = tracing::field::Empty,
        );
        span.in_scope(|| tracing::info!("agent turn started"));
        self.turn_span = Some(span);
    }

    fn in_turn_span(&self, event: impl FnOnce()) {
        if let Some(span) = &self.turn_span {
            span.in_scope(event);
        } else {
            event();
        }
    }

    fn close_turn_span(&mut self, terminal_state: &'static str) {
        if let Some(span) = self.turn_span.take() {
            span.record("terminal_state", terminal_state);
            span.in_scope(|| tracing::info!(terminal_state, "agent turn completed"));
        }
    }

    fn instrument_tool_future(&self, call: &ToolCall, result: ToolFuture) -> ToolFuture {
        let span = {
            let _guard = self.turn_span.as_ref().map(Span::enter);
            tracing::info_span!(
                "agent.tool_call",
                session_id = %self.session_id.as_str(),
                turn = self.turn as u64,
                tool_name = %call.name,
                tool_call_id = %call.id,
            )
        };
        Box::pin(result.instrument(span))
    }

    async fn persist_provider_request(
        &mut self,
        draft: &ModelRequestDraft,
        projected: Option<Value>,
    ) -> Result<()> {
        let canonical_request = canonical_request_json(draft);
        let mut projected_request = projected.unwrap_or_else(|| canonical_request.clone());
        redact_provider_images(&mut projected_request);
        let request = RequestRecord {
            id: RequestId::new(format!(
                "{}-request-{}",
                self.session_id.as_str(),
                self.turn
            )),
            turn: self.turn as u64,
            model: self.model.clone(),
            canonical_request_hash: stable_request_hash(&canonical_request),
            projected_request,
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

    fn session_runtime(&self) -> AgentSessionRuntime<'_, S> {
        let mut runtime = AgentSessionRuntime::new(&self.session_store);
        if let (Some(context), Some(incarnation)) =
            (&self.session_run_context, self.session_incarnation)
        {
            runtime = runtime.with_session_run_context(context.clone(), incarnation);
        }
        if let Some(sink) = &self.event_sink {
            runtime.with_session_event_sink(Arc::clone(sink))
        } else {
            runtime
        }
    }

    fn publish_user_message_events(
        &self,
        first_prompt_index: usize,
        first_journal_index: usize,
        prompts: &[String],
    ) {
        let Some(sink) = self
            .session_run_context
            .as_ref()
            .and_then(SessionRunContext::event_sink)
            .or(self.event_sink.as_ref())
        else {
            return;
        };
        for (offset, text) in prompts.iter().enumerate() {
            let prompt_index =
                u64::try_from(first_prompt_index.saturating_add(offset)).unwrap_or(u64::MAX);
            super::event::spawn_session_event(
                Arc::clone(sink),
                crate::SessionEvent::UserMessageCreated {
                    id: super::event::user_message_event_id(
                        &self.session_id,
                        first_journal_index.saturating_add(offset.saturating_mul(2)),
                    ),
                    session_id: self.session_id.clone(),
                    incarnation: self.session_record.as_ref().and_then(|record| record.metadata.incarnation.clone()),
                    prompt_index,
                    user_message_count: prompt_index.saturating_add(1),
                    text: text.clone(),
                    metadata: self.session_metadata_values.clone(),
                    revision: self.session_revision,
                    cursor: self.last_timeline_cursor,
                },
            );
        }
    }

    fn publish_session_summary(&self) {
        let (Some(context), Some(incarnation), Some(record)) = (
            &self.session_run_context,
            self.session_incarnation,
            &self.session_record,
        ) else {
            return;
        };
        context.publish_run_summary(incarnation, record.summary());
    }
}

fn may_contain_template(prompt: &str) -> bool {
    prompt.contains('@') || prompt.contains('/')
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

fn tool_call_timeline_event(
    session_id: &SessionId,
    turn: u64,
    call: ToolCall,
) -> PendingTimelineEvent {
    PendingTimelineEvent::tool_call(session_id.clone(), turn, PendingTimelineItemId::New, call)
}

fn provider_metrics_timeline_event(
    session_id: &SessionId,
    turn: u64,
    metadata: &crate::ProviderTurnMetadata,
) -> PendingTimelineEvent {
    let usage = metadata.usage.unwrap_or_default();
    let cost = metadata.cost.as_ref();
    PendingTimelineEvent::new(
        session_id.clone(),
        turn,
        PendingTimelineItemId::New,
        TimelineEventKind::ItemFinished,
        Some(TimelineChannel::from(CHANNEL_METRICS)),
        TimelineContent::Json {
            value: json!({
                "kind": "turn_metrics",
                "provider": metadata.provider,
                "model": metadata.model,
                "usage": {
                    "inputTokens": usage.input_tokens.unwrap_or_default(),
                    "outputTokens": usage.output_tokens.unwrap_or_default(),
                    "cacheReadTokens": usage.cache_read_tokens.unwrap_or_default(),
                    "cacheWriteTokens": usage.cache_write_tokens.unwrap_or_default(),
                    "imageInputTokens": usage.image_input_tokens.unwrap_or_default(),
                    "imageInputCount": usage.image_input_count.unwrap_or_default(),
                    "totalTokens": usage.total_tokens(),
                    "totalInputTokens": usage.total_input_tokens(),
                    "contextTokens": usage.context_tokens(),
                },
                "cost": cost.map(|cost| json!({
                    "currency": cost.currency,
                    "inputMicros": cost.input_micros,
                    "outputMicros": cost.output_micros,
                    "cacheReadMicros": cost.cache_read_micros,
                    "cacheWriteMicros": cost.cache_write_micros,
                    "imageInputMicros": cost.image_input_micros,
                    "totalMicros": cost.total_micros,
                })),
            }),
        },
    )
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

fn canonical_request_json(draft: &ModelRequestDraft) -> Value {
    json!({
        "prompt": draft.prompt,
        "prompt_turns": draft.prompt_turns.iter().map(prompt_turn_json).collect::<Vec<_>>(),
        "system_prompt": draft.system_prompt,
        "tools": draft.tools.iter().map(tool_json).collect::<Vec<_>>(),
        "turns": draft.turns.iter().map(turn_json).collect::<Vec<_>>(),
    })
}

fn prompt_turn_json(prompt_turn: &PromptTurn) -> Value {
    json!({
        "turn": prompt_turn.turn,
        "prompt": prompt_turn.prompt,
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
        "content": result.content.blocks().iter().map(|block| match block {
            ToolResultContentBlock::Text { text } => json!({
                "type": "text",
                "text": text,
            }),
            ToolResultContentBlock::Image(image) => json!({
                "type": "image",
                "media_type": image.media_type(),
                "decoded_bytes": image.decoded_bytes(),
                "sha256": image.sha256(),
            }),
        }).collect::<Vec<_>>(),
        "is_error": result.is_error,
    })
}

fn redact_provider_images(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                redact_provider_images(value);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("image")
                && object
                    .get("source")
                    .and_then(Value::as_object)
                    .and_then(|source| source.get("type"))
                    .and_then(Value::as_str)
                    == Some("base64")
            {
                if let Some(source) = object.get_mut("source").and_then(Value::as_object_mut) {
                    redact_base64_field(source, "data");
                }
            } else if object.get("type").and_then(Value::as_str) == Some("input_image") {
                redact_data_url_field(object, "image_url");
            } else if object.get("type").and_then(Value::as_str) == Some("image_url")
                && let Some(image_url) = object.get_mut("image_url").and_then(Value::as_object_mut)
            {
                redact_data_url_field(image_url, "url");
            }
            for value in object.values_mut() {
                redact_provider_images(value);
            }
        }
        _ => {}
    }
}

fn redact_base64_field(object: &mut serde_json::Map<String, Value>, field: &str) {
    let Some(data) = object.remove(field) else {
        return;
    };
    object.insert(field.to_owned(), json!("[redacted inline image]"));
    if let Some((decoded_bytes, sha256)) = data.as_str().and_then(image_metadata) {
        object.insert("decoded_bytes".to_owned(), json!(decoded_bytes));
        object.insert("sha256".to_owned(), json!(sha256));
        object.insert("image_metadata_valid".to_owned(), json!(true));
    } else {
        object.insert("image_metadata_valid".to_owned(), json!(false));
    }
}

fn redact_data_url_field(object: &mut serde_json::Map<String, Value>, field: &str) {
    let Some(data_url) = object.remove(field) else {
        return;
    };
    object.insert(field.to_owned(), json!("[redacted inline image]"));
    let metadata = data_url
        .as_str()
        .and_then(|data_url| data_url.split_once(','))
        .and_then(|(header, data)| {
            let media_type = header.strip_prefix("data:")?.strip_suffix(";base64")?;
            let (decoded_bytes, sha256) = image_metadata(data)?;
            Some((media_type.to_owned(), decoded_bytes, sha256))
        });
    if let Some((media_type, decoded_bytes, sha256)) = metadata {
        object.insert("media_type".to_owned(), json!(media_type));
        object.insert("decoded_bytes".to_owned(), json!(decoded_bytes));
        object.insert("sha256".to_owned(), json!(sha256));
        object.insert("image_metadata_valid".to_owned(), json!(true));
    } else {
        object.insert("image_metadata_valid".to_owned(), json!(false));
    }
}

fn image_metadata(data: &str) -> Option<(usize, String)> {
    if data.len() > MAX_TOOL_RESULT_IMAGE_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let decoded = STANDARD.decode(data).ok()?;
    if decoded.len() > MAX_TOOL_RESULT_IMAGE_BYTES {
        return None;
    }
    let digest = Sha256::digest(&decoded);
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(sha256, "{byte:02x}");
    }
    Some((decoded.len(), sha256))
}

fn stable_request_hash(canonical_request: &Value) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical_request.to_string().hash(&mut hasher);
    format!("compat-{:016x}", hasher.finish())
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
        prompt_turns: draft.prompt_turns.clone(),
        turns: draft.turns.clone(),
    }
}

#[cfg(test)]
fn unsupported_before_provider_request_outcome(outcome: &HookOutcome) -> LlmError {
    LlmError::Protocol {
        provider: "middleware",
        message: format!(
            "unsupported before_provider_request middleware outcome during agent loop request construction: {outcome:?}"
        ),
    }
}

#[cfg(test)]
fn unsupported_build_system_prompt_outcome(outcome: &HookOutcome) -> LlmError {
    LlmError::Protocol {
        provider: "middleware",
        message: format!(
            "unsupported build_system_prompt middleware outcome during agent loop request construction: {outcome:?}"
        ),
    }
}

fn error_tool_result(call: &ToolCall, error: &LlmError) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        call.name.clone(),
        format!("Tool error: {error}\nArguments: {}", call.arguments),
    )
    .with_error()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{ToolResult, ToolResultContent, ToolResultContentBlock, ToolResultImage};

    use super::{redact_provider_images, tool_result_json};

    #[test]
    fn provider_request_audit_redacts_native_and_data_url_images() {
        let mut request = json!({
            "anthropic": {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgo="
                }
            },
            "openai": {
                "type": "input_image",
                "image_url": "data:image/png;base64,iVBORw0KGgo="
            }
        });

        redact_provider_images(&mut request);
        let encoded = request.to_string();

        assert!(!encoded.contains("iVBOR"));
        assert_eq!(request["anthropic"]["source"]["decoded_bytes"], 8);
        assert_eq!(request["openai"]["decoded_bytes"], 8);
        assert_eq!(
            request["anthropic"]["source"]["sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn provider_request_audit_fails_closed_for_malformed_images() {
        let mut request = json!({
            "invalid_base64": {
                "type": "image",
                "source": { "type": "base64", "data": "secret-not-base64" }
            },
            "non_string": {
                "type": "image",
                "source": { "type": "base64", "data": { "secret": true } }
            },
            "invalid_data_url": {
                "type": "input_image",
                "image_url": "secret-malformed-data-url"
            }
        });

        redact_provider_images(&mut request);
        let encoded = request.to_string();

        assert!(!encoded.contains("secret"));
        assert_eq!(
            request["invalid_base64"]["source"]["data"],
            "[redacted inline image]"
        );
        assert_eq!(
            request["non_string"]["source"]["image_metadata_valid"],
            false
        );
        assert_eq!(request["invalid_data_url"]["image_metadata_valid"], false);
    }

    #[test]
    fn canonical_request_audit_keeps_image_metadata_without_data() {
        let result = ToolResult::from_content(
            "call-image",
            "camera",
            ToolResultContent::from_blocks(vec![ToolResultContentBlock::Image(
                ToolResultImage::new("iVBORw0KGgo=", "image/png").expect("valid image"),
            )])
            .expect("bounded content"),
            false,
        );

        let audit = tool_result_json(&result);

        assert!(!audit.to_string().contains("iVBOR"));
        assert_eq!(audit["content"][0]["media_type"], "image/png");
        assert_eq!(audit["content"][0]["decoded_bytes"], 8);
        assert_eq!(audit["content"][0]["sha256"].as_str().unwrap().len(), 64);
    }
}
