//! Agent construction and configuration.

use std::{fmt, path::PathBuf, sync::Arc};

use crate::{
    ModelRef, PermissionPolicy, Result, SharedPermissionHandler, SystemPromptBuilder,
    mcp::McpConfig,
    prompt::{AgentsPromptConfig, FilesystemPromptConfig},
    tools::{
        filesystem::FilesystemConfig,
        question::QuestionConfig,
        skills::{SkillDefinition, SkillsConfig, render_skills_catalog},
    },
};

#[cfg(feature = "shell")]
use crate::{prompt::ShellPromptConfig, tools::shell::ShellConfig};

use super::{Agent, AgentTool};

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
