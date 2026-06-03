//! Internal agent loop.

use std::collections::{HashSet, VecDeque};

use futures_util::StreamExt;

use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, LlmError, LlmStream, PermissionEngine,
    PermissionLevel, PermissionPolicy, Result, ThinkingBlock, ToolCall, ToolDefinition, ToolResult,
};

use super::{
    AgentEvent, AgentRunBuilder, AgentState, AgentTool, AgentToolContext, AgentTranscriptEvent,
};

pub(super) struct AgentLoopState<'a, P>
where
    P: LlmStream + ?Sized,
{
    provider: &'a P,
    prompt: String,
    system_prompt: Option<String>,
    tools: Vec<AgentTool>,
    tool_calls: Vec<ToolCall>,
    tool_results: Vec<ToolResult>,
    thinking_blocks: Vec<ThinkingBlock>,
    tool_context: AgentToolContext,
    state: AgentState,
    turn: usize,
    phase: AgentLoopPhase,
    pending: VecDeque<Result<AgentEvent>>,
    permission_engine: PermissionEngine,
    permission_policy: PermissionPolicy,
}

enum AgentLoopPhase {
    StartTurn,
    Streaming(BoxCompletionStream),
    Done,
}

impl<'a, P> AgentLoopState<'a, P>
where
    P: LlmStream + ?Sized,
{
    pub(super) fn from_run(run: AgentRunBuilder<'a, P>) -> Self {
        let permission_policy = run.permission_policy;
        let skills_revealed_roots = run
            .visible_skills
            .iter()
            .map(|skill| skill.skills_dir.clone())
            .collect();
        Self {
            provider: run.provider,
            prompt: run.prompt,
            system_prompt: run.system_prompt,
            tools: run.tools,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            thinking_blocks: Vec::new(),
            tool_context: AgentToolContext {
                agents_system_paths: run.agents_system_paths.into_iter().collect(),
                agents_loaded_paths: HashSet::new(),
                visible_skills: run.visible_skills,
                skills_enabled: run.skills_enabled,
                skills_revealed_roots,
                skills_loaded_paths: HashSet::new(),
            },
            state: AgentState::default(),
            turn: 0,
            phase: AgentLoopPhase::StartTurn,
            pending: VecDeque::new(),
            permission_engine: PermissionEngine::new(
                permission_policy.clone(),
                run.permission_handler,
            ),
            permission_policy,
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
                    let mut request = CompletionRequest::new(&self.prompt);
                    request.system_prompt = self.system_prompt.as_deref();
                    request.tools = self
                        .tools
                        .iter()
                        .filter(|tool| {
                            tool_is_surfaced(
                                &self.permission_policy,
                                tool.definition(),
                                self.permission_engine.has_handler(),
                            )
                        })
                        .map(|tool| tool.definition().clone())
                        .collect();
                    request.tool_calls = self.tool_calls.clone();
                    request.tool_results = self.tool_results.clone();
                    request.thinking_blocks = self.thinking_blocks.clone();

                    match self.provider.stream_request(request).await {
                        Ok(stream) => self.phase = AgentLoopPhase::Streaming(stream),
                        Err(error) => return Some((Err(error), self)),
                    }
                }
                AgentLoopPhase::Streaming(mut stream) => match stream.next().await {
                    Some(Ok(CompletionEvent::Delta(delta))) => {
                        self.state.events.push(AgentTranscriptEvent::Text {
                            turn: self.turn,
                            text: delta.clone(),
                        });
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
                        self.thinking_blocks.push(block.clone());
                        self.state.thinking_blocks.push(block);
                        self.phase = AgentLoopPhase::Streaming(stream);
                    }
                    Some(Ok(CompletionEvent::ToolCall(call))) => {
                        self.state.events.push(AgentTranscriptEvent::ToolCall {
                            turn: self.turn,
                            call: call.clone(),
                        });
                        self.tool_calls.push(call.clone());
                        let event = AgentEvent::ToolCall {
                            turn: self.turn,
                            call: call.clone(),
                        };
                        let result = self.execute_tool(&call).await;
                        self.tool_results.push(result.clone());
                        self.state.events.push(AgentTranscriptEvent::ToolResult {
                            turn: self.turn,
                            result: result.clone(),
                        });
                        self.pending.push_back(Ok(AgentEvent::ToolResult {
                            turn: self.turn,
                            result,
                        }));
                        self.pending
                            .push_back(Ok(AgentEvent::TurnFinished { turn: self.turn }));
                        self.turn += 1;
                        self.phase = AgentLoopPhase::StartTurn;
                        return Some((Ok(event), self));
                    }
                    Some(Ok(CompletionEvent::Finished)) | None => {
                        self.pending
                            .push_back(Ok(AgentEvent::TurnFinished { turn: self.turn }));
                        self.pending.push_back(Ok(AgentEvent::Finished {
                            state: self.state.clone(),
                        }));
                        self.phase = AgentLoopPhase::Done;
                    }
                    Some(Err(error)) => return Some((Err(error), self)),
                },
                AgentLoopPhase::Done => return None,
            }
        }
    }

    async fn execute_tool(&mut self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.definition().name == call.name)
        else {
            return error_tool_result(
                call,
                &LlmError::Protocol {
                    provider: "agent",
                    message: format!("unknown tool: {}", call.name),
                },
            );
        };

        if !tool_is_surfaced(
            &self.permission_policy,
            tool.definition(),
            self.permission_engine.has_handler(),
        ) {
            return error_tool_result(
                call,
                &LlmError::Protocol {
                    provider: "permission",
                    message: format!("tool is not permitted: {}", call.name),
                },
            );
        }

        let claims = match tool.claims(call) {
            Ok(claims) => claims,
            Err(error) => return error_tool_result(call, &error),
        };

        if let Err(error) = self.permission_engine.check(call, &claims) {
            return error_tool_result(call, &error);
        }

        match tool.call(call, &mut self.tool_context).await {
            Ok(content) => ToolResult::new(call.id.clone(), call.name.clone(), content),
            Err(error) => error_tool_result(call, &error),
        }
    }
}

fn tool_is_surfaced(
    policy: &PermissionPolicy,
    definition: &ToolDefinition,
    has_permission_handler: bool,
) -> bool {
    match policy.evaluate_tool(&definition.name) {
        PermissionLevel::Allow => true,
        PermissionLevel::Ask => has_permission_handler,
        PermissionLevel::Deny => false,
    }
}

fn error_tool_result(call: &ToolCall, error: &LlmError) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        call.name.clone(),
        format!("Tool error: {error}\nArguments: {}", call.arguments),
    )
}
