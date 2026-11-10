//! Per-run agent configuration.

use std::fmt;
use std::sync::Arc;

use futures_util::stream;
use tokio_util::sync::CancellationToken;

use super::session::{
    AgentRunLimits, EphemeralSessionRun, MemorySessionStore, ResumeInput, SessionId,
    SessionRunContext, SessionStore,
};
use super::{AgentRuntime, AgentTool, BoxAgentTimelineStream, r#loop::AgentLoopState};
use crate::middleware::{chain::MiddlewareChain, prompt::SystemPromptOverride};

/// Builder for an agent loop request.
pub struct AgentRunBuilder<S = MemorySessionStore>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    runtime: AgentRuntime,
    prompt: String,
    input: ResumeInput,
    session_id: SessionId,
    session_store: S,
    session_run_context: Option<SessionRunContext>,
    system_prompt: SystemPromptOverride,
    extra_tools: Vec<AgentTool>,
    cancellation_token: Option<CancellationToken>,
    event_sink: Option<Arc<dyn crate::SessionEventSink>>,
    completion_validation: Option<super::completion::CompletionValidation>,
}

impl AgentRunBuilder {
    pub(crate) fn resume_with_transient_input<S>(
        runtime: &AgentRuntime,
        session_id: SessionId,
        store: &S,
        input: crate::TransientInput,
    ) -> crate::Result<AgentRunBuilder<S>>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let provider = crate::transient_input::bind_provider(runtime.provider.clone(), input)?;
        let mut owned = runtime.clone();
        owned.provider = provider;
        Ok(AgentRunBuilder::resume(
            &owned,
            session_id,
            store.clone(),
            ResumeInput::Continue,
        ))
    }

    pub(super) fn new(runtime: &AgentRuntime, prompt: String) -> Self {
        Self {
            runtime: runtime.clone(),
            prompt: prompt.clone(),
            input: ResumeInput::Prompt(prompt),
            session_id: SessionId::new("ephemeral"),
            session_store: MemorySessionStore::new(),
            session_run_context: None,
            system_prompt: SystemPromptOverride::default(),
            extra_tools: Vec::new(),
            cancellation_token: None,
            event_sink: None,
            completion_validation: runtime.completion_validation.clone(),
        }
    }

    pub(super) fn resume<S>(
        runtime: &AgentRuntime,
        session_id: SessionId,
        session_store: S,
        input: ResumeInput,
    ) -> AgentRunBuilder<S>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let prompt = match &input {
            ResumeInput::Prompt(prompt) => prompt.clone(),
            ResumeInput::Prompts(prompts) => prompts.last().cloned().unwrap_or_default(),
            ResumeInput::Continue | ResumeInput::ToolResults(_) => String::new(),
        };
        AgentRunBuilder {
            runtime: runtime.clone(),
            prompt,
            input,
            session_id,
            session_store,
            session_run_context: None,
            system_prompt: SystemPromptOverride::default(),
            extra_tools: Vec::new(),
            cancellation_token: None,
            event_sink: None,
            completion_validation: runtime.completion_validation.clone(),
        }
    }
}

impl<S> AgentRunBuilder<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Bind accounted invocation-local input to an existing durable helper task.
    /// Empty selections should omit this call to retain ordinary text behavior.
    ///
    /// # Errors
    /// Rejects unsupported providers or an already-bound runtime.
    pub fn with_transient_input(mut self, input: crate::TransientInput) -> crate::Result<Self> {
        self.runtime.provider =
            crate::transient_input::bind_provider(self.runtime.provider, input)?;
        Ok(self)
    }

    /// Validate completion proposals before committing output or finishing the session.
    #[must_use]
    pub fn with_completion_validation(
        mut self,
        validation: super::completion::CompletionValidation,
    ) -> Self {
        self.runtime.max_provider_turns = Some(
            self.runtime
                .max_provider_turns
                .map_or(validation.max_turns, |limit| {
                    limit.min(validation.max_turns)
                }),
        );
        self.completion_validation = Some(validation);
        self
    }

    /// Add one tool to this run.
    #[must_use]
    pub fn with_tool(mut self, tool: AgentTool) -> Self {
        self.extra_tools.push(tool.clone());
        self.runtime.tools.push(tool);
        self
    }

    /// Replace the agent-configured system prompt for this run.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt.replace(prompt);
        self
    }

    /// Append a system prompt fragment for this run.
    #[must_use]
    pub fn with_system_prompt_fragment(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt.push_fragment(prompt);
        self
    }

    /// Stop this run when the supplied cancellation token is canceled.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    /// Apply per-run execution limits without loosening runtime-configured limits.
    #[must_use]
    pub fn with_run_limits(mut self, limits: AgentRunLimits) -> Self {
        if let Some(limit) = limits.max_provider_turns {
            let limit = limit.get();
            self.runtime.max_provider_turns = Some(
                self.runtime
                    .max_provider_turns
                    .map_or(limit, |configured| configured.min(limit)),
            );
        }
        self
    }

    /// Deliver post-durability semantic session events to `sink`.
    #[must_use]
    pub fn with_session_event_sink(mut self, sink: Arc<dyn crate::SessionEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    pub(crate) fn with_session_run_context(mut self, context: SessionRunContext) -> Self {
        self.session_run_context = Some(context);
        self
    }

    /// Persist this run in the supplied session store and session id.
    #[must_use]
    pub fn with_session_store<T>(self, store: T, id: SessionId) -> AgentRunBuilder<T>
    where
        T: SessionStore + Clone + Send + Sync + 'static,
    {
        let Self {
            runtime,
            prompt,
            input,
            session_id: _,
            session_store: _,
            session_run_context,
            system_prompt,
            extra_tools,
            cancellation_token,
            event_sink,
            completion_validation,
        } = self;
        AgentRunBuilder {
            runtime,
            prompt,
            input,
            session_id: id,
            session_store: store,
            session_run_context,
            system_prompt,
            extra_tools,
            cancellation_token,
            event_sink,
            completion_validation,
        }
    }

    /// Stream the full ordered agent timeline.
    #[must_use]
    pub fn stream(self) -> BoxAgentTimelineStream<'static> {
        let run = self.into_ephemeral_session_run();
        Box::pin(stream::unfold(
            AgentLoopState::from_ephemeral_session(run),
            AgentLoopState::next,
        ))
    }

    fn into_ephemeral_session_run(self) -> EphemeralSessionRun<S> {
        let Self {
            runtime,
            prompt: _,
            input,
            session_id,
            session_store,
            session_run_context,
            system_prompt,
            extra_tools,
            cancellation_token,
            event_sink,
            completion_validation,
        } = self;
        let AgentRuntime {
            completion_validation: _,
            provider,
            model,
            model_cost,
            tools,
            middleware_chain: default_chain,
            retry_config,
            max_provider_turns,
            host_context,
        } = runtime;

        let middleware_chain = if extra_tools.is_empty() {
            default_chain
        } else {
            let mut middleware_chain = MiddlewareChain::new();
            middleware_chain.push(super::host::HostToolRegistry(extra_tools));
            middleware_chain.extend(default_chain);
            middleware_chain
        };

        EphemeralSessionRun {
            provider,
            model,
            model_cost,
            input,
            system_prompt_override: (!system_prompt.is_empty()).then_some(system_prompt),
            tools,
            middleware_chain,
            retry_config,
            max_provider_turns,
            host_context,
            session_id,
            session_store,
            session_run_context,
            cancellation_token: cancellation_token.unwrap_or_default(),
            event_sink,
            completion_validation,
        }
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl<S> fmt::Debug for AgentRunBuilder<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRunBuilder")
            .field("runtime", &self.runtime)
            .field("prompt", &self.prompt)
            .field("system_prompt", &self.system_prompt)
            .field("session_run_context", &self.session_run_context.is_some())
            .field("cancellation_token", &self.cancellation_token.is_some())
            .finish()
    }
}
