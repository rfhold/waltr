//! Per-run agent configuration.

use std::{fmt, path::PathBuf, sync::Arc};

use futures_util::stream;

use crate::{LlmStream, PermissionPolicy, SharedPermissionHandler};

use super::{Agent, AgentTool, BoxAgentEventStream, r#loop::AgentLoopState};
use crate::SkillDefinition;

/// Builder for an agent loop request.
pub struct AgentRunBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    pub(super) provider: &'a P,
    pub(super) prompt: String,
    pub(super) system_prompt: Option<String>,
    pub(super) tools: Vec<AgentTool>,
    pub(super) agents_system_paths: Vec<PathBuf>,
    pub(super) visible_skills: Vec<SkillDefinition>,
    pub(super) skills_enabled: bool,
    pub(super) permission_policy: PermissionPolicy,
    pub(super) permission_handler: Option<SharedPermissionHandler>,
}

impl<'a, P> AgentRunBuilder<'a, P>
where
    P: LlmStream + ?Sized,
{
    pub(super) fn new(agent: &'a Agent, provider: &'a P, prompt: String) -> Self {
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
