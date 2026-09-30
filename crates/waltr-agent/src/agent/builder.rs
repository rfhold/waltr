//! Agent construction and configuration.

use std::{fmt, path::PathBuf, sync::Arc};

use crate::{
    LlmStream, ModelCost, ModelRef, PermissionPolicy, SystemPromptBuilder,
    middleware::{
        AgentMiddleware, agents::AgentsInstructionsMiddleware, chain::MiddlewareChain,
        file_templates::FileTemplateMiddleware, prompt::SystemPromptMiddleware,
        tools::PermissionFilteredToolRegistryMiddleware,
    },
    prompt::{AgentsPromptConfig, FilesystemPromptConfig},
    tools::{
        filesystem::FilesystemConfig, question::QuestionConfig, skills::SkillsConfig,
        task::TaskConfig,
    },
};

#[cfg(feature = "shell")]
use crate::{prompt::ShellPromptConfig, tools::shell::ShellConfig};

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
    system_prompt_identity: Option<String>,
    agents_prompt_configs: Vec<AgentsPromptConfig>,
    agents_read_roots: Vec<PathBuf>,
    skills_enabled: bool,
    skills_root: Option<PathBuf>,
    skills_reveal_roots: Vec<PathBuf>,
    permission_policy: PermissionPolicy,
    explicit_permission_policy: bool,
    retry_config: ProviderRetryConfig,
    template_base_dir: Option<PathBuf>,
    file_templates: bool,
    max_provider_turns: Option<u64>,
    sandbox: Option<crate::sandbox_gateway::SandboxHandle>,
}

impl AgentRuntimeBuilder {
    /// Create a builder for a model.
    #[must_use]
    pub fn new<P>(model: ModelRef, provider: Arc<P>) -> Self
    where
        P: LlmStream + 'static,
    {
        Self::new_dyn(model, provider)
    }

    pub(crate) fn new_dyn(model: ModelRef, provider: Arc<dyn LlmStream>) -> Self {
        Self {
            provider,
            model,
            model_cost: None,
            tools: Vec::new(),
            middleware_chain: MiddlewareChain::new(),
            system_prompt: SystemPromptBuilder::new(),
            system_prompt_identity: None,
            agents_prompt_configs: Vec::new(),
            agents_read_roots: Vec::new(),
            skills_enabled: false,
            skills_root: None,
            skills_reveal_roots: Vec::new(),
            permission_policy: PermissionPolicy::default(),
            explicit_permission_policy: false,
            retry_config: ProviderRetryConfig::default(),
            template_base_dir: None,
            file_templates: false,
            max_provider_turns: None,
            sandbox: None,
        }
    }

    pub(crate) const fn model(&self) -> &ModelRef {
        &self.model
    }

    /// Bind workspace-dependent tools and context to a connected sandbox.
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: crate::sandbox_gateway::SandboxHandle) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Set model pricing used to journal per-turn cost.
    #[must_use]
    pub fn with_model_cost(mut self, cost: ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }

    /// Opt into filesystem tools as generic host tools.
    #[must_use]
    pub fn with_filesystem_tools(mut self, mut config: FilesystemConfig) -> Self {
        if let Some(root) = self
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.canonical_root())
        {
            config.root = root;
        }
        self.file_templates = true;
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
        let sandbox = self
            .sandbox
            .clone()
            .expect("filesystem tools require a sandbox handle");
        self.tools
            .extend(config.into_agent_tools(&self.model, sandbox));
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

    /// Identify the definition-owned base prompt for durable replacement semantics.
    #[must_use]
    pub fn with_system_prompt_identity(mut self, identity: impl Into<String>) -> Self {
        self.system_prompt_identity = Some(identity.into());
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
        if config.include_snapshot {
            let sandbox = self
                .sandbox
                .clone()
                .expect("filesystem snapshot requires a sandbox handle");
            self.middleware_chain.push(
                crate::middleware::sandbox_snapshot::SandboxSnapshotMiddleware::new(
                    config, sandbox,
                ),
            );
        }
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
        self.sandbox
            .as_ref()
            .expect("skills require a sandbox handle");
        self.skills_enabled = true;
        self.skills_root = Some(config.root);
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
    pub fn with_shell_tool(mut self, mut config: ShellConfig) -> Self {
        if let Some(root) = self
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.canonical_root())
        {
            if config.default_workdir == config.root {
                config.default_workdir = root.clone();
            }
            config.root = root;
        }
        if !self.explicit_permission_policy {
            self.permission_policy = self
                .permission_policy
                .merged(PermissionPolicy::filesystem_default(config.root.clone()));
        }
        self.middleware_chain.push(config.root_hints_middleware());
        let sandbox = self
            .sandbox
            .clone()
            .expect("shell tool requires a sandbox handle");
        self.tools.push(config.into_agent_tool(sandbox));
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
    pub const fn with_provider_retry_config(mut self, config: ProviderRetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Limit logical provider turns for each run. Transport retries do not consume the limit.
    #[must_use]
    pub const fn with_max_provider_turns(mut self, max_provider_turns: u64) -> Self {
        self.max_provider_turns = Some(max_provider_turns);
        self
    }

    /// Set the base directory used for prompt template file references.
    #[must_use]
    pub fn with_template_base_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.template_base_dir = Some(path.into());
        self
    }

    pub(crate) const fn without_file_templates(mut self) -> Self {
        self.file_templates = false;
        self
    }

    pub(crate) const fn with_file_templates(mut self) -> Self {
        self.file_templates = true;
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
        let mut prompt_middleware = SystemPromptMiddleware::new(self.system_prompt.build());
        if let Some(identity) = self.system_prompt_identity {
            prompt_middleware = prompt_middleware.with_identity(identity);
        }
        middleware_chain.push(prompt_middleware);
        if self.file_templates {
            middleware_chain.push(FileTemplateMiddleware);
        }
        middleware_chain.push(PermissionFilteredToolRegistryMiddleware::new(
            self.tools.clone(),
            self.permission_policy.clone(),
            true,
        ));
        middleware_chain.extend(self.middleware_chain);
        if !self.agents_prompt_configs.is_empty() || !self.agents_read_roots.is_empty() {
            let sandbox = self
                .sandbox
                .clone()
                .expect("AGENTS.md requires a sandbox handle");
            middleware_chain.push(
                AgentsInstructionsMiddleware::new(
                    self.agents_prompt_configs,
                    self.agents_read_roots,
                )
                .with_sandbox(sandbox),
            );
        }
        if self.skills_enabled {
            if let Some(root) = self.skills_root {
                let sandbox = self
                    .sandbox
                    .clone()
                    .expect("skills require a sandbox handle");
                middleware_chain.push(
                    crate::middleware::sandbox_skills::SandboxSkillsMiddleware::new(
                        root,
                        self.skills_reveal_roots.clone(),
                        sandbox,
                    ),
                );
            }
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
            max_provider_turns: self.max_provider_turns,
            sandbox: self.sandbox,
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
            .field("system_prompt_identity", &self.system_prompt_identity)
            .field("agents_prompt_configs", &self.agents_prompt_configs)
            .field("agents_read_roots", &self.agents_read_roots)
            .field("skills_enabled", &self.skills_enabled)
            .field("skills_root", &self.skills_root)
            .field("skills_reveal_roots", &self.skills_reveal_roots)
            .field("permission_policy", &self.permission_policy)
            .field(
                "explicit_permission_policy",
                &self.explicit_permission_policy,
            )
            .field("retry_config", &self.retry_config)
            .field("template_base_dir", &self.template_base_dir)
            .field("file_templates", &self.file_templates)
            .field("max_provider_turns", &self.max_provider_turns)
            .finish()
    }
}
