//! Agent construction and configuration.

use std::{fmt, path::PathBuf, sync::Arc};

use crate::{
    LlmStream, ModelCost, ModelRef, PermissionPolicy, SystemPromptBuilder,
    middleware::{
        AgentMiddleware, agents::AgentsInstructionsMiddleware, chain::MiddlewareChain,
        claims::FilesystemClaimsMiddleware, file_templates::FileTemplateMiddleware,
        prompt::SystemPromptMiddleware, skills::SkillsMiddleware,
        tools::PermissionFilteredToolRegistryMiddleware,
    },
    prompt::{AgentsPromptConfig, FilesystemPromptConfig},
    tools::{
        filesystem::FilesystemConfig, question::QuestionConfig, skills::SkillsConfig,
        task::TaskConfig,
    },
};

#[cfg(feature = "shell")]
use crate::{
    middleware::claims::ShellClaimsMiddleware, prompt::ShellPromptConfig, tools::shell::ShellConfig,
};

use super::{AgentRuntime, AgentTool, ProviderRetryConfig, SessionStore};

/// Builder for an agent runtime configuration.
#[derive(Clone)]
pub struct AgentRuntimeBuilder {
    provider: Arc<dyn LlmStream>,
    model: ModelRef,
    model_cost: Option<ModelCost>,
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    system_prompt: SystemPromptBuilder,
    agents_prompt_configs: Vec<AgentsPromptConfig>,
    agents_read_roots: Vec<PathBuf>,
    skills_enabled: bool,
    skills: Vec<crate::SkillDefinition>,
    skills_reveal_roots: Vec<PathBuf>,
    permission_policy: PermissionPolicy,
    explicit_permission_policy: bool,
    retry_config: ProviderRetryConfig,
    template_base_dir: Option<PathBuf>,
}

impl AgentRuntimeBuilder {
    /// Create a builder for a model.
    #[must_use]
    pub fn new<P>(model: ModelRef, provider: Arc<P>) -> Self
    where
        P: LlmStream + 'static,
    {
        Self {
            provider,
            model,
            model_cost: None,
            tools: Vec::new(),
            middleware_chain: MiddlewareChain::new(),
            system_prompt: SystemPromptBuilder::new(),
            agents_prompt_configs: Vec::new(),
            agents_read_roots: Vec::new(),
            skills_enabled: false,
            skills: Vec::new(),
            skills_reveal_roots: Vec::new(),
            permission_policy: PermissionPolicy::default(),
            explicit_permission_policy: false,
            retry_config: ProviderRetryConfig::default(),
            template_base_dir: None,
        }
    }

    /// Set model pricing used to journal per-turn cost.
    #[must_use]
    pub fn with_model_cost(mut self, cost: ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }

    /// Opt into filesystem tools as generic host tools.
    #[must_use]
    pub fn with_filesystem_tools(mut self, config: FilesystemConfig) -> Self {
        if !self.explicit_permission_policy {
            self.permission_policy = self
                .permission_policy
                .merged(PermissionPolicy::filesystem_default(config.root.clone()));
        }
        if config.agents_read_context {
            self.agents_read_roots.push(config.root.clone());
        }
        if config.skills_context {
            self.skills_reveal_roots.push(config.root.clone());
        }
        self.middleware_chain
            .push(config.root_hints_middleware(&self.model));
        self.middleware_chain
            .push(FilesystemClaimsMiddleware::new(config.clone(), &self.model));
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

    /// Opt into durable `AGENTS.md` instruction context.
    #[must_use]
    pub fn with_agents_system_prompt(mut self, config: AgentsPromptConfig) -> Self {
        self.agents_prompt_configs.push(config);
        self
    }

    /// Opt into Agent Skills catalog and the dedicated skill loading tool.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_skills(mut self, config: SkillsConfig) -> Self {
        let skills = config.discover();
        self.skills_enabled = true;
        self.skills.extend(skills);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_skill_definitions(mut self, skills: Vec<crate::SkillDefinition>) -> Self {
        self.skills_enabled = true;
        self.skills.extend(skills);
        self
    }

    /// Opt into the structured user question tool.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_question_tool(mut self, config: QuestionConfig) -> Self {
        self.tools.push(config.into_agent_tool());
        self.middleware_chain
            .push(crate::middleware::question_suspension::QuestionSuspensionMiddleware);
        self
    }

    /// Opt into the subagent task tool.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn with_task_tool<S>(mut self, config: TaskConfig<S>) -> Self
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        self.middleware_chain.push(config.visibility_middleware());
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
        self.middleware_chain.push(config.root_hints_middleware());
        self.middleware_chain
            .push(ShellClaimsMiddleware::new(config.clone()));
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

    /// Add middleware to the agent's default middleware chain.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn with_middleware<M>(mut self, middleware: M) -> Self
    where
        M: AgentMiddleware + 'static,
    {
        self.middleware_chain.push(middleware);
        self
    }

    /// Replace the agent's default middleware chain.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn with_middleware_chain(mut self, middleware_chain: MiddlewareChain) -> Self {
        self.middleware_chain = middleware_chain;
        self
    }

    /// Set the permission policy for this agent.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self.explicit_permission_policy = true;
        self
    }

    /// Set provider retry behavior for this agent runtime.
    #[must_use]
    pub fn with_provider_retry_config(mut self, config: ProviderRetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Set the base directory used for prompt template file references.
    #[must_use]
    pub fn with_template_base_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.template_base_dir = Some(path.into());
        self
    }

    /// Build the agent runtime configuration.
    ///
    /// Middleware assembly order is deterministic and affects provider request
    /// hashes: the stateful system prompt middleware runs first, then feature
    /// and custom middleware in registration order, then `AGENTS.md`
    /// instructions, then skills.
    #[must_use]
    pub fn build(self) -> AgentRuntime {
        let mut middleware_chain = MiddlewareChain::new();
        middleware_chain.push(SystemPromptMiddleware::new(self.system_prompt.build()));
        middleware_chain.push(FileTemplateMiddleware);
        middleware_chain.push(PermissionFilteredToolRegistryMiddleware::new(
            self.tools.clone(),
            self.permission_policy.clone(),
            true,
        ));
        middleware_chain.extend(self.middleware_chain);
        if !self.agents_prompt_configs.is_empty() || !self.agents_read_roots.is_empty() {
            middleware_chain.push(AgentsInstructionsMiddleware::new(
                self.agents_prompt_configs,
                self.agents_read_roots,
            ));
        }
        if self.skills_enabled {
            middleware_chain.push(SkillsMiddleware::new(self.skills, self.skills_reveal_roots));
        }
        AgentRuntime {
            provider: self.provider,
            model: self.model,
            model_cost: self.model_cost,
            tools: self.tools,
            middleware_chain,
            permission_policy: self.permission_policy,
            retry_config: self.retry_config,
            template_base_dir: self.template_base_dir,
        }
    }
}

impl fmt::Debug for AgentRuntimeBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntimeBuilder")
            .field("provider", &self.provider.name())
            .field("model", &self.model)
            .field("model_cost", &self.model_cost)
            .field("tools", &self.tools)
            .field("middleware_chain_len", &self.middleware_chain.len())
            .field("system_prompt", &self.system_prompt)
            .field("agents_prompt_configs", &self.agents_prompt_configs)
            .field("agents_read_roots", &self.agents_read_roots)
            .field("skills_enabled", &self.skills_enabled)
            .field("skills", &self.skills)
            .field("skills_reveal_roots", &self.skills_reveal_roots)
            .field("permission_policy", &self.permission_policy)
            .field(
                "explicit_permission_policy",
                &self.explicit_permission_policy,
            )
            .field("retry_config", &self.retry_config)
            .field("template_base_dir", &self.template_base_dir)
            .finish()
    }
}
