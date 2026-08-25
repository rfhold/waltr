//! Internal agent loop.

#![allow(clippy::significant_drop_tightening)]

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt::Write as _,
    hash::{Hash, Hasher},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::permission::{
    PERMISSION_STATE_NAMESPACE, PermissionApprovalStage, PermissionClaimIdentity,
    PermissionRuntimeState, permission_claim_identities,
};
use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, CompletionTurn, LlmError, LlmStream,
    MAX_TOOL_RESULT_IMAGE_BYTES, ModelRef, PendingTimelineEvent, PendingTimelineItemId,
    PermissionClaim, PermissionLevel, PermissionOperation, PermissionPattern, PermissionPolicy,
    PermissionResource, PermissionResourcePattern, PermissionResponse, PromptTurn, Result,
    TimelineChannel, TimelineContent, TimelineEventKind, TimelineItemId, ToolCall, ToolDefinition,
    ToolExecutionContext, ToolProgressSink, ToolResult, ToolResultContentBlock, TurnItem,
    middleware::{
        self as middleware, HookOutcome,
        chain::MiddlewareChain,
        context::{
            MiddlewareStateLog, ModelRequestDraft, RequestContext, RunContext, SystemPromptParts,
            ToolCallContext, ToolDraft, ToolResultContext,
        },
    },
    template::{TemplateRender, TemplateRuntime, TemplateSource},
    timeline::{AgentTimelineEvent, CHANNEL_EXTERNAL_REQUEST, CHANNEL_METRICS, TimelineStatus},
};
use tokio::{sync::mpsc, time::Sleep};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, Span};

use super::{
    AgentTool,
    session::{
        AgentSessionRuntime, EphemeralSessionRun, ExternalRequest, ExternalRequestId,
        ExternalRequestKind, ExternalRequestStatus, ExternalResponse, PendingWorkStatus, RequestId,
        RequestRecord, ResumeInput, Revision, RuntimeResumeOutput, SessionCreateRequest, SessionId,
        SessionIncarnation, SessionJournalEvent, SessionOperation, SessionRecord,
        SessionRollbackTarget, SessionRunContext, SessionSnapshot, SessionStart,
        SessionStartMessage, SessionStore, SessionStoreError,
        Suspension as SessionSuspension,
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
    permission_policy: PermissionPolicy,
    retry_config: ProviderRetryConfig,
    provider_attempt: u32,
    retry_checkpoint: Option<RetryCheckpoint>,
    session_id: SessionId,
    session_revision: Revision,
    session_journal_len: u64,
    last_timeline_cursor: Option<crate::timeline::TimelineCursor>,
    session_store: S,
    session_record: Option<SessionRecord>,
    session_run_context: Option<SessionRunContext>,
    session_incarnation: Option<SessionIncarnation>,
    session_initialized: bool,
    session_metadata_values: BTreeMap<String, Value>,
    middleware_state: MiddlewareStateLog,
    system_prompt_override: Option<middleware::prompt::SystemPromptOverride>,
    cancellation_token: CancellationToken,
    template_base_dir: Option<PathBuf>,
    max_provider_turns: Option<u64>,
    provider_turns_dispatched: u64,
    turn_span: Option<Span>,
    assistant_item_id: Option<(u64, TimelineItemId)>,
    thinking_item_id: Option<(u64, TimelineItemId)>,
    tool_item_ids: HashMap<String, TimelineItemId>,
    event_sink: Option<Arc<dyn crate::SessionEventSink>>,
}

#[derive(Debug, Clone)]
struct RetryCheckpoint {
    journal_len: u64,
    after_cursor: Option<crate::timeline::TimelineCursor>,
    turn: u64,
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
    call: ToolCall,
    result: ToolFuture,
    progress_rx: Option<mpsc::UnboundedReceiver<PendingTimelineEvent>>,
}

type ResolvedToolCall = (usize, ToolDefinition);

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
            ResumeInput::Prompts(prompts) => prompts.last().map_or("", String::as_str),
            ResumeInput::Continue
            | ResumeInput::ToolResults(_)
            | ResumeInput::ExternalResponse { .. } => "",
        }
        .to_owned();
        let permission_policy = run.permission_policy;
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
            permission_policy,
            retry_config: run.retry_config,
            provider_attempt: 1,
            retry_checkpoint: None,
            session_id: run.session_id,
            session_revision: Revision::ZERO,
            session_journal_len: 0,
            last_timeline_cursor: None,
            session_store: run.session_store,
            session_record: None,
            session_run_context: run.session_run_context,
            session_incarnation: None,
            session_initialized: false,
            session_metadata_values: BTreeMap::new(),
            middleware_state: MiddlewareStateLog::default(),
            system_prompt_override: run.system_prompt_override,
            cancellation_token: run.cancellation_token,
            template_base_dir: run.template_base_dir,
            max_provider_turns: run.max_provider_turns,
            provider_turns_dispatched: 0,
            turn_span: None,
            assistant_item_id: None,
            thinking_item_id: None,
            tool_item_ids: HashMap::new(),
            event_sink: run.event_sink,
        }
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::needless_continue, clippy::single_match_else)]
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
                    self.ensure_turn_span();
                    match self.initialize_session().await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => return Some((Err(error), self)),
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
                    let draft = match self.build_provider_request().await {
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
                AgentLoopPhase::Streaming(mut streaming) => match tokio::select! {
                    biased;

                    () = self.cancellation_token.cancelled() => {
                        if let Err(error) = self.record_interrupted().await {
                            return Some((Err(error), self));
                        }
                        self.phase = AgentLoopPhase::Done;
                        return None;
                    }
                    event = streaming.stream.next() => event,
                } {
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
                            metadata.cost = metadata
                                .usage
                                .and_then(|usage| self.model_cost.as_ref()?.cost_for_usage(usage));
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
                        let mut events = self.middleware_state.drain_pending();
                        events.push(SessionJournalEvent::TurnFinished {
                            turn: self.turn as u64,
                        });
                        events.push(SessionJournalEvent::Finished);
                        if let Err(error) = self.append_journal_events(events).await {
                            return Some((Err(error), self));
                        }
                        match self.append_final_assistant_snapshot().await {
                            Ok(Some(event)) => self.pending.push_back(Ok(event)),
                            Ok(None) => {}
                            Err(error) => return Some((Err(error), self)),
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
                },
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
                    }
                }
                AgentLoopPhase::ExecuteTools(mut batch) => {
                    let Some(call) = batch.calls.pop_front() else {
                        if let Err(error) = self.finish_tool_batch().await {
                            return Some((Err(error), self));
                        }
                        continue;
                    };
                    match self.start_tool(call).await {
                        Ok(ToolExecution::Running(running)) => {
                            self.phase = AgentLoopPhase::RunningTool(running, batch);
                            continue;
                        }
                        Ok(ToolExecution::Finished(Some(result))) => {
                            if let Err(error) = self.finish_tool_result(result).await {
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
                            result = &mut running.result => {
                                if let Some(progress_rx) = running.progress_rx.as_mut() {
                                    match self.append_timeline_events(drain_progress_events(progress_rx)).await {
                                        Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
                                        Err(error) => return Some((Err(error), self)),
                                    }
                                }
                                match self.finish_running_tool(running.call, result).await {
                                    Ok(true) => self.phase = AgentLoopPhase::ExecuteTools(batch),
                                    Ok(false) => {}
                                    Err(error) => return Some((Err(error), self)),
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
                        match self.finish_running_tool(running.call, result).await {
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
        let prompt_turns = self.prompt_turns();
        let middleware_chain = self.middleware_chain.clone();
        let mut ctx = RequestContext {
            middleware_state: &mut self.middleware_state,
            session_metadata_values: self.session_metadata_values.clone(),
            system_prompt_override: &mut self.system_prompt_override,
            middleware_chain: middleware_chain.clone(),
            permission_policy: self.permission_policy.clone(),
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
            HookOutcome::Suspend(suspension) => {
                self.suspend_from_hook(suspension).await?;
                return Ok(None);
            }
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
            outcome => Err(unsupported_before_provider_request_outcome(&outcome)),
        }
    }

    fn resolve_tool_call(&self, call: &ToolCall) -> Option<ResolvedToolCall> {
        let tool_index = self
            .tools
            .iter()
            .position(|tool| tool.definition().name == call.name)?;
        Some((tool_index, self.tools[tool_index].definition().clone()))
    }

    #[allow(clippy::too_many_lines)]
    async fn start_tool(&mut self, mut call: ToolCall) -> Result<ToolExecution> {
        let whole_tool_level = self.permission_policy.evaluate_tool(&call.name);
        let resolved = self.resolve_tool_call(&call);
        let whole_tool_request = PermissionRequestContext {
            session_id: self.session_id.as_str(),
            turn: self.turn as u64,
            stage: PermissionApprovalStage::WholeTool,
            policy: &self.permission_policy,
        };
        let whole_tool_request_id = permission_request_id(&whole_tool_request, &call, &[]);
        let whole_tool_approved = match whole_tool_level {
            PermissionLevel::Allow => false,
            PermissionLevel::Ask => {
                let state = self.permission_runtime_state()?;
                if state.has_one_shot(
                    &whole_tool_request_id,
                    &call.id,
                    PermissionApprovalStage::WholeTool,
                    &[],
                ) {
                    true
                } else {
                    let outcome = HookOutcome::Suspend(permission_suspension(
                        &whole_tool_request,
                        &call,
                        &[],
                    ));
                    return self
                        .tool_result_from_hook_outcome(&call, outcome)
                        .await
                        .map(ToolExecution::Finished);
                }
            }
            PermissionLevel::Deny => {
                return Ok(ToolExecution::Finished(Some(error_tool_result(
                    &call,
                    &LlmError::Protocol {
                        provider: "permission",
                        message: format!("tool is not permitted: {}", call.name),
                    },
                ))));
            }
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
                match self.permission_outcome(&call, &claims, whole_tool_approved)? {
                    PermissionOutcome::Allow => {}
                    PermissionOutcome::Deny(message) => {
                        return Ok(ToolExecution::Finished(Some(error_tool_result(
                            &call,
                            &LlmError::Protocol {
                                provider: "permission",
                                message,
                            },
                        ))));
                    }
                    PermissionOutcome::Ask(claims) => {
                        let request = PermissionRequestContext {
                            session_id: self.session_id.as_str(),
                            turn: self.turn as u64,
                            stage: PermissionApprovalStage::Contextual,
                            policy: &self.permission_policy,
                        };
                        let outcome =
                            HookOutcome::Suspend(permission_suspension(&request, &call, &claims));
                        return self
                            .tool_result_from_hook_outcome(&call, outcome)
                            .await
                            .map(ToolExecution::Finished);
                    }
                }
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

        match self.permission_outcome(&call, &claims, whole_tool_approved) {
            Ok(PermissionOutcome::Allow) => {
                if whole_tool_approved {
                    let mut state = self.permission_runtime_state()?;
                    if state.consume_one_shot(
                        &whole_tool_request_id,
                        &call.id,
                        PermissionApprovalStage::WholeTool,
                        &[],
                    ) {
                        self.set_permission_runtime_state(&state)?;
                    }
                }
                self.in_turn_span(|| tracing::debug!(tool_name = %call.name, tool_call_id = %call.id, claim_count = claims.len(), "tool permission allowed"));
                self.flush_middleware_state().await?;
            }
            Ok(PermissionOutcome::Deny(message)) => {
                self.in_turn_span(|| tracing::warn!(tool_name = %call.name, tool_call_id = %call.id, claim_count = claims.len(), "tool permission denied"));
                return Ok(ToolExecution::Finished(Some(error_tool_result(
                    &call,
                    &LlmError::Protocol {
                        provider: "permission",
                        message,
                    },
                ))));
            }
            Ok(PermissionOutcome::Ask(claims)) => {
                self.in_turn_span(|| tracing::info!(tool_name = %call.name, tool_call_id = %call.id, claim_count = claims.len(), "tool permission requested"));
                let request = PermissionRequestContext {
                    session_id: self.session_id.as_str(),
                    turn: self.turn as u64,
                    stage: PermissionApprovalStage::Contextual,
                    policy: &self.permission_policy,
                };
                let outcome = HookOutcome::Suspend(permission_suspension(&request, &call, &claims));
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
        let result = self.tools[tool_index].call(
            ToolExecutionContext::with_progress_and_cancellation(
                call.clone(),
                progress,
                self.cancellation_token.clone(),
            )
            .with_session_context(self.session_id.clone(), self.turn as u64),
        );
        let result = self.instrument_tool_future(&call, result);
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
    ) -> Result<bool> {
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

        let after_outcome = {
            let mut ctx = ToolResultContext {
                call: Some(&call),
                result: &mut result,
                middleware_state: &mut self.middleware_state,
                middleware_chain: self.middleware_chain.clone(),
                permission_policy: self.permission_policy.clone(),
                cancellation_token: self.cancellation_token.clone(),
                session_id: self.session_id.as_str(),
                turn: self.turn as u64,
            };
            self.middleware_chain.after_tool_call(&mut ctx).await
        };
        match after_outcome {
            Ok(HookOutcome::Continue) => {
                self.finish_tool_result(result).await?;
                Ok(true)
            }
            Ok(outcome) => {
                if let Some(result) = self.tool_result_from_hook_outcome(&call, outcome).await? {
                    self.finish_tool_result(result).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(error) => {
                self.finish_tool_result(error_tool_result(&call, &error))
                    .await?;
                Ok(true)
            }
        }
    }

    async fn finish_tool_result(&mut self, result: ToolResult) -> Result<()> {
        let parent_id = self.tool_item_id(&result.call_id)?;
        CompletionTurn::entry(&mut self.turns, self.turn as u64)
            .items
            .push(TurnItem::ToolResult(result.clone()));
        let events = self.middleware_state.drain_pending();
        self.append_journal_events(events).await?;
        let event = self
            .append_timeline_event(tool_result_timeline_event(
                &self.session_id,
                self.turn as u64,
                parent_id,
                result,
            ))
            .await?;
        self.pending.push_back(Ok(event));
        Ok(())
    }

    async fn finish_tool_batch(&mut self) -> Result<()> {
        self.append_journal_event(SessionJournalEvent::TurnFinished {
            turn: self.turn as u64,
        })
        .await?;
        self.close_turn_span("tools_finished");
        self.turn += 1;
        self.phase = AgentLoopPhase::StartTurn;
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
            HookOutcome::Suspend(suspension) => {
                self.in_turn_span(|| tracing::info!(tool_name = %call.name, tool_call_id = %call.id, "agent turn suspended"));
                if let Err(error) = self.suspend_from_hook(suspension).await {
                    return Ok(Some(error_tool_result(call, &error)));
                }
                Ok(None)
            }
        }
    }

    async fn suspend_from_hook(&mut self, mut suspension: middleware::Suspension) -> Result<()> {
        let mut ctx = RunContext {
            middleware_state: &mut self.middleware_state,
        };
        self.middleware_chain
            .on_suspend(&mut ctx, &mut suspension)
            .await?;
        if let Some(request) = suspension.external_request.clone() {
            let event = self.persist_external_request(&request).await?;
            let session_request = external_request_from_draft(&request);
            let mut events = self.middleware_state.drain_pending();
            events.push(SessionJournalEvent::Suspended {
                suspension: SessionSuspension::AwaitingExternalInput {
                    request: Box::new(session_request),
                },
            });
            self.append_journal_events(events).await?;
            self.pending.push_back(Ok(event));
        }
        self.phase = AgentLoopPhase::Done;
        self.close_turn_span("suspended");
        Ok(())
    }

    fn permission_outcome(
        &mut self,
        tool_call: &ToolCall,
        claims: &[PermissionClaim],
        _whole_tool_approved: bool,
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

        let input = self
            .permission_policy
            .evaluate_input(&tool_call.name, &tool_call.arguments);
        if input
            .as_ref()
            .is_some_and(|evaluation| evaluation.effect.level == PermissionLevel::Deny)
        {
            return Ok(PermissionOutcome::Deny(
                "permission denied: tool input does not satisfy policy".to_owned(),
            ));
        }

        let mut state = self.permission_runtime_state()?;
        let input_claims = input
            .as_ref()
            .filter(|input| input.effect.level == PermissionLevel::Ask)
            .map_or(&[][..], |input| input.claims.as_slice());
        let unmatched = claims
            .iter()
            .filter(|claim| {
                self.permission_policy
                    .evaluate_claim_refinement_effect(&tool_call.name, claim)
                    .is_some_and(|effect| effect.level == PermissionLevel::Ask)
            })
            .cloned()
            .chain(input_claims.iter().cloned())
            .filter(|claim| !state.patterns.iter().any(|pattern| pattern.matches(claim)))
            .collect::<Vec<_>>();
        let identities = permission_claim_identities(&unmatched);
        let request_id = permission_request_id(
            &PermissionRequestContext {
                session_id: self.session_id.as_str(),
                turn: self.turn as u64,
                stage: PermissionApprovalStage::Contextual,
                policy: &self.permission_policy,
            },
            tool_call,
            &identities,
        );
        if state.consume_one_shot(
            &request_id,
            &tool_call.id,
            PermissionApprovalStage::Contextual,
            &identities,
        ) {
            self.set_permission_runtime_state(&state)?;
            return Ok(PermissionOutcome::Allow);
        }
        if unmatched.is_empty() {
            return Ok(PermissionOutcome::Allow);
        }
        Ok(PermissionOutcome::Ask(unmatched))
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
                if let Some(operation) = &operation {
                    operation.register_loaded_summary(record.summary());
                }
                self.session_incarnation = incarnation;
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
        let prompts = match input {
            ResumeInput::Prompt(prompt) => vec![prompt],
            ResumeInput::Prompts(prompts) if !prompts.is_empty() => prompts,
            ResumeInput::Prompts(_) => {
                return Err(session_store_error("prompt batch must not be empty"));
            }
            ResumeInput::Continue
            | ResumeInput::ToolResults(_)
            | ResumeInput::ExternalResponse { .. } => {
                return Err(session_store_error(
                    "cannot resume missing session without a prompt input",
                ));
            }
        };
        let Some(prompts) = self.render_prompt_templates(prompts).await? else {
            return Ok(false);
        };
        self.prompt = prompts.last().cloned().unwrap_or_default();
        let messages = prompts
            .iter()
            .cloned()
            .map(SessionStartMessage::user)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| session_store_error(error.to_string()))?;
        let mut request = SessionCreateRequest::new(self.session_id.clone());
        request.start = SessionStart::Fresh { messages };
        let record = self
            .session_store
            .create(request)
            .await
            .map_err(session_store_error)?;
        if let Some(operation) = &operation {
            operation.publish_created(record.summary());
        }
        self.session_incarnation = incarnation;
        self.session_metadata_values = record.metadata.values.clone();
        self.session_journal_len = record.journal.len() as u64;
        self.last_timeline_cursor = record.timeline_events().last().map(|event| event.cursor);
        let events = record.timeline_events().cloned().collect::<Vec<_>>();
        if let Some(context) = &self.session_run_context {
            context.timeline().publish_all(events.clone());
        }
        self.session_record = Some(record.clone());
        let turn = self.turn as u64;
        self.prompt_turns = prompts
            .iter()
            .map(|prompt| PromptTurn {
                turn,
                prompt: prompt.clone(),
            })
            .collect();
        self.publish_user_message_events(0, 0, &prompts);
        self.pending.extend(events.into_iter().map(Ok));
        self.session_initialized = true;
        Ok(true)
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_resume_input(&mut self, record: SessionRecord) -> Result<bool> {
        let Some(input) = self.input.take() else {
            self.hydrate_from_record(&record);
            return Ok(true);
        };

        let mut continue_start_turn = true;
        let record = match input {
            ResumeInput::Continue => record,
            ResumeInput::Prompt(prompt) => {
                let prompt = match self.render_prompt_templates(vec![prompt]).await? {
                    Some(mut prompts) => prompts.remove(0),
                    None => return Ok(false),
                };
                let snapshot = record.snapshot();
                if snapshot.current_prompt.as_deref() == Some(prompt.as_str())
                    && snapshot.prompts.len() == 1
                    && snapshot.turns.is_empty()
                {
                    record
                } else {
                    match self
                        .session_runtime()
                        .resume(&self.session_id, ResumeInput::Prompt(prompt))
                        .await?
                    {
                        RuntimeResumeOutput::Resumed { record }
                        | RuntimeResumeOutput::Suspended { record, .. } => record,
                    }
                }
            }
            ResumeInput::Prompts(prompts) => {
                let Some(prompts) = self.render_prompt_templates(prompts).await? else {
                    return Ok(false);
                };
                match self
                    .session_runtime()
                    .resume(&self.session_id, ResumeInput::Prompts(prompts))
                    .await?
                {
                    RuntimeResumeOutput::Resumed { record }
                    | RuntimeResumeOutput::Suspended { record, .. } => record,
                }
            }
            ResumeInput::ExternalResponse {
                request_id,
                response,
            } => {
                let retry = pending_tool_batch_for_permission_retry(
                    &record.snapshot(),
                    &request_id,
                    &response,
                )?;
                let cancel = permission_cancel_response(&record.snapshot(), &request_id, &response);
                let record = match self
                    .session_runtime()
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
                if !retry.is_empty() {
                    self.phase = AgentLoopPhase::ExecuteTools(ToolBatch {
                        calls: retry.into(),
                    });
                    continue_start_turn = false;
                } else if cancel {
                    self.phase = AgentLoopPhase::Done;
                    continue_start_turn = false;
                }
                record
            }
            ResumeInput::ToolResults(results) => {
                match self
                    .session_runtime()
                    .resume(&self.session_id, ResumeInput::ToolResults(results))
                    .await?
                {
                    RuntimeResumeOutput::Resumed { record }
                    | RuntimeResumeOutput::Suspended { record, .. } => record,
                }
            }
        };

        self.hydrate_from_record(&record);
        Ok(continue_start_turn)
    }

    fn hydrate_from_record(&mut self, record: &SessionRecord) {
        let snapshot = record.snapshot();
        self.middleware_state = MiddlewareStateLog::from_snapshot(&snapshot);
        self.session_metadata_values = record.metadata.values.clone();
        self.prompt = snapshot.current_prompt.unwrap_or_default();
        self.prompt_turns = snapshot.prompt_turns;
        self.turns = snapshot.turns;
        self.turn = usize::try_from(snapshot.turn).unwrap_or(usize::MAX);
        self.session_revision = record.revision;
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
    ) -> Result<Option<Vec<String>>> {
        let current_dir;
        let base_dir = if let Some(base_dir) = self.template_base_dir.as_deref() {
            base_dir
        } else {
            current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            current_dir.as_path()
        };
        let mut rendered = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            let mut runtime = TemplateRuntime {
                base_dir,
                middleware_chain: &self.middleware_chain,
                permission_policy: &self.permission_policy,
                middleware_state: &mut self.middleware_state,
                cancellation_token: self.cancellation_token.clone(),
                session_id: self.session_id.as_str(),
                turn: self.turn as u64,
                source: TemplateSource::UserPrompt,
            };
            match runtime.render_template(&prompt).await? {
                TemplateRender::Rendered(prompt) => rendered.push(prompt),
                TemplateRender::Suspend(suspension) => {
                    self.suspend_from_hook(suspension).await?;
                    return Ok(None);
                }
            }
        }
        Ok(Some(rendered))
    }

    async fn append_journal_event(&mut self, event: SessionJournalEvent) -> Result<()> {
        self.append_journal_events(vec![event]).await
    }

    async fn append_journal_events(&mut self, events: Vec<SessionJournalEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let event_count = events.len() as u64;
        let projected_events = events.clone();
        self.session_revision = self
            .session_store
            .append_journal(&self.session_id, self.session_revision, events)
            .await
            .map_err(session_store_error)?;
        self.session_journal_len = self.session_journal_len.saturating_add(event_count);
        if let Some(record) = &mut self.session_record {
            record.revision = self.session_revision;
            record.append_journal_events(projected_events);
            record.touch_activity();
        }
        self.publish_session_summary();
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
        let record = self.session_runtime().interrupt(&self.session_id).await?;
        self.hydrate_from_record(&record);
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
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let assigned = self
            .session_store
            .append_timeline(&self.session_id, self.session_revision, events)
            .await
            .map_err(session_store_error)?;
        if let Some(context) = &self.session_run_context {
            context.timeline().publish_all(assigned.clone());
        }
        self.session_revision = self.session_revision.next();
        self.session_journal_len = self
            .session_journal_len
            .saturating_add(assigned.len() as u64);
        self.last_timeline_cursor = assigned
            .last()
            .map(|event| event.cursor)
            .or(self.last_timeline_cursor);
        if let Some(record) = &mut self.session_record {
            record.revision = self.session_revision;
            record
                .append_journal_events(assigned.iter().cloned().map(SessionJournalEvent::Timeline));
            record.touch_activity();
        }
        self.publish_session_summary();
        Ok(assigned)
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

    async fn pending_failed_session_event(&mut self, error: &LlmError) {
        if self.session_initialized {
            drop(
                self.append_journal_event(SessionJournalEvent::Failed {
                    message: error.to_string(),
                    authentication_failure: error.authentication_failure().cloned(),
                })
                .await,
            );
        }
        self.close_turn_span("error");
    }

    async fn retry_or_fail_provider_error(&mut self, error: LlmError) -> Result<()> {
        let Some(decision) = self.retry_config.classify(&error, self.provider_attempt) else {
            self.pending_failed_session_event(&error).await;
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
            .session_store
            .load(&self.session_id)
            .await
            .map_err(session_store_error)?;
        self.hydrate_from_record(&record);
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
            PendingTimelineItemId::New,
        ))
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

fn external_request_timeline_event(
    session_id: &SessionId,
    turn: u64,
    request: ExternalRequest,
    kind: TimelineEventKind,
    item_id: PendingTimelineItemId,
) -> PendingTimelineEvent {
    PendingTimelineEvent::new(
        session_id.clone(),
        turn,
        item_id,
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

struct PermissionRequestContext<'a> {
    session_id: &'a str,
    turn: u64,
    stage: PermissionApprovalStage,
    policy: &'a PermissionPolicy,
}

fn permission_suspension(
    request: &PermissionRequestContext<'_>,
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
                    "resource": permission_claim_resource(&claim.resource),
                    "summary": claim.summary,
                })
            })
            .collect::<Vec<_>>(),
        "patterns": permission_patterns(claims),
        "claim_identities": permission_claim_identities(claims),
    });
    middleware::Suspension::external(
        "permission",
        middleware::ExternalRequestDraft {
            id: permission_request_id(request, call, &permission_claim_identities(claims)),
            kind: middleware::ExternalRequestKind::Permission,
            tool_call_id: Some(call.id.clone()),
            prompt: summary,
            payload,
            status: middleware::ExternalRequestStatus::Pending,
        },
    )
}

fn permission_request_id(
    request: &PermissionRequestContext<'_>,
    call: &ToolCall,
    claims: &[PermissionClaimIdentity],
) -> String {
    let identity = serde_json::to_string(&(request.stage, request.policy.identity(), claims))
        .expect("permission request identity serializes");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in identity.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(
        "permission:{}:{}:{}:{hash:016x}",
        request.session_id, request.turn, call.id
    )
}

fn permission_patterns(claims: &[PermissionClaim]) -> Vec<PermissionPattern> {
    let mut patterns = Vec::new();
    for claim in claims {
        let pattern = permission_pattern(claim);
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}

fn permission_pattern(claim: &PermissionClaim) -> PermissionPattern {
    let resource = match &claim.resource {
        PermissionResource::FilesystemPath(path) => {
            let parent = path.parent().unwrap_or(path.as_path()).to_path_buf();
            PermissionResourcePattern::FilesystemPathPrefix(parent)
        }
        PermissionResource::FilesystemPathPrefix(path) => {
            PermissionResourcePattern::FilesystemPathPrefix(path.clone())
        }
        PermissionResource::WorkingDirectory(path) => {
            PermissionResourcePattern::WorkingDirectoryPrefix(path.clone())
        }
        PermissionResource::ShellCommand(command) => {
            return PermissionPattern {
                operation: Some(PermissionOperation::Execute),
                resource: PermissionResourcePattern::ShellCommand(command.clone()),
            };
        }
        PermissionResource::ToolInput {
            tool,
            pointer,
            value,
        } => PermissionResourcePattern::ToolInput {
            tool: tool.clone(),
            pointer: pointer.clone(),
            value: value.clone(),
        },
    };
    PermissionPattern {
        operation: Some(claim.operation),
        resource,
    }
}

fn pending_tool_batch_for_permission_retry(
    snapshot: &SessionSnapshot,
    request_id: &ExternalRequestId,
    response: &ExternalResponse,
) -> Result<Vec<ToolCall>> {
    let Some(request) = snapshot
        .external_requests
        .iter()
        .rev()
        .find(|request| request.id == *request_id)
    else {
        return Ok(Vec::new());
    };
    if request.kind != ExternalRequestKind::Permission {
        return Ok(Vec::new());
    }
    let response = PermissionResponse::from_value(response.payload.clone())?;
    if !matches!(
        response,
        PermissionResponse::AllowOnce | PermissionResponse::AllowAlways { .. }
    ) {
        return Ok(Vec::new());
    }
    if request
        .payload
        .get("template")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Vec::new());
    }
    let Some(tool_call_id) = request.tool_call_id.as_deref() else {
        return Err(session_store_error(
            "permission request missing tool call id",
        ));
    };
    if !snapshot
        .pending_work
        .iter()
        .any(|work| work.id == tool_call_id && work.tool_call.is_some())
    {
        return Err(session_store_error(format!(
            "pending permission work not found for tool call: {tool_call_id}"
        )));
    }

    Ok(snapshot
        .pending_work
        .iter()
        .filter(|work| matches!(work.status, PendingWorkStatus::Pending))
        .filter_map(|work| work.tool_call.clone())
        .collect())
}

fn permission_cancel_response(
    _snapshot: &SessionSnapshot,
    _request_id: &ExternalRequestId,
    response: &ExternalResponse,
) -> bool {
    match PermissionResponse::from_value(response.payload.clone()) {
        Ok(PermissionResponse::Deny {
            mode: crate::PermissionDenyMode::CancelWork,
            ..
        }) => true,
        Ok(_) | Err(_) => false,
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

fn permission_claim_resource(resource: &PermissionResource) -> String {
    match resource {
        PermissionResource::ToolInput { tool, pointer, .. } => {
            format!("ToolInput {{ tool: {tool:?}, pointer: {pointer:?} }}")
        }
        resource => format!("{resource:?}"),
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
        prompt_turns: draft.prompt_turns.clone(),
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
