//! Agent construction and configuration.

use std::{path::PathBuf, sync::Arc};

use crate::agent_host::{
    LlmStream, ModelCost, ModelRef, PermissionPolicy, SystemPromptBuilder,
    middleware::{
        AgentMiddleware, agents::AgentsInstructionsMiddleware, chain::MiddlewareChain,
        file_templates::FileTemplateMiddleware,
    },
    prompt::{AgentsPromptConfig, FilesystemPromptConfig},
    tools::{
        filesystem::FilesystemConfig, question::QuestionConfig, skills::SkillsConfig,
        task::TaskConfig,
    },
};

#[cfg(feature = "shell")]
use crate::agent_host::{prompt::ShellPromptConfig, tools::shell::ShellConfig};

use super::{AgentRuntime, Capability, ProviderRetryConfig, SessionStore};

/// Builder for an agent runtime configuration.
#[derive(Clone)]
pub struct HostRuntimeBuilder {
    mcp_source_revision: Option<String>,
    mcp_skills_runtime: Option<std::sync::Arc<crate::agent_host::mcp::McpClientRuntime>>,
    mcp_resources_runtime: Option<(Arc<crate::agent_host::mcp::McpClientRuntime>, bool)>,
    base: waltr_agent::AgentRuntimeBuilder,
    model: ModelRef,
    model_cost: Option<ModelCost>,
    tools: Vec<Capability>,
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
    sandbox: Option<crate::agent_host::sandbox_gateway::SandboxHandle>,
    session: Option<waltr_agent::SessionId>,
    policy: Arc<dyn crate::agent_host::dispatcher::CapabilityPolicy>,
}

impl HostRuntimeBuilder {
    pub(crate) fn with_mcp_source_revision(mut self, revision: String) -> Self {
        self.mcp_source_revision = Some(revision);
        self
    }
    pub(crate) fn with_mcp_skills(
        mut self,
        runtime: Arc<crate::agent_host::mcp::McpClientRuntime>,
    ) -> Self {
        self.mcp_source_revision = Some(runtime.dispatch_source_revision());
        self.mcp_skills_runtime = Some(runtime);
        self
    }
    pub(crate) fn with_mcp_resources(
        mut self,
        runtime: Arc<crate::agent_host::mcp::McpClientRuntime>,
        ordinary: bool,
    ) -> Self {
        self.mcp_source_revision = Some(runtime.dispatch_source_revision());
        self.mcp_resources_runtime = Some((runtime, ordinary));
        self
    }
    /// Create a builder for a model.
    #[must_use]
    pub fn new<P>(model: ModelRef, provider: Arc<P>) -> Self
    where
        P: LlmStream + 'static,
    {
        Self::new_dyn(model, provider)
    }

    /// Create a builder from a host-resolved provider trait object.
    #[must_use]
    pub fn new_dyn(model: ModelRef, provider: Arc<dyn LlmStream>) -> Self {
        Self::from_provider(waltr_agent::AgentRuntimeBuilder::new_dyn(model, provider))
    }

    pub fn from_provider(base: waltr_agent::AgentRuntimeBuilder) -> Self {
        let model = base.model().clone();
        Self {
            mcp_source_revision: None,
            mcp_skills_runtime: None,
            mcp_resources_runtime: None,
            base,
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
            session: None,
            policy: Arc::new(crate::agent_host::dispatcher::UnconfiguredPolicy),
        }
    }

    pub(crate) const fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn with_dispatch_context(
        mut self,
        session: Option<waltr_agent::SessionId>,
        policy: Arc<dyn crate::agent_host::dispatcher::CapabilityPolicy>,
    ) -> Self {
        self.session = session;
        self.policy = policy;
        self
    }
    /// Bind workspace-dependent tools and context to a connected sandbox.
    #[must_use]
    pub fn with_sandbox(
        mut self,
        sandbox: crate::agent_host::sandbox_gateway::SandboxHandle,
    ) -> Self {
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
                crate::agent_host::middleware::sandbox_snapshot::SandboxSnapshotMiddleware::new(
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
            .push(crate::agent_host::middleware::question::QuestionMiddleware);
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
    pub fn with_tool(mut self, tool: Capability) -> Self {
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

    pub(crate) fn narrowed_permissions(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = self.permission_policy.intersection(policy);
        self.explicit_permission_policy = true;
        self
    }

    /// Network-free whole-tool admission projection; not execution authorization.
    pub(crate) fn helper_capture_policy(&self) -> PermissionPolicy {
        self.permission_policy.clone()
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
    /// hashes: the remote-skill provenance guard runs first, then the stateful
    /// system prompt middleware, then feature
    /// and custom middleware in registration order, then `AGENTS.md`
    /// instructions, then skills.
    #[must_use]
    pub fn build(self) -> AgentRuntime {
        self.build_with_dispatcher().0
    }

    pub fn build_with_dispatcher(
        mut self,
    ) -> (
        AgentRuntime,
        crate::agent_host::dispatcher::CapabilityDispatcher,
    ) {
        if self.skills_enabled || self.mcp_skills_runtime.is_some() {
            self.tools.push(Capability::new(
                crate::agent_host::middleware::skills::SkillsMiddleware::definition(),
                |context| {
                    Ok(context
                        .result("skill context did not handle this invocation")
                        .with_error())
                },
            ));
        }
        if self.mcp_resources_runtime.is_some() || self.mcp_skills_runtime.is_some() {
            let ordinary = self
                .mcp_resources_runtime
                .as_ref()
                .is_some_and(|(_, ordinary)| *ordinary);
            self.tools.push(Capability::new(
                crate::agent_host::middleware::mcp_resources::McpResourcesMiddleware::definition(
                    ordinary,
                ),
                |context| {
                    Ok(context
                        .result("resource context did not handle this invocation")
                        .with_error())
                },
            ));
        }
        let mut middleware_chain = MiddlewareChain::new();
        let remote = crate::agent_host::middleware::mcp_skills::McpSkillsMiddleware::new(
            self.mcp_skills_runtime.clone(),
        );
        middleware_chain.push(remote.clone());
        if self.file_templates {
            middleware_chain.push(FileTemplateMiddleware);
        }
        middleware_chain.push(
            crate::agent_host::middleware::tools::PermissionFilteredToolRegistryMiddleware::new(
                self.tools.clone(),
                self.permission_policy.clone(),
                true,
            ),
        );
        middleware_chain.extend(self.middleware_chain);
        if let Some((runtime, ordinary)) = self.mcp_resources_runtime {
            middleware_chain.push(if ordinary {
            crate::agent_host::middleware::mcp_resources::McpResourcesMiddleware::new(runtime)
        } else {
            crate::agent_host::middleware::mcp_resources::McpResourcesMiddleware::skill_references(runtime)
        });
        } else if let Some(runtime) = self.mcp_skills_runtime.clone() {
            middleware_chain.push(crate::agent_host::middleware::mcp_resources::McpResourcesMiddleware::skill_references(runtime));
        }
        if !self.agents_prompt_configs.is_empty() || !self.agents_read_roots.is_empty() {
            middleware_chain.push(
                AgentsInstructionsMiddleware::new(
                    self.agents_prompt_configs,
                    self.agents_read_roots,
                )
                .with_sandbox(self.sandbox.clone().expect("instructions require worker")),
            );
        }
        let mut local = None;
        if self.skills_enabled {
            if let Some(root) = self.skills_root {
                let adapter =
                    crate::agent_host::middleware::sandbox_skills::SandboxSkillsMiddleware::new(
                        root,
                        self.skills_reveal_roots,
                        self.sandbox.clone().expect("skills require worker"),
                    );
                middleware_chain.push(adapter.clone());
                local = Some(adapter);
            }
        }
        if local.is_some() || self.mcp_skills_runtime.is_some() {
            middleware_chain.push(
                crate::agent_host::middleware::skills::SkillsMiddleware::new(
                    local,
                    remote,
                    self.mcp_skills_runtime.is_some(),
                ),
            );
        }
        if !self
            .tools
            .iter()
            .any(|tool| tool.definition().name == "read")
        {
            if let Some(sandbox) = &self.sandbox {
                self.tools.push(
                    Capability::worker(
                        crate::agent_host::tools::filesystem::FilesystemToolKind::Read.definition(),
                        sandbox.clone(),
                    )
                    .with_identity("host-context:read")
                    .with_model_exposure(false),
                );
            }
        }
        let mut dispatcher = crate::agent_host::dispatcher::CapabilityDispatcher::new(
            self.tools,
            self.session,
            self.policy,
        )
        .expect("runtime capability names and identities must be unique")
        .with_context(middleware_chain.clone(), self.permission_policy.clone());
        if let Some(source) = self.mcp_source_revision {
            dispatcher = dispatcher.with_revision_source(&source);
        }
        let host_context = std::sync::Arc::new(crate::agent_host::ServerHostContext::new(
            middleware_chain,
            self.permission_policy,
            self.template_base_dir,
            self.sandbox,
            dispatcher.clone(),
        ));
        let mut base = self.base.with_host_context(host_context);
        if let Some(cost) = self.model_cost {
            base = base.with_model_cost(cost);
        }
        if let Some(prompt) = self.system_prompt.build() {
            base = base.with_system_prompt(prompt);
        }
        if let Some(identity) = self.system_prompt_identity {
            base = base.with_system_prompt_identity(identity);
        }
        for tool in dispatcher.agent_tools() {
            base = base.with_tool(tool);
        }
        base = base.with_provider_retry_config(self.retry_config);
        if let Some(limit) = self.max_provider_turns {
            base = base.with_max_provider_turns(limit);
        }
        (base.build(), dispatcher)
    }
}
