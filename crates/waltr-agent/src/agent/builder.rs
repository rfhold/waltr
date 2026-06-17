//! Agent construction and configuration.

use std::{fmt, path::PathBuf};

use tokio_util::sync::CancellationToken;

use crate::{
    ModelRef, PermissionPolicy, Result, SystemPromptBuilder,
    mcp::McpConfig,
    middleware::{
        AgentMiddleware, agents::AgentsInstructionsMiddleware, chain::MiddlewareChain,
        claims::FilesystemClaimsMiddleware, prompt::SystemPromptMiddleware,
        skills::SkillsMiddleware,
    },
    prompt::{AgentsPromptConfig, FilesystemPromptConfig},
    tools::{
        filesystem::FilesystemConfig,
        question::QuestionConfig,
        skills::{SkillDefinition, SkillsConfig},
    },
};

#[cfg(feature = "shell")]
use crate::{
    middleware::claims::ShellClaimsMiddleware, prompt::ShellPromptConfig, tools::shell::ShellConfig,
};

use super::{Agent, AgentTool};

/// Builder for an agent configuration.
#[derive(Clone)]
pub struct AgentBuilder {
    model: ModelRef,
    tools: Vec<AgentTool>,
    middleware_chain: MiddlewareChain,
    system_prompt: SystemPromptBuilder,
    agents_prompt_configs: Vec<AgentsPromptConfig>,
    agents_read_roots: Vec<PathBuf>,
    skills_catalog: Option<Vec<SkillDefinition>>,
    skills_reveal_roots: Vec<PathBuf>,
    permission_policy: PermissionPolicy,
    explicit_permission_policy: bool,
}

impl AgentBuilder {
    /// Create a builder for a model.
    #[must_use]
    pub fn new(model: ModelRef) -> Self {
        Self {
            model,
            tools: Vec::new(),
            middleware_chain: MiddlewareChain::new(),
            system_prompt: SystemPromptBuilder::new(),
            agents_prompt_configs: Vec::new(),
            agents_read_roots: Vec::new(),
            skills_catalog: None,
            skills_reveal_roots: Vec::new(),
            permission_policy: PermissionPolicy::default(),
            explicit_permission_policy: false,
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
        self.skills_catalog
            .get_or_insert_with(Vec::new)
            .extend(skills);
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

    /// Opt into configured MCP servers as host tools with caller-controlled cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error if any MCP server cannot be initialized or its tools cannot be listed.
    #[allow(clippy::needless_pass_by_value)]
    pub async fn with_mcp_servers_with_cancellation(
        mut self,
        config: McpConfig,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        self.tools.extend(
            config
                .into_agent_tools_with_cancellation(cancellation_token)
                .await?,
        );
        Ok(self)
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

    /// Build the agent configuration.
    ///
    /// Middleware assembly order is deterministic and affects provider request
    /// hashes: the stateful system prompt middleware runs first, then feature
    /// and custom middleware in registration order, then `AGENTS.md`
    /// instructions, then skills.
    #[must_use]
    pub fn build(self) -> Agent {
        let mut middleware_chain = MiddlewareChain::new();
        middleware_chain.push(SystemPromptMiddleware::new(self.system_prompt.build()));
        middleware_chain.extend(self.middleware_chain);
        if !self.agents_prompt_configs.is_empty() || !self.agents_read_roots.is_empty() {
            middleware_chain.push(AgentsInstructionsMiddleware::new(
                self.agents_prompt_configs,
                self.agents_read_roots,
            ));
        }
        if let Some(catalog) = self.skills_catalog {
            middleware_chain.push(SkillsMiddleware::new(catalog, self.skills_reveal_roots));
        }
        Agent {
            model: self.model,
            tools: self.tools,
            middleware_chain,
            permission_policy: self.permission_policy,
        }
    }
}

impl fmt::Debug for AgentBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentBuilder")
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("middleware_chain_len", &self.middleware_chain.len())
            .field("system_prompt", &self.system_prompt)
            .field("agents_prompt_configs", &self.agents_prompt_configs)
            .field("agents_read_roots", &self.agents_read_roots)
            .field("skills_catalog", &self.skills_catalog)
            .field("skills_reveal_roots", &self.skills_reveal_roots)
            .field("permission_policy", &self.permission_policy)
            .field(
                "explicit_permission_policy",
                &self.explicit_permission_policy,
            )
            .finish()
    }
}
