//! Agent configuration and loop execution.

use std::{
    collections::{HashSet, VecDeque},
    fmt,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};

use futures_util::{Stream, StreamExt, stream};

use crate::{
    BoxCompletionStream, CompletionEvent, CompletionRequest, LlmError, LlmStream, ModelRef,
    PermissionClaim, PermissionEngine, PermissionLevel, PermissionPolicy, Result,
    SharedPermissionHandler, SystemPromptBuilder, ThinkingBlock, ToolCall, ToolDefinition,
    ToolResult,
    mcp::McpConfig,
    prompt::{AgentsPromptConfig, FilesystemPromptConfig},
    tools::{
        filesystem::FilesystemConfig,
        filesystem_skills::{SkillDefinition, SkillsConfig, render_skills_catalog},
        question::QuestionConfig,
    },
};

#[cfg(feature = "shell")]
use crate::{prompt::ShellPromptConfig, tools::shell::ShellConfig};

/// Boxed stream of agent loop events.
pub type BoxAgentEventStream<'a> =
    std::pin::Pin<Box<dyn Stream<Item = Result<AgentEvent>> + Send + 'a>>;

/// Agent configuration and enabled capabilities.
#[derive(Clone)]
pub struct Agent {
    model: ModelRef,
    tools: Vec<AgentTool>,
    system_prompt: Option<String>,
    agents_system_paths: Vec<PathBuf>,
    visible_skills: Vec<SkillDefinition>,
    skills_enabled: bool,
    permission_policy: PermissionPolicy,
    permission_handler: Option<SharedPermissionHandler>,
}

impl Agent {
    /// Start building an agent for a model.
    #[must_use]
    pub fn builder(model: ModelRef) -> AgentBuilder {
        AgentBuilder::new(model)
    }

    /// Return the model identity configured for this agent.
    #[must_use]
    pub const fn model(&self) -> &ModelRef {
        &self.model
    }

    /// Start building an agent loop request.
    pub fn prompt<'a, P>(
        &'a self,
        provider: &'a P,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<'a, P>
    where
        P: LlmStream + ?Sized,
    {
        AgentRunBuilder::new(self, provider, prompt.into())
    }

    /// Return configured host tools.
    #[must_use]
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("system_prompt", &self.system_prompt)
            .field("agents_system_paths", &self.agents_system_paths)
            .field("visible_skills", &self.visible_skills)
            .field("skills_enabled", &self.skills_enabled)
            .field("permission_policy", &self.permission_policy)
            .field("has_permission_handler", &self.permission_handler.is_some())
            .finish()
    }
}

/// Builder for an agent configuration.
#[derive(Clone)]
pub struct AgentBuilder {
    model: ModelRef,
    tools: Vec<AgentTool>,
    system_prompt: SystemPromptBuilder,
    agents_system_paths: Vec<PathBuf>,
    visible_skills: Vec<SkillDefinition>,
    skills_enabled: bool,
    permission_policy: PermissionPolicy,
    explicit_permission_policy: bool,
    permission_handler: Option<SharedPermissionHandler>,
}

impl AgentBuilder {
    /// Create a builder for a model.
    #[must_use]
    pub fn new(model: ModelRef) -> Self {
        Self {
            model,
            tools: Vec::new(),
            system_prompt: SystemPromptBuilder::new(),
            agents_system_paths: Vec::new(),
            visible_skills: Vec::new(),
            skills_enabled: false,
            permission_policy: PermissionPolicy::default(),
            explicit_permission_policy: false,
            permission_handler: None,
        }
    }

    /// Opt into filesystem tools as generic host tools.
    #[must_use]
    pub fn with_filesystem_tools(mut self, config: FilesystemConfig) -> Self {
        if !self.explicit_permission_policy {
            self.permission_policy = self
                .permission_policy
                .merged(PermissionPolicy::filesystem_default(config.root.clone()));
        }
        self.tools.extend(config.into_agent_tools(&self.model));
        self
    }

    /// Opt into the reusable base system prompt.
    #[must_use]
    pub fn with_base_system_prompt(mut self) -> Self {
        self.system_prompt = self.system_prompt.with_base_prompt();
        self
    }

    /// Append a system prompt fragment to this agent.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = self.system_prompt.with_fragment(prompt);
        self
    }

    /// Append deterministic date context to this agent's system prompt.
    #[must_use]
    pub fn with_system_date(mut self, date: impl Into<String>) -> Self {
        self.system_prompt = self.system_prompt.with_date(date);
        self
    }

    /// Opt into filesystem system prompt context.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_filesystem_system_prompt(mut self, config: FilesystemPromptConfig) -> Self {
        self.system_prompt = self.system_prompt.with_fragment(config.system_prompt());
        self
    }

    /// Opt into static `AGENTS.md` system prompt context.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_agents_system_prompt(mut self, config: AgentsPromptConfig) -> Self {
        let instructions = config.instructions();
        for instruction in instructions {
            self.agents_system_paths.push(instruction.path);
            self.system_prompt = self.system_prompt.with_fragment(instruction.content);
        }
        self
    }

    /// Opt into Agent Skills catalog and the dedicated skill loading tool.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_skills(mut self, config: SkillsConfig) -> Self {
        let skills = config.discover();
        if let Some(catalog) = render_skills_catalog(&skills, "Available skills:") {
            self.system_prompt = self.system_prompt.with_fragment(catalog);
        }
        self.visible_skills.extend(skills);
        self.skills_enabled = true;
        self.tools.push(config.into_agent_tool());
        self
    }

    /// Opt into configured MCP servers as host tools.
    ///
    /// # Errors
    ///
    /// Returns an error if any MCP server cannot be initialized or its tools cannot be listed.
    #[allow(clippy::needless_pass_by_value)]
    pub async fn with_mcp_servers(mut self, config: McpConfig) -> Result<Self> {
        self.tools.extend(config.into_agent_tools().await?);
        Ok(self)
    }

    /// Opt into the structured user question tool.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_question_tool(mut self, config: QuestionConfig) -> Self {
        self.tools.push(config.into_agent_tool());
        self
    }

    /// Opt into the shell tool as a generic host tool.
    #[cfg(feature = "shell")]
    #[must_use]
    pub fn with_shell_tool(mut self, config: ShellConfig) -> Self {
        if !self.explicit_permission_policy {
            self.permission_policy = self
                .permission_policy
                .merged(PermissionPolicy::filesystem_default(config.root.clone()));
        }
        self.tools.push(config.into_agent_tool());
        self
    }

    /// Opt into shell system prompt context.
    #[cfg(feature = "shell")]
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_shell_system_prompt(mut self, config: ShellPromptConfig) -> Self {
        self.system_prompt = self.system_prompt.with_fragment(config.system_prompt());
        self
    }

    /// Add a host tool to the agent.
    #[must_use]
    pub fn with_tool(mut self, tool: AgentTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Add a permission request handler for contextual ask decisions.
    #[must_use]
    pub fn with_permission_handler(
        mut self,
        handler: impl Fn(&crate::PermissionRequest) -> crate::PermissionDecision + Send + Sync + 'static,
    ) -> Self {
        self.permission_handler = Some(Arc::new(handler));
        self
    }

    /// Set the permission policy for this agent.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self.explicit_permission_policy = true;
        self
    }

    /// Build the agent configuration.
    #[must_use]
    pub fn build(self) -> Agent {
        Agent {
            model: self.model,
            tools: self.tools,
            system_prompt: self.system_prompt.build(),
            agents_system_paths: self.agents_system_paths,
            visible_skills: self.visible_skills,
            skills_enabled: self.skills_enabled,
            permission_policy: self.permission_policy,
            permission_handler: self.permission_handler,
        }
    }
}

impl fmt::Debug for AgentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBuilder")
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("system_prompt", &self.system_prompt)
            .field("agents_system_paths", &self.agents_system_paths)
            .field("visible_skills", &self.visible_skills)
            .field("skills_enabled", &self.skills_enabled)
            .field("permission_policy", &self.permission_policy)
            .field(
                "explicit_permission_policy",
                &self.explicit_permission_policy,
            )
            .field("has_permission_handler", &self.permission_handler.is_some())
            .finish()
    }
}

/// Host tool available to an agent loop.
#[derive(Clone)]
pub struct AgentTool {
    definition: ToolDefinition,
    handler: Arc<ToolHandler>,
    claim_extractor: Arc<ClaimExtractor>,
}

type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
type ToolHandler =
    dyn for<'a> Fn(&'a ToolCall, &'a mut AgentToolContext) -> ToolFuture<'a> + Send + Sync;
type ClaimExtractor = dyn Fn(&ToolCall) -> Result<Vec<PermissionClaim>> + Send + Sync;

/// Mutable context shared across tool executions in one agent run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentToolContext {
    /// `AGENTS.md` files already loaded into the static system prompt.
    pub agents_system_paths: HashSet<PathBuf>,
    /// `AGENTS.md` files dynamically loaded by tools in this run.
    pub agents_loaded_paths: HashSet<PathBuf>,
    /// Agent Skills visible to the skill loading tool in this run.
    pub visible_skills: Vec<SkillDefinition>,
    /// Whether Agent Skills support is enabled for this run.
    pub skills_enabled: bool,
    /// `.agents/skills` roots already revealed by tools in this run.
    pub skills_revealed_roots: HashSet<PathBuf>,
    /// `SKILL.md` files already loaded by the skill loading tool in this run.
    pub skills_loaded_paths: HashSet<PathBuf>,
}

impl AgentTool {
    /// Create a host tool from a definition and synchronous handler.
    pub fn new(
        definition: ToolDefinition,
        handler: impl Fn(&ToolCall) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call, _context| Box::pin(std::future::ready(handler(call)))),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Create a host tool from a definition and asynchronous handler.
    pub fn new_async(
        definition: ToolDefinition,
        handler: impl for<'a> Fn(&'a ToolCall) -> ToolFuture<'a> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call, _context| handler(call)),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Create a host tool with access to mutable per-run tool context.
    pub fn with_context(
        definition: ToolDefinition,
        handler: impl Fn(&ToolCall, &mut AgentToolContext) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(move |call, context| {
                Box::pin(std::future::ready(handler(call, context)))
            }),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Create a host tool with asynchronous access to mutable per-run tool context.
    pub fn with_context_async(
        definition: ToolDefinition,
        handler: impl for<'a> Fn(&'a ToolCall, &'a mut AgentToolContext) -> ToolFuture<'a>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Arc::new(handler),
            claim_extractor: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Add a contextual permission claim extractor.
    #[must_use]
    pub fn with_permission_claims(
        mut self,
        extractor: impl Fn(&ToolCall) -> Result<Vec<PermissionClaim>> + Send + Sync + 'static,
    ) -> Self {
        self.claim_extractor = Arc::new(extractor);
        self
    }

    /// Return the provider-facing tool definition.
    #[must_use]
    pub const fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn call(&self, call: &ToolCall, context: &mut AgentToolContext) -> Result<String> {
        (self.handler)(call, context).await
    }

    fn claims(&self, call: &ToolCall) -> Result<Vec<PermissionClaim>> {
        (self.claim_extractor)(call)
    }
}

impl fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// Ordered agent stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// Thinking/reasoning text emitted by the model in a specific turn.
    ThinkingDelta {
        /// Model turn that emitted this thinking text.
        turn: usize,
        /// Incremental thinking/reasoning text emitted by the model.
        delta: String,
    },
    /// Text emitted by the model in a specific turn.
    TextDelta {
        /// Model turn that emitted this text.
        turn: usize,
        /// Incremental text emitted by the model.
        delta: String,
    },
    /// Tool call requested by the model in a specific turn.
    ToolCall {
        /// Model turn that requested the tool call.
        turn: usize,
        /// Tool call requested by the model.
        call: ToolCall,
    },
    /// Tool result returned by the host in a specific turn.
    ToolResult {
        /// Model turn that requested the corresponding tool call.
        turn: usize,
        /// Tool result returned by the host.
        result: ToolResult,
    },
    /// Current model turn finished.
    TurnFinished {
        /// Model turn that finished.
        turn: usize,
    },
    /// Agent loop finished with final state.
    Finished {
        /// Final accumulated agent state.
        state: AgentState,
    },
}

/// Accumulated agent state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentState {
    /// Ordered transcript events accumulated by the agent loop.
    pub events: Vec<AgentTranscriptEvent>,
    /// Provider-specific thinking/reasoning blocks accumulated for replay.
    pub thinking_blocks: Vec<ThinkingBlock>,
}

impl AgentState {
    /// Return concatenated assistant text from the last turn containing text.
    #[must_use]
    pub fn last_assistant_text(&self) -> String {
        let Some(last_text_turn) = self.events.iter().rev().find_map(|event| match event {
            AgentTranscriptEvent::Text { turn, .. } => Some(*turn),
            AgentTranscriptEvent::Thinking { .. }
            | AgentTranscriptEvent::ToolCall { .. }
            | AgentTranscriptEvent::ToolResult { .. } => None,
        }) else {
            return String::new();
        };

        self.events
            .iter()
            .filter_map(|event| match event {
                AgentTranscriptEvent::Text { turn, text } if *turn == last_text_turn => {
                    Some(text.as_str())
                }
                AgentTranscriptEvent::Text { .. }
                | AgentTranscriptEvent::Thinking { .. }
                | AgentTranscriptEvent::ToolCall { .. }
                | AgentTranscriptEvent::ToolResult { .. } => None,
            })
            .collect()
    }
}

/// Ordered persisted agent transcript event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTranscriptEvent {
    /// Thinking/reasoning emitted by the model.
    Thinking {
        /// Model turn that emitted this thinking text.
        turn: usize,
        /// Thinking/reasoning text delta emitted by the model.
        text: String,
    },
    /// Text emitted by the model.
    Text {
        /// Model turn that emitted this text.
        turn: usize,
        /// Text delta emitted by the model.
        text: String,
    },
    /// Tool call requested by the model.
    ToolCall {
        /// Model turn that requested the tool call.
        turn: usize,
        /// Tool call requested by the model.
        call: ToolCall,
    },
    /// Tool result returned by the host.
    ToolResult {
        /// Model turn that requested the corresponding tool call.
        turn: usize,
        /// Tool result returned by the host.
        result: ToolResult,
    },
}

/// Builder for an agent loop request.
pub struct AgentRunBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    provider: &'a P,
    prompt: String,
    system_prompt: Option<String>,
    tools: Vec<AgentTool>,
    agents_system_paths: Vec<PathBuf>,
    visible_skills: Vec<SkillDefinition>,
    skills_enabled: bool,
    permission_policy: PermissionPolicy,
    permission_handler: Option<SharedPermissionHandler>,
}

impl<'a, P> AgentRunBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    fn new(agent: &'a Agent, provider: &'a P, prompt: String) -> Self {
        Self {
            provider,
            prompt,
            system_prompt: agent.system_prompt.clone(),
            tools: agent.tools.clone(),
            agents_system_paths: agent.agents_system_paths.clone(),
            visible_skills: agent.visible_skills.clone(),
            skills_enabled: agent.skills_enabled,
            permission_policy: agent.permission_policy.clone(),
            permission_handler: agent.permission_handler.clone(),
        }
    }

    /// Add one tool to this run.
    #[must_use]
    pub fn with_tool(mut self, tool: AgentTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Replace the agent-assembled system prompt for this run.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        self.system_prompt = if prompt.trim().is_empty() {
            None
        } else {
            Some(prompt.trim().to_owned())
        };
        self
    }

    /// Append a system prompt fragment for this run.
    #[must_use]
    pub fn with_system_prompt_fragment(mut self, prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return self;
        }
        self.system_prompt = Some(self.system_prompt.map_or_else(
            || prompt.to_owned(),
            |existing| format!("{existing}\n\n{prompt}"),
        ));
        self
    }

    /// Override or add a permission request handler for this run.
    #[must_use]
    pub fn with_permission_handler(
        mut self,
        handler: impl Fn(&crate::PermissionRequest) -> crate::PermissionDecision + Send + Sync + 'static,
    ) -> Self {
        self.permission_handler = Some(Arc::new(handler));
        self
    }

    /// Override the permission policy for this run.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self
    }

    /// Stream the full ordered agent timeline.
    #[must_use]
    pub fn stream(self) -> BoxAgentEventStream<'a> {
        Box::pin(stream::unfold(
            AgentLoopState::from_run(self),
            AgentLoopState::next,
        ))
    }
}

impl<P> fmt::Debug for AgentRunBuilder<'_, P>
where
    P: LlmStream + ?Sized,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRunBuilder")
            .field("provider", &self.provider.name())
            .field("prompt", &self.prompt)
            .field("system_prompt", &self.system_prompt)
            .field("tools", &self.tools)
            .field("agents_system_paths", &self.agents_system_paths)
            .field("visible_skills", &self.visible_skills)
            .field("skills_enabled", &self.skills_enabled)
            .field("permission_policy", &self.permission_policy)
            .field("has_permission_handler", &self.permission_handler.is_some())
            .finish()
    }
}

struct AgentLoopState<'a, P>
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
    fn from_run(run: AgentRunBuilder<'a, P>) -> Self {
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
    async fn next(mut self) -> Option<(Result<AgentEvent>, Self)> {
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::{StreamExt, stream};
    use serde_json::json;

    use crate::{
        AgentsPromptConfig, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
        CompletionRequest, FilesystemPromptConfig, LlmError, ModelApi, ModelRef, ProviderId,
        SkillsConfig, ThinkingBlock, ThinkingBlockKind, ThinkingDelta, ToolCall, ToolDefinition,
        tools::filesystem::FilesystemConfig,
    };

    use super::{Agent, AgentEvent, AgentTool};

    #[test]
    fn filesystem_tools_are_opt_in() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);

        assert!(Agent::builder(model.clone()).build().tools().is_empty());
        assert!(
            Agent::builder(model)
                .with_filesystem_tools(FilesystemConfig::new("."))
                .build()
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "apply_patch")
        );
    }

    #[tokio::test]
    async fn agents_system_prompt_is_sent_to_provider() {
        let temp = TempRoot::new("waltr-agent-static-agents-test");
        temp.write("AGENTS.md", "Prefer concise answers.\n");
        let provider = AgentsSystemPromptProvider {
            expected_path: temp.path().join("AGENTS.md"),
        };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "hello").stream();
        let mut text = String::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "ok");
    }

    struct AgentsSystemPromptProvider {
        expected_path: PathBuf,
    }

    impl crate::LlmStream for AgentsSystemPromptProvider {
        fn name(&self) -> &'static str {
            "agents_system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains(&format!(
                    "Instructions from: {}",
                    self.expected_path.display()
                )));
                assert!(system_prompt.contains("Prefer concise answers."));

                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("ok".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn filesystem_read_agents_context_is_opt_in_and_loaded_once() {
        let temp = TempRoot::new("waltr-agent-read-agents-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = RepeatedReadProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
            .build();

        let mut events = agent.prompt(&provider, "read twice").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("<system-reminder>"));
        assert!(tool_results[0].contains("Nested instructions."));
        assert!(tool_results[0].contains("Root instructions."));
        assert!(!tool_results[1].contains("<system-reminder>"));
    }

    #[tokio::test]
    async fn filesystem_read_agents_context_is_disabled_by_default() {
        let temp = TempRoot::new("waltr-agent-read-agents-disabled-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_filesystem_tools(FilesystemConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(!tool_result.contains("<system-reminder>"));
        assert!(!tool_result.contains("Root instructions."));
        assert!(!tool_result.contains("Nested instructions."));
    }

    #[tokio::test]
    async fn filesystem_read_agents_context_skips_static_root_instruction() {
        let temp = TempRoot::new("waltr-agent-read-static-agents-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(tool_result.contains("Nested instructions."));
        assert!(!tool_result.contains("Root instructions."));
    }

    #[tokio::test]
    async fn skills_system_prompt_lists_root_skills_and_exposes_loader() {
        let temp = TempRoot::new("waltr-agent-skills-system-test");
        temp.write_skill(
            "example",
            "Use for example tasks.",
            "# Example\nFollow example steps.\n",
        );
        let provider = SkillsSystemPromptProvider {
            expected_root: temp.path().canonicalize().expect("canonical temp root"),
        };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "hello").stream();
        while events.next().await.is_some() {}
    }

    #[tokio::test]
    async fn load_skill_loads_content_and_lists_resources_once() {
        let temp = TempRoot::new("waltr-agent-load-skill-test");
        temp.write_skill(
            "example",
            "Use for example tasks.",
            "# Example\nFollow example steps.\n",
        );
        temp.write(".agents/skills/example/references/guide.md", "details\n");
        let provider = LoadSkillProvider::new(json!({ "name": "example" }));
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "load skill").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("<skill_content name=\"example\">"));
        assert!(tool_results[0].contains("Follow example steps."));
        assert!(tool_results[0].contains("- references/guide.md"));
        assert!(tool_results[1].contains("already loaded"));
    }

    #[tokio::test]
    async fn duplicate_skill_names_require_root() {
        let temp = TempRoot::new("waltr-agent-duplicate-skills-test");
        temp.write_skill("example", "Use root example.", "# Root Example\n");
        temp.write(
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
        );
        temp.write("src/main.rs", "fn main() {}\n");
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let provider = DuplicateSkillProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "read and load").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("New skills are available"));
        assert!(tool_results[0].contains("Use app example."));
        assert!(tool_results[1].contains("Tool error:"));
        assert!(tool_results[1].contains("ambiguous"));
    }

    #[tokio::test]
    async fn duplicate_skill_names_load_with_root() {
        let temp = TempRoot::new("waltr-agent-rooted-skill-test");
        temp.write_skill("example", "Use root example.", "# Root Example\n");
        temp.write(
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
        );
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let app_root = temp
            .path()
            .join("packages/app")
            .canonicalize()
            .expect("canonical app root");
        let provider = RootedSkillProvider { root: app_root };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "read and load").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[1].contains("<skill_content name=\"example\">"));
        assert!(tool_results[1].contains("# App Example"));
        assert!(!tool_results[1].contains("# Root Example"));
    }

    #[tokio::test]
    async fn glob_directory_walk_reveals_skills_once() {
        let temp = TempRoot::new("waltr-agent-glob-skills-test");
        temp.write(
            "packages/app/.agents/skills/app-skill/SKILL.md",
            "---\nname: app-skill\ndescription: Use app skill.\n---\n# App Skill\n",
        );
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let provider = RepeatedGlobProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "glob twice").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("New skills are available"));
        assert!(tool_results[0].contains("app-skill: Use app skill."));
        assert!(!tool_results[1].contains("New skills are available"));
    }

    #[tokio::test]
    async fn dynamic_skills_discovery_is_opt_in() {
        let temp = TempRoot::new("waltr-agent-skills-disabled-test");
        temp.write(
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
        );
        temp.write("src/main.rs", "fn main() {}\n");
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(!tool_result.contains("New skills are available"));
        assert!(!tool_result.contains("Use app example."));
    }

    struct SkillsSystemPromptProvider {
        expected_root: PathBuf,
    }

    impl crate::LlmStream for SkillsSystemPromptProvider {
        fn name(&self) -> &'static str {
            "skills_system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains("Available skills:"));
                assert!(system_prompt.contains("example: Use for example tasks."));
                assert!(system_prompt.contains(&format!("Root: {}", self.expected_root.display())));
                assert!(request.tools.iter().any(|tool| tool.name == "load_skill"));

                Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream)
            })
        }
    }

    struct LoadSkillProvider {
        requests: Mutex<usize>,
        arguments: serde_json::Value,
    }

    impl LoadSkillProvider {
        fn new(arguments: serde_json::Value) -> Self {
            Self {
                requests: Mutex::new(0),
                arguments,
            }
        }
    }

    impl crate::LlmStream for LoadSkillProvider {
        fn name(&self) -> &'static str {
            "load_skill_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("lock requests");
            let request_index = *requests;
            *requests += 1;
            drop(requests);
            let arguments = self.arguments.clone();

            Box::pin(async move {
                let events = if request_index < 2 {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("load_{request_index}"),
                        "load_skill",
                        arguments,
                    )))]
                } else {
                    vec![Ok(CompletionEvent::Finished)]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct DuplicateSkillProvider;

    impl crate::LlmStream for DuplicateSkillProvider {
        fn name(&self) -> &'static str {
            "duplicate_skill_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = match request.tool_results.len() {
                    0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "read_once",
                        "read",
                        json!({ "filePath": "packages/app/src/main.rs" }),
                    )))],
                    1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "load_once",
                        "load_skill",
                        json!({ "name": "example" }),
                    )))],
                    _ => vec![Ok(CompletionEvent::Finished)],
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct RootedSkillProvider {
        root: PathBuf,
    }

    impl crate::LlmStream for RootedSkillProvider {
        fn name(&self) -> &'static str {
            "rooted_skill_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let root = self.root.display().to_string();
            Box::pin(async move {
                let events = match request.tool_results.len() {
                    0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "read_once",
                        "read",
                        json!({ "filePath": "packages/app/src/main.rs" }),
                    )))],
                    1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "load_once",
                        "load_skill",
                        json!({ "name": "example", "root": root }),
                    )))],
                    _ => vec![Ok(CompletionEvent::Finished)],
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[derive(Default)]
    struct RepeatedGlobProvider {
        requests: Mutex<usize>,
    }

    impl crate::LlmStream for RepeatedGlobProvider {
        fn name(&self) -> &'static str {
            "repeated_glob_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("lock requests");
            let request_index = *requests;
            *requests += 1;
            drop(requests);

            Box::pin(async move {
                let events = if request_index < 2 {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("glob_{request_index}"),
                        "glob",
                        json!({ "pattern": "**/*.rs" }),
                    )))]
                } else {
                    vec![Ok(CompletionEvent::Finished)]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[derive(Default)]
    struct RepeatedReadProvider {
        requests: Mutex<usize>,
    }

    impl crate::LlmStream for RepeatedReadProvider {
        fn name(&self) -> &'static str {
            "repeated_read_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("lock requests");
            let request_index = *requests;
            *requests += 1;
            drop(requests);

            Box::pin(async move {
                let events = if request_index < 2 {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("read_{request_index}"),
                        "read",
                        json!({ "filePath": "src/main.rs" }),
                    )))]
                } else {
                    vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct SingleReadProvider;

    impl crate::LlmStream for SingleReadProvider {
        fn name(&self) -> &'static str {
            "single_read_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let is_continuation = !request.tool_results.is_empty();
            Box::pin(async move {
                let events = if is_continuation {
                    vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                } else {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "read_once",
                        "read",
                        json!({ "filePath": "src/main.rs" }),
                    )))]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn agent_system_prompt_fragments_are_sent_in_order() {
        let provider = SystemPromptProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_base_system_prompt()
            .with_system_date("2026-06-02")
            .with_filesystem_system_prompt(FilesystemPromptConfig::new("/tmp/waltr-root"))
            .with_system_prompt("custom fragment")
            .build();

        let mut events = agent
            .prompt(&provider, "hello")
            .with_system_prompt_fragment("run fragment")
            .stream();
        let mut text = String::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "ok");
    }

    struct SystemPromptProvider;

    impl crate::LlmStream for SystemPromptProvider {
        fn name(&self) -> &'static str {
            "system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains(crate::BASE_SYSTEM_PROMPT));
                assert!(system_prompt.contains("Current date: 2026-06-02."));
                assert!(system_prompt.contains("Filesystem tools are available"));
                assert!(system_prompt.contains("custom fragment"));
                assert!(system_prompt.ends_with("run fragment"));
                assert!(
                    system_prompt.find(crate::BASE_SYSTEM_PROMPT)
                        < system_prompt.find("Current date: 2026-06-02.")
                );
                assert!(
                    system_prompt.find("Current date: 2026-06-02.")
                        < system_prompt.find("Filesystem tools are available")
                );
                assert!(
                    system_prompt.find("Filesystem tools are available")
                        < system_prompt.find("custom fragment")
                );
                assert!(system_prompt.find("custom fragment") < system_prompt.find("run fragment"));

                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("ok".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn tool_errors_are_reported_to_agent_for_recovery() {
        let provider = RecoveringProvider;
        let tool = AgentTool::new(
            ToolDefinition::new(
                "fail_once",
                "Always fails.",
                json!({ "type": "object", "additionalProperties": false }),
            ),
            |_call| {
                Err(LlmError::Protocol {
                    provider: "test_tool",
                    message: "validation failed".to_owned(),
                })
            },
        );
        let agent = Agent::builder(ModelRef::new(
            ProviderId::OpenAi,
            "test-model",
            ModelApi::OpenAiResponses,
        ))
        .with_tool(tool)
        .build();

        let mut events = agent.prompt(&provider, "recover").stream();
        let mut saw_error_result = false;
        let mut final_text = String::new();

        while let Some(event) = events.next().await {
            match event.expect("agent event") {
                AgentEvent::ToolResult { result, .. } => {
                    saw_error_result = result.content.contains("Tool error:")
                        && result.content.contains("validation failed");
                }
                AgentEvent::TextDelta { delta, .. } => final_text.push_str(&delta),
                AgentEvent::ThinkingDelta { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::TurnFinished { .. }
                | AgentEvent::Finished { .. } => {}
            }
        }

        assert!(saw_error_result);
        assert_eq!(final_text, "recovered");
    }

    #[tokio::test]
    async fn agent_surfaces_and_stores_thinking_without_final_text_pollution() {
        let provider = ThinkingProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model).build();

        let mut events = agent.prompt(&provider, "think").stream();
        let mut saw_thinking_delta = false;
        let mut final_state = None;

        while let Some(event) = events.next().await {
            match event.expect("agent event") {
                AgentEvent::ThinkingDelta { delta, .. } => {
                    saw_thinking_delta = delta == "hidden thought";
                }
                AgentEvent::Finished { state } => final_state = Some(state),
                AgentEvent::TextDelta { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::ToolResult { .. }
                | AgentEvent::TurnFinished { .. } => {}
            }
        }

        let state = final_state.expect("final state");
        assert!(saw_thinking_delta);
        assert_eq!(state.last_assistant_text(), "final answer");
        assert!(matches!(
            state.events[0],
            super::AgentTranscriptEvent::Thinking { ref text, .. } if text == "hidden thought"
        ));
        assert_eq!(state.thinking_blocks.len(), 1);
    }

    struct ThinkingProvider;

    impl crate::LlmStream for ThinkingProvider {
        fn name(&self) -> &'static str {
            "thinking_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::ThinkingDelta(ThinkingDelta {
                        text: "hidden thought".to_owned(),
                    })),
                    Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: "hidden thought".to_owned(),
                        payload: json!({ "type": "reasoning", "id": "rs_1" }),
                    })),
                    Ok(CompletionEvent::Delta("final answer".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn agent_replays_thinking_blocks_on_tool_continuation() {
        let provider = ReplayThinkingProvider::default();
        let tool = AgentTool::new(
            ToolDefinition::new(
                "lookup",
                "Look up test data.",
                json!({ "type": "object", "additionalProperties": false }),
            ),
            |_call| Ok("tool output".to_owned()),
        );
        let agent = Agent::builder(ModelRef::new(
            ProviderId::OpenAi,
            "test-model",
            ModelApi::OpenAiResponses,
        ))
        .with_tool(tool)
        .build();

        let mut events = agent.prompt(&provider, "use tool").stream();
        while events.next().await.is_some() {}

        assert!(provider.saw_replayed_thinking());
    }

    #[derive(Default)]
    struct ReplayThinkingProvider {
        calls: Mutex<usize>,
        saw_replayed_thinking: Mutex<bool>,
    }

    impl ReplayThinkingProvider {
        fn saw_replayed_thinking(&self) -> bool {
            *self.saw_replayed_thinking.lock().expect("lock")
        }
    }

    impl crate::LlmStream for ReplayThinkingProvider {
        fn name(&self) -> &'static str {
            "replay_thinking_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let mut calls = self.calls.lock().expect("lock");
                *calls += 1;
                if *calls == 1 {
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                            kind: ThinkingBlockKind::OpenAiResponses,
                            text: "reasoning".to_owned(),
                            payload: json!({ "type": "reasoning", "id": "rs_1" }),
                        })),
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "call_1",
                            "lookup",
                            json!({}),
                        ))),
                    ])) as BoxCompletionStream)
                } else {
                    *self.saw_replayed_thinking.lock().expect("lock") =
                        request.thinking_blocks.len() == 1;
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                }
            })
        }
    }

    struct RecoveringProvider;

    impl crate::LlmStream for RecoveringProvider {
        fn name(&self) -> &'static str {
            "recovering_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = if request.tool_results.is_empty() {
                    vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "call_1",
                            "fail_once",
                            json!({}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ]
                } else {
                    assert!(
                        request.tool_results[0]
                            .content
                            .contains("validation failed")
                    );
                    vec![
                        Ok(CompletionEvent::Delta("recovered".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                };

                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, content).expect("write temp file");
        }

        fn write_skill(&self, name: &str, description: &str, body: &str) {
            self.write(
                &format!(".agents/skills/{name}/SKILL.md"),
                &format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
            );
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}
