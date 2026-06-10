//! Per-run agent configuration.

use std::collections::HashSet;
use std::{fmt, path::PathBuf};

use futures_util::stream;

use crate::{LlmStream, ModelRef, PermissionPolicy};

use super::session::{
    EphemeralSessionRun, MemorySessionStore, ResumeInput, SessionId, SessionStore,
};
use super::{Agent, AgentTool, AgentToolContext, BoxAgentEventStream, r#loop::AgentLoopState};
use crate::{
    SkillDefinition,
    middleware::{
        chain::MiddlewareChain, prompt::StaticSystemPromptMiddleware,
        tools::PermissionFilteredToolRegistryMiddleware,
    },
};

/// Builder for an agent loop request.
pub struct AgentRunBuilder<'a, P, S = MemorySessionStore>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(super) provider: &'a P,
    pub(super) model: ModelRef,
    pub(super) prompt: String,
    pub(super) system_prompt: Option<String>,
    pub(super) tools: Vec<AgentTool>,
    pub(super) middleware_chain: MiddlewareChain,
    pub(super) agents_system_paths: Vec<PathBuf>,
    pub(super) visible_skills: Vec<SkillDefinition>,
    pub(super) skills_enabled: bool,
    pub(super) permission_policy: PermissionPolicy,
    pub(super) input: ResumeInput,
    pub(super) session_id: SessionId,
    pub(super) session_store: S,
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
            system_prompt: agent.system_prompt.clone(),
            tools: agent.tools.clone(),
            middleware_chain: agent.middleware_chain.clone(),
            agents_system_paths: agent.agents_system_paths.clone(),
            visible_skills: agent.visible_skills.clone(),
            skills_enabled: agent.skills_enabled,
            permission_policy: agent.permission_policy.clone(),
            input: ResumeInput::Prompt(prompt),
            session_id: SessionId::new("ephemeral"),
            session_store: MemorySessionStore::new(),
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
            ResumeInput::ToolResults(_)
            | ResumeInput::ExternalResponse { .. }
            | ResumeInput::Interrupt => String::new(),
        };
        AgentRunBuilder {
            provider,
            model: agent.model.clone(),
            prompt,
            system_prompt: agent.system_prompt.clone(),
            tools: agent.tools.clone(),
            middleware_chain: agent.middleware_chain.clone(),
            agents_system_paths: agent.agents_system_paths.clone(),
            visible_skills: agent.visible_skills.clone(),
            skills_enabled: agent.skills_enabled,
            permission_policy: agent.permission_policy.clone(),
            input,
            session_id,
            session_store,
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

    /// Override the permission policy for this run.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self
    }

    /// Persist this run in the supplied in-memory session store and session id.
    #[must_use]
    pub fn with_session_store<T>(self, store: T, id: SessionId) -> AgentRunBuilder<'a, P, T>
    where
        T: SessionStore + Clone + Send + Sync + 'static,
    {
        let Self {
            provider,
            model,
            prompt,
            system_prompt,
            tools,
            middleware_chain,
            agents_system_paths,
            visible_skills,
            skills_enabled,
            permission_policy,
            input,
            session_id: _,
            session_store: _,
        } = self;
        AgentRunBuilder {
            provider,
            model,
            prompt,
            system_prompt,
            tools,
            middleware_chain,
            agents_system_paths,
            visible_skills,
            skills_enabled,
            permission_policy,
            input,
            session_id: id,
            session_store: store,
        }
    }

    /// Stream the full ordered agent timeline.
    #[must_use]
    pub fn stream(self) -> BoxAgentEventStream<'a> {
        let run = self.into_ephemeral_session_run();
        Box::pin(stream::unfold(
            AgentLoopState::from_ephemeral_session(run),
            AgentLoopState::next,
        ))
    }

    fn into_ephemeral_session_run(self) -> EphemeralSessionRun<'a, P, S> {
        let tool_definitions = self
            .tools
            .iter()
            .map(|tool| tool.definition().clone())
            .collect();
        let mut middleware_chain = MiddlewareChain::new();
        middleware_chain.push(StaticSystemPromptMiddleware::new(
            self.system_prompt.clone(),
        ));
        middleware_chain.push(PermissionFilteredToolRegistryMiddleware::new(
            tool_definitions,
            self.permission_policy.clone(),
            true,
        ));
        middleware_chain.extend(self.middleware_chain);

        let skills_revealed_roots = self
            .visible_skills
            .iter()
            .map(|skill| skill.skills_dir.clone())
            .collect();
        EphemeralSessionRun::new(
            self.provider,
            self.model,
            self.input,
            self.system_prompt,
            self.tools,
            middleware_chain,
            AgentToolContext {
                agents_system_paths: self.agents_system_paths.into_iter().collect(),
                agents_loaded_paths: HashSet::new(),
                visible_skills: self.visible_skills,
                skills_enabled: self.skills_enabled,
                skills_revealed_roots,
                skills_loaded_paths: HashSet::new(),
            },
            self.permission_policy,
            self.session_id,
            self.session_store,
        )
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
            .field("system_prompt", &self.system_prompt)
            .field("tools", &self.tools)
            .field("middleware_chain_len", &self.middleware_chain.len())
            .field("agents_system_paths", &self.agents_system_paths)
            .field("visible_skills", &self.visible_skills)
            .field("skills_enabled", &self.skills_enabled)
            .field("permission_policy", &self.permission_policy)
            .finish()
    }
}
