//! Per-run agent configuration.

use std::fmt;

use futures_util::stream;
use tokio_util::sync::CancellationToken;

use crate::{LlmStream, ModelRef, PermissionPolicy};

use super::session::{
    EphemeralSessionRun, MemorySessionStore, ResumeInput, SessionId, SessionStore,
};
use super::{Agent, AgentTool, BoxAgentTimelineStream, r#loop::AgentLoopState};
use crate::middleware::{
    chain::MiddlewareChain, prompt::SystemPromptOverride,
    tools::PermissionFilteredToolRegistryMiddleware,
};

/// Static run configuration cloned from the agent.
#[derive(Clone)]
struct RunDefaults {
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    permission_policy: PermissionPolicy,
}

impl RunDefaults {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            tools: agent.tools.clone(),
            middleware_chain: agent.middleware_chain.clone(),
            permission_policy: agent.permission_policy.clone(),
        }
    }
}

/// Per-run values explicitly set by the caller.
#[derive(Default)]
struct RunOverrides {
    tools: Vec<AgentTool>,
    system_prompt: SystemPromptOverride,
    permission_policy: Option<PermissionPolicy>,
    cancellation_token: Option<CancellationToken>,
}

/// Builder for an agent loop request.
pub struct AgentRunBuilder<'a, P, S = MemorySessionStore>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    provider: &'a P,
    model: ModelRef,
    prompt: String,
    input: ResumeInput,
    session_id: SessionId,
    session_store: S,
    defaults: RunDefaults,
    overrides: RunOverrides,
}

impl<'a, P> AgentRunBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    pub(super) fn new(agent: &'a Agent, provider: &'a P, prompt: String) -> Self {
        Self {
            provider,
            model: agent.model.clone(),
            prompt: prompt.clone(),
            input: ResumeInput::Prompt(prompt),
            session_id: SessionId::new("ephemeral"),
            session_store: MemorySessionStore::new(),
            defaults: RunDefaults::from_agent(agent),
            overrides: RunOverrides::default(),
        }
    }

    pub(super) fn resume<S>(
        agent: &'a Agent,
        provider: &'a P,
        session_id: SessionId,
        session_store: S,
        input: ResumeInput,
    ) -> AgentRunBuilder<'a, P, S>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let prompt = match &input {
            ResumeInput::Prompt(prompt) => prompt.clone(),
            ResumeInput::ToolResults(_) | ResumeInput::ExternalResponse { .. } => String::new(),
        };
        AgentRunBuilder {
            provider,
            model: agent.model.clone(),
            prompt,
            input,
            session_id,
            session_store,
            defaults: RunDefaults::from_agent(agent),
            overrides: RunOverrides::default(),
        }
    }
}

impl<'a, P, S> AgentRunBuilder<'a, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Add one tool to this run.
    #[must_use]
    pub fn with_tool(mut self, tool: AgentTool) -> Self {
        self.overrides.tools.push(tool);
        self
    }

    /// Replace the agent-configured system prompt for this run.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.overrides.system_prompt.replace(prompt);
        self
    }

    /// Append a system prompt fragment for this run.
    #[must_use]
    pub fn with_system_prompt_fragment(mut self, prompt: impl Into<String>) -> Self {
        self.overrides.system_prompt.push_fragment(prompt);
        self
    }

    /// Override the permission policy for this run.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.overrides.permission_policy = Some(policy);
        self
    }

    /// Stop this run when the supplied cancellation token is canceled.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.overrides.cancellation_token = Some(token);
        self
    }

    /// Persist this run in the supplied session store and session id.
    #[must_use]
    pub fn with_session_store<T>(self, store: T, id: SessionId) -> AgentRunBuilder<'a, P, T>
    where
        T: SessionStore + Clone + Send + Sync + 'static,
    {
        let Self {
            provider,
            model,
            prompt,
            input,
            session_id: _,
            session_store: _,
            defaults,
            overrides,
        } = self;
        AgentRunBuilder {
            provider,
            model,
            prompt,
            input,
            session_id: id,
            session_store: store,
            defaults,
            overrides,
        }
    }

    /// Stream the full ordered agent timeline.
    #[must_use]
    pub fn stream(self) -> BoxAgentTimelineStream<'a> {
        let run = self.into_ephemeral_session_run();
        Box::pin(stream::unfold(
            AgentLoopState::from_ephemeral_session(run),
            AgentLoopState::next,
        ))
    }

    fn into_ephemeral_session_run(self) -> EphemeralSessionRun<'a, P, S> {
        let RunDefaults {
            mut tools,
            middleware_chain: default_chain,
            permission_policy,
        } = self.defaults;
        let RunOverrides {
            tools: extra_tools,
            system_prompt,
            permission_policy: policy_override,
            cancellation_token,
        } = self.overrides;
        tools.extend(extra_tools);
        let permission_policy = policy_override.unwrap_or(permission_policy);

        let tool_definitions = tools.iter().map(|tool| tool.definition().clone()).collect();
        let mut middleware_chain = MiddlewareChain::new();
        middleware_chain.push(PermissionFilteredToolRegistryMiddleware::new(
            tool_definitions,
            permission_policy.clone(),
            true,
        ));
        middleware_chain.extend(default_chain);

        EphemeralSessionRun {
            provider: self.provider,
            model: self.model,
            input: self.input,
            system_prompt_override: (!system_prompt.is_empty()).then_some(system_prompt),
            tools,
            middleware_chain,
            permission_policy,
            session_id: self.session_id,
            session_store: self.session_store,
            cancellation_token: cancellation_token.unwrap_or_default(),
        }
    }
}

impl<P, S> fmt::Debug for AgentRunBuilder<'_, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRunBuilder")
            .field("provider", &self.provider.name())
            .field("model", &self.model)
            .field("prompt", &self.prompt)
            .field("default_tools", &self.defaults.tools)
            .field(
                "default_middleware_chain_len",
                &self.defaults.middleware_chain.len(),
            )
            .field(
                "default_permission_policy",
                &self.defaults.permission_policy,
            )
            .field("override_tools", &self.overrides.tools)
            .field("override_system_prompt", &self.overrides.system_prompt)
            .field(
                "override_permission_policy",
                &self.overrides.permission_policy,
            )
            .finish()
    }
}
