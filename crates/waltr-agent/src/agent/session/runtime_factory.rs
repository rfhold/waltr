use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
};

use crate::{
    AgentFeature, AgentPermissionSelector, AgentRuntime, AgentRuntimeBuilder, AgentsPromptConfig,
    AuthTarget, AuthenticationFailure, BoxCompletionStream, BoxStreamFuture, CompletionRequest,
    FilesystemPromptConfig, LlmError, LlmStream, McpFeatureConfig, ModelApi, ModelCost, ModelRef,
    PermissionEffect, PermissionLevel, PermissionPolicy, PermissionToolSelector, ProviderId,
    ResolvedAgent, ResolvedPermission, SkillsConfig,
    definition::{AgentDefinitionBundle, AgentDefinitionCatalog, McpServerSelection},
    filesystem::FilesystemConfig,
    mcp::{McpClientRuntime, McpConfig, McpServerConfig},
    provider::{
        AnthropicMessages, OpenAiChatCompletions, OpenAiReasoningConfig, OpenAiReasoningEffort,
        OpenAiReasoningSummary, OpenAiResponses, OpenAiResponsesWebSocket,
    },
    question::QuestionConfig,
    tools::task::TaskConfig,
};
#[cfg(feature = "shell")]
use crate::{ShellPromptConfig, shell::ShellConfig};
use futures_util::StreamExt as _;
use mcp::{McpTracePropagation, client::McpBearerProviderError};

mod headers;
mod oauth;
#[cfg(test)]
#[path = "runtime_factory/test_sandbox.rs"]
pub(super) mod test_sandbox;

use headers::{configured_credential, is_credential_header, resolve_provider_headers};
use oauth::now_ms;
pub use oauth::{CredentialRefreshCoordinator, CredentialRefreshError, RefreshMode};

use super::{
    AgentSessionRuntimeError, CredentialStore, InferenceProviderSettings, InferenceSettings,
    OAuthCredential, SessionCoordinator, SessionRecord, SessionStore,
};

const ENV_PREFIX: &str = "${env:";
const OAUTH_REFRESH_AHEAD_MS: i64 = 5 * 60 * 1000;

type RuntimeProviderOverride = Arc<
    dyn Fn(&InferenceSettings) -> Result<AgentRuntimeProvider, AgentSessionRuntimeError>
        + Send
        + Sync,
>;
type AgentCatalogProvider = Arc<dyn Fn() -> AgentDefinitionCatalog + Send + Sync>;

/// Provider-neutral request for host-resolved MCP connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConnectionResolveRequest {
    /// Definition-owned server selection.
    pub servers: McpServerSelection,
}

/// Transport used by one host-resolved MCP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpConnectionKind {
    /// Child process connected over standard input/output.
    Stdio,
    /// Remote Streamable HTTP endpoint.
    StreamableHttp,
}

/// Current host-resolved data needed to start one MCP connection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct McpResolvedConnection {
    /// Stable configured server key.
    pub key: String,
    /// Connection transport.
    pub kind: McpConnectionKind,
    /// Command line for a stdio connection.
    pub command: String,
    /// Endpoint for a Streamable HTTP connection.
    pub url: String,
}

/// Asynchronous result returned by an MCP connection resolver.
pub type McpConnectionResolveFuture = Pin<
    Box<dyn Future<Output = Result<Vec<McpResolvedConnection>, AgentSessionRuntimeError>> + Send>,
>;

type McpConnectionResolver =
    Arc<dyn Fn(McpConnectionResolveRequest) -> McpConnectionResolveFuture + Send + Sync>;

/// Identity of a prepared MCP runtime. Skills additionally bind credential incarnations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRuntimeFingerprint {
    pub trace_propagation: bool,
    pub(super) connections: Vec<McpRuntimeFingerprintConnection>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct McpRuntimeFingerprintConnection {
    pub(super) key: String,
    pub(super) kind: McpConnectionKind,
    pub(super) target: String,
}

impl McpRuntimeFingerprintConnection {
    pub(super) fn from_resolved(connection: &McpResolvedConnection) -> Self {
        Self {
            key: connection.key.clone(),
            kind: connection.kind,
            target: match connection.kind {
                McpConnectionKind::Stdio => connection.command.clone(),
                McpConnectionKind::StreamableHttp => connection.url.clone(),
            },
        }
    }
}

/// Fully resolved MCP preparation data for one reconstruction.
#[derive(Debug)]
pub struct PreparedMcpRuntime {
    pub fingerprint: McpRuntimeFingerprint,
    pub connections: Vec<McpResolvedConnection>,
    pub config: McpConfig,
    pub credentialed_servers: BTreeSet<String>,
}

/// Provider-only input for session runtime construction.
#[derive(Clone)]
pub struct AgentRuntimeProvider {
    model: ModelRef,
    provider: Arc<dyn LlmStream>,
    model_cost: Option<ModelCost>,
}

impl AgentRuntimeProvider {
    /// Create provider input for a model.
    #[must_use]
    pub fn new<P>(model: ModelRef, provider: Arc<P>) -> Self
    where
        P: LlmStream + 'static,
    {
        Self {
            model,
            provider,
            model_cost: None,
        }
    }

    /// Set model pricing used to journal per-turn cost.
    #[must_use]
    pub fn with_model_cost(mut self, cost: ModelCost) -> Self {
        self.model_cost = Some(cost);
        self
    }

    fn into_builder(self) -> AgentRuntimeBuilder {
        builder_with_model_cost(
            AgentRuntimeBuilder::new_dyn(self.model, self.provider),
            self.model_cost,
        )
    }
}

impl std::fmt::Debug for AgentRuntimeProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRuntimeProvider")
            .field("model", &self.model)
            .field("model_cost", &self.model_cost)
            .finish_non_exhaustive()
    }
}

/// Agent runtime factory shared by sessions.
#[derive(Clone)]
pub struct AgentSessionRuntimeFactory {
    /// Workspace root for tools and prompts.
    pub root: PathBuf,
    /// Required worker connection for workspace-dependent runtime behavior.
    pub sandbox: crate::sandbox_gateway::SandboxHandle,
    /// Credential store used for provider auth.
    pub credential_store: CredentialStore,
    outbound_oauth_loopback_test_mode: bool,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    definition: ResolvedAgent,
    catalog: Option<AgentDefinitionCatalog>,
    catalog_provider: Option<AgentCatalogProvider>,
    runtime_provider_override: Option<RuntimeProviderOverride>,
    mcp_connection_resolver: Option<McpConnectionResolver>,
}

impl std::fmt::Debug for AgentSessionRuntimeFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionRuntimeFactory")
            .field("root", &self.root)
            .field("sandbox", &self.sandbox)
            .field("credential_store", &self.credential_store)
            .field(
                "outbound_oauth_loopback_test_mode",
                &self.outbound_oauth_loopback_test_mode,
            )
            .field("trusted_mcp_oauth_origins", &self.trusted_mcp_oauth_origins)
            .field("definition", &self.definition)
            .field("catalog", &self.catalog)
            .field(
                "catalog_provider",
                &self.catalog_provider.as_ref().map(|_| "<provider>"),
            )
            .field(
                "runtime_provider_override",
                &self
                    .runtime_provider_override
                    .as_ref()
                    .map(|_| "<override>"),
            )
            .field(
                "mcp_connection_resolver",
                &self.mcp_connection_resolver.as_ref().map(|_| "<resolver>"),
            )
            .finish_non_exhaustive()
    }
}

impl AgentSessionRuntimeFactory {
    /// Create a runtime factory.
    #[must_use]
    pub fn new(
        sandbox: crate::sandbox_gateway::SandboxHandle,
        root: PathBuf,
        credential_store: impl Into<CredentialStore>,
        definition: ResolvedAgent,
    ) -> Self {
        Self {
            root,
            sandbox,
            credential_store: credential_store.into(),
            outbound_oauth_loopback_test_mode: false,
            trusted_mcp_oauth_origins: Vec::new(),
            definition,
            catalog: None,
            catalog_provider: None,
            runtime_provider_override: None,
            mcp_connection_resolver: None,
        }
    }

    /// Configure every agent selectable from the current startup catalog.
    ///
    /// # Panics
    ///
    /// Panics if the catalog does not contain its declared default agent.
    #[must_use]
    pub fn with_agent_catalog(mut self, catalog: AgentDefinitionCatalog) -> Self {
        self.definition = catalog
            .resolved(catalog.default_agent_name())
            .expect("catalog default agent must be present")
            .clone();
        self.catalog = Some(catalog);
        self.catalog_provider = None;
        self
    }

    /// Configure a synchronous provider for agents selectable by future explicit switches.
    ///
    /// The factory's initial definition and default name remain stable. Only
    /// [`Self::bundle_for_agent`] consults this provider.
    #[must_use]
    pub fn with_agent_catalog_provider(
        mut self,
        provide: impl Fn() -> AgentDefinitionCatalog + Send + Sync + 'static,
    ) -> Self {
        self.catalog_provider = Some(Arc::new(provide));
        self
    }

    /// Return the current default agent name.
    #[must_use]
    pub fn default_agent_name(&self) -> &str {
        self.catalog.as_ref().map_or(
            self.definition.name.as_str(),
            AgentDefinitionCatalog::default_agent_name,
        )
    }

    /// Build a fresh durable bundle for a currently selectable agent.
    ///
    /// # Errors
    ///
    /// Returns [`AgentSessionRuntimeError::FailedPrecondition`] when `name` is absent from the
    /// current catalog or this factory has no catalog.
    pub fn bundle_for_agent(
        &self,
        name: &str,
    ) -> Result<AgentDefinitionBundle, AgentSessionRuntimeError> {
        let provided_catalog = self.catalog_provider.as_ref().map(|provide| provide());
        let bundle = provided_catalog
            .as_ref()
            .or(self.catalog.as_ref())
            .and_then(|catalog| catalog.bundle(name))
            .ok_or_else(|| {
                AgentSessionRuntimeError::FailedPrecondition(format!(
                    "agent '{name}' is not present in the current catalog"
                ))
            })?;
        Ok(bundle)
    }

    /// Permit private HTTPS peers for host-supplied MCP OAuth origins.
    #[must_use]
    pub fn with_trusted_mcp_oauth_origins(mut self, origins: Vec<reqwest::Url>) -> Self {
        self.trusted_mcp_oauth_origins = origins;
        self
    }

    /// Permit private HTTPS peers for one host-supplied MCP OAuth origin.
    #[must_use]
    pub fn with_trusted_mcp_oauth_origin(self, origin: reqwest::Url) -> Self {
        self.with_trusted_mcp_oauth_origins(vec![origin])
    }

    #[cfg(test)]
    const fn with_outbound_oauth_loopback_test_mode(mut self) -> Self {
        self.outbound_oauth_loopback_test_mode = true;
        self
    }

    /// Override provider construction while retaining definition-owned runtime behavior.
    #[must_use]
    pub fn with_runtime_provider(
        mut self,
        build: impl Fn(&InferenceSettings) -> Result<AgentRuntimeProvider, AgentSessionRuntimeError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.runtime_provider_override = Some(Arc::new(build));
        self
    }

    /// Resolve current host-owned MCP definitions for every runtime reconstruction.
    #[must_use]
    pub fn with_mcp_connection_resolver(
        mut self,
        resolve: impl Fn(McpConnectionResolveRequest) -> McpConnectionResolveFuture
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.mcp_connection_resolver = Some(Arc::new(resolve));
        self
    }

    async fn base_runtime_builder(
        &self,
        settings: &InferenceSettings,
        definition: &ResolvedAgent,
    ) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError> {
        let builder = if let Some(build) = self.runtime_provider_override.as_ref() {
            build(settings)?.into_builder()
        } else {
            self.provider_builder(settings).await?
        };
        Ok(self.apply_definition(builder, definition))
    }

    fn apply_definition(
        &self,
        mut builder: AgentRuntimeBuilder,
        definition: &ResolvedAgent,
    ) -> AgentRuntimeBuilder {
        builder = builder
            .with_sandbox(self.sandbox.clone())
            .with_template_base_dir(self.root.clone())
            .without_file_templates();
        if let Some(prompt) = definition.system_prompt.as_ref() {
            builder = builder.with_system_prompt(prompt.clone());
        }
        if let Some(filesystem) = definition.features.filesystem.as_ref() {
            builder = builder.with_file_templates();
            let mut config = FilesystemConfig::new(self.root.clone());
            if filesystem.agents_read_context {
                config = config.with_agents_read_context();
            }
            if filesystem.skills_context {
                config = config.with_skills_context();
            }
            builder = builder.with_filesystem_tools(config);
            if filesystem.system_prompt {
                builder = builder
                    .with_filesystem_system_prompt(FilesystemPromptConfig::new(self.root.clone()));
            }
        }
        if definition
            .features
            .agents
            .as_ref()
            .is_some_and(|agents| agents.system_prompt)
        {
            builder = builder.with_agents_system_prompt(AgentsPromptConfig::new(self.root.clone()));
        }
        if definition.features.skills.is_some() {
            builder = builder.with_skills(SkillsConfig::new(self.root.clone()));
        }
        if definition.permissions.iter().any(|entry| {
            matches!(&entry.selector, AgentPermissionSelector::Name(name) if name == "question")
        }) {
            builder = builder.with_question_tool(QuestionConfig::new());
        }

        #[cfg(feature = "shell")]
        if let Some(shell) = definition.features.shell.as_ref() {
            builder = builder.with_shell_tool(ShellConfig::new(self.root.clone()));
            if shell.system_prompt {
                builder =
                    builder.with_shell_system_prompt(ShellPromptConfig::new(self.root.clone()));
            }
        }
        builder
    }

    fn definition_permission_policy(
        &self,
        definition: &ResolvedAgent,
        model: &ModelRef,
        mcp_tool_names: &[String],
    ) -> PermissionPolicy {
        let mut policy = PermissionPolicy::new(PermissionLevel::Deny);
        let feature_tools = |feature| {
            self.definition_feature_tool_names(definition, model, feature, mcp_tool_names)
        };

        for feature in [
            AgentFeature::Filesystem,
            AgentFeature::Skills,
            AgentFeature::Shell,
            AgentFeature::Task,
            AgentFeature::Mcp,
        ] {
            if feature_default_permissions(&definition.features, feature) {
                for name in feature_tools(feature) {
                    policy = policy.with_tool_rule(
                        PermissionToolSelector::exact(name),
                        PermissionEffect::allow(),
                    );
                }
            }
        }

        for entry in &definition.permissions {
            let names = match &entry.selector {
                AgentPermissionSelector::Name(name) => vec![name.clone()],
                AgentPermissionSelector::Feature(feature) => feature_tools(*feature),
            };
            for name in names {
                let selector = PermissionToolSelector::exact(name);
                match &entry.permission {
                    ResolvedPermission::Tool(effect) => {
                        policy = policy.with_tool_rule(selector, effect.clone());
                    }
                    ResolvedPermission::Resources(resources) => {
                        for rule in &resources.paths {
                            let pattern = match &rule.pattern {
                                crate::PermissionPathPattern::OutsideRoot(_) => {
                                    crate::PermissionPathPattern::OutsideRoot(self.root.clone())
                                }
                                pattern => pattern.clone(),
                            };
                            policy = policy.with_directory_rule(
                                selector.clone(),
                                pattern,
                                rule.effect.clone(),
                            );
                        }
                        for rule in &resources.commands {
                            policy = policy.with_command_rule(
                                selector.clone(),
                                rule.pattern.clone(),
                                rule.effect.clone(),
                            );
                        }
                    }
                    ResolvedPermission::Inputs(rules) => {
                        for rule in rules {
                            policy = policy.with_input_rule(selector.clone(), rule.clone());
                        }
                    }
                }
            }
        }
        policy
    }

    fn definition_feature_tool_names(
        &self,
        definition: &ResolvedAgent,
        model: &ModelRef,
        feature: AgentFeature,
        mcp_tool_names: &[String],
    ) -> Vec<String> {
        match feature {
            AgentFeature::Filesystem if definition.features.filesystem.is_some() => {
                FilesystemConfig::new(self.root.clone())
                    .exposed_tools(model)
                    .all()
                    .into_iter()
                    .map(|kind| kind.name().to_owned())
                    .collect()
            }
            AgentFeature::Skills if definition.features.skills.is_some() => {
                vec!["load_skill".to_owned()]
            }
            #[cfg(feature = "shell")]
            AgentFeature::Shell if definition.features.shell.is_some() => {
                vec!["shell".to_owned()]
            }
            AgentFeature::Task if definition.features.task.is_some() => vec!["task".to_owned()],
            AgentFeature::Mcp
                if definition
                    .features
                    .mcp
                    .as_ref()
                    .is_some_and(|mcp| mcp.expose_tools || mcp.skills || mcp.resources) =>
            {
                mcp_tool_names.to_vec()
            }
            AgentFeature::Agents
            | AgentFeature::Filesystem
            | AgentFeature::Skills
            | AgentFeature::Shell
            | AgentFeature::Task
            | AgentFeature::Mcp => Vec::new(),
        }
    }

    /// Assemble a finished runtime from one durable session record.
    pub(crate) async fn runtime_for_session<S>(
        &self,
        record: &SessionRecord,
        sessions: SessionCoordinator<S>,
        mcp_runtime: Option<Arc<McpClientRuntime>>,
    ) -> Result<AgentRuntime, AgentSessionRuntimeError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let mut factory = self.clone();
        factory.root = self.sandbox_root()?;
        let settings = record.inference_settings.as_ref().ok_or_else(|| {
            AgentSessionRuntimeError::FailedPrecondition(
                "session runtime settings are required before prompting".to_owned(),
            )
        })?;
        let definition = factory.definition_for_session(record)?;
        let identity = record.agent_definition.as_ref().map_or_else(
            || {
                factory.catalog.as_ref().map_or_else(
                    || factory.default_agent_name().to_owned(),
                    |catalog| catalog.default_bundle().identity(),
                )
            },
            AgentDefinitionBundle::identity,
        );
        let mut builder = factory.base_runtime_builder(settings, &definition).await?;
        builder = builder.with_system_prompt_identity(identity);
        if let Some(task) = definition.features.task.as_ref() {
            let captured_bundle = record.agent_definition.clone().or_else(|| {
                factory
                    .catalog
                    .as_ref()
                    .map(AgentDefinitionCatalog::default_bundle)
            });
            builder = builder.with_task_tool(
                TaskConfig::new(sessions)
                    .with_agent_definition(captured_bundle)
                    .with_max_subagent_depth(task.max_subagent_depth),
            );
        }
        let mut mcp_tool_names = Vec::new();
        if let Some(mcp_runtime) = mcp_runtime
            && let Some(mcp) = definition.features.mcp.as_ref()
        {
            if mcp.skills {
                mcp_tool_names.extend(["load_skill".to_owned()]);
                builder = builder.with_mcp_skills(Arc::clone(&mcp_runtime));
            }
            if mcp.expose_tools {
                for tool in mcp_runtime.agent_tools().await {
                    mcp_tool_names.push(tool.definition().name.clone());
                    builder = builder.with_tool(tool);
                }
            }
            if mcp.instructions {
                builder = builder.with_middleware(
                    crate::middleware::mcp_instructions::McpInstructionsMiddleware::new(
                        Arc::clone(&mcp_runtime),
                    ),
                );
            }
            if mcp.resources || mcp.skills {
                mcp_tool_names.push("read_mcp_resource".to_owned());
                builder = builder.with_mcp_resources(Arc::clone(&mcp_runtime), mcp.resources);
            }
            if mcp.prompts {
                builder = builder.with_middleware(
                    crate::middleware::mcp_prompts::McpPromptsMiddleware::new(mcp_runtime),
                );
            }
        }
        let policy =
            factory.definition_permission_policy(&definition, builder.model(), &mcp_tool_names);
        Ok(builder.with_permission_policy(policy).build())
    }

    fn definition_for_session(
        &self,
        record: &SessionRecord,
    ) -> Result<ResolvedAgent, AgentSessionRuntimeError> {
        let Some(bundle) = record.agent_definition.as_ref() else {
            return Ok(self.definition.clone());
        };
        bundle.resolve().map_err(|error| {
            AgentSessionRuntimeError::FailedPrecondition(format!(
                "stored session agent definition is invalid: {error}"
            ))
        })
    }

    pub(crate) fn mcp_feature_for_session(
        &self,
        record: &SessionRecord,
    ) -> Result<Option<McpFeatureConfig>, AgentSessionRuntimeError> {
        Ok(self.definition_for_session(record)?.features.mcp)
    }

    pub(crate) async fn prepare_mcp_runtime(
        &self,
        feature: &McpFeatureConfig,
    ) -> Result<Option<PreparedMcpRuntime>, AgentSessionRuntimeError> {
        if matches!(&feature.servers, McpServerSelection::Include(keys) if keys.is_empty()) {
            return Ok(None);
        }
        let connections = match &self.mcp_connection_resolver {
            Some(resolve) => {
                resolve(McpConnectionResolveRequest {
                    servers: feature.servers.clone(),
                })
                .await?
            }
            None => match &feature.servers {
                McpServerSelection::Include(keys) if !keys.is_empty() => {
                    return Err(missing_mcp_servers(keys));
                }
                _ => Vec::new(),
            },
        };
        let mut connections = connections;
        connections.sort();
        if let McpServerSelection::Include(requested) = &feature.servers {
            let resolved = connections
                .iter()
                .map(|connection| connection.key.as_str())
                .collect::<BTreeSet<_>>();
            let missing = requested
                .iter()
                .filter(|key| !resolved.contains(key.as_str()))
                .cloned()
                .collect::<BTreeSet<_>>();
            if !missing.is_empty() {
                return Err(missing_mcp_servers(&missing));
            }
        }
        if connections.is_empty() {
            return Ok(None);
        }
        let mut fingerprint_connections = Vec::new();
        for connection in &connections {
            let mut identity = McpRuntimeFingerprintConnection::from_resolved(connection);
            if feature.skills {
                use sha2::{Digest as _, Sha256};
                let token = self.mcp_bearer_token(connection).await?;
                let bytes = serde_json::to_vec(&token).expect("optional token serializes");
                let digest = Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                identity
                    .target
                    .push_str(&format!("#skills-credential:{digest}"));
            }
            fingerprint_connections.push(identity);
        }
        let fingerprint = McpRuntimeFingerprint {
            trace_propagation: feature.trace_propagation,
            connections: fingerprint_connections,
        };
        let (mut config, credentialed_servers) = self
            .mcp_config_with_credentials(&connections, feature.trace_propagation)
            .await?;
        if feature.skills {
            config = config.with_skills();
        }
        Ok(Some(PreparedMcpRuntime {
            fingerprint,
            connections,
            config,
            credentialed_servers,
        }))
    }

    pub(crate) async fn preflight_cached_mcp_authentication(
        &self,
        connections: &[McpResolvedConnection],
        credentialed_servers: &BTreeSet<String>,
    ) -> Result<(), AgentSessionRuntimeError> {
        for server in connections
            .iter()
            .filter(|server| credentialed_servers.contains(&server.key))
        {
            if self.mcp_bearer_token(server).await?.is_none() {
                return Err(AgentSessionRuntimeError::Authentication {
                    target: AuthTarget::mcp_server(&server.key),
                    message: "MCP OAuth credential is required".to_owned(),
                    reason: crate::AuthenticationFailureReason::CredentialRequired,
                    mcp_authorization: None,
                });
            }
        }
        Ok(())
    }

    fn sandbox_root(&self) -> Result<PathBuf, AgentSessionRuntimeError> {
        let root = self.sandbox.canonical_root().ok_or_else(|| {
            AgentSessionRuntimeError::FailedPrecondition(
                "sandbox worker root is unavailable".into(),
            )
        })?;
        if root.as_os_str().is_empty() {
            return Err(AgentSessionRuntimeError::FailedPrecondition(
                "sandbox worker root is missing".into(),
            ));
        }
        Ok(root)
    }

    pub(crate) fn mcp_root_uri(&self) -> Result<String, AgentSessionRuntimeError> {
        Ok(format!("file://{}", self.sandbox_root()?.display()))
    }

    #[allow(clippy::too_many_lines)]
    async fn provider_builder(
        &self,
        settings: &InferenceSettings,
    ) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError> {
        let provider = &settings.provider;
        let headers = resolve_provider_headers(&provider.headers)?;
        let oauth = self.oauth_access(provider, &headers).await?;
        let model_id = provider.model_id.clone();
        let endpoint_url = (!provider.url.trim().is_empty()).then_some(provider.url.as_str());

        match provider.api {
            ModelApi::AnthropicMessages => {
                ensure_no_reasoning_effort(settings, "Anthropic Messages")?;
                let oauth_active = oauth.is_some();
                let oauth_billing_version = if oauth_active {
                    configured_claude_code_version(&headers)?.map(ToOwned::to_owned)
                } else {
                    None
                };
                let mut provider_impl = AnthropicMessages::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = provider_impl.with_oauth_billing_version(oauth_billing_version);
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
            ModelApi::OpenAiChatCompletions => {
                ensure_no_reasoning_effort(settings, "OpenAI Chat Completions")?;
                let oauth_active = oauth.is_some();
                let mut provider_impl = OpenAiChatCompletions::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
            ModelApi::OpenAiResponses => {
                let oauth_active = oauth.is_some();
                let mut provider_impl = OpenAiResponses::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                if let Some(reasoning) = openai_reasoning_config(settings)? {
                    provider_impl = provider_impl.with_reasoning(reasoning);
                }
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
            ModelApi::OpenAiResponsesWebSocket => {
                let oauth_active = oauth.is_some();
                let mut provider_impl = OpenAiResponsesWebSocket::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                if let Some(reasoning) = openai_reasoning_config(settings)? {
                    provider_impl = provider_impl.with_reasoning(reasoning);
                }
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(
                        model,
                        Arc::new(TargetedProvider::new(provider_impl, &provider.key)),
                    ),
                    provider.model_cost.clone(),
                ))
            }
        }
    }

    async fn oauth_access(
        &self,
        provider: &InferenceProviderSettings,
        resolved_headers: &BTreeMap<String, String>,
    ) -> Result<Option<OAuthCredential>, AgentSessionRuntimeError> {
        if configured_credential(resolved_headers).is_some() {
            return Ok(None);
        }
        let Some(definition) = provider.oauth.as_ref() else {
            return Ok(None);
        };
        let credential = self
            .refresh_coordinator()
            .refresh_provider(&provider.key, definition, RefreshMode::Normal)
            .await
            .map_err(|error| refresh_runtime_error(&error))?;
        credential
            .map(Some)
            .ok_or_else(|| AgentSessionRuntimeError::Authentication {
                target: AuthTarget::provider(&provider.key),
                message: "provider OAuth credential is required".to_owned(),
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                mcp_authorization: None,
            })
    }

    #[allow(clippy::too_many_lines)]
    async fn mcp_config_with_credentials(
        &self,
        connections: &[McpResolvedConnection],
        trace_propagation: bool,
    ) -> Result<(McpConfig, BTreeSet<String>), AgentSessionRuntimeError> {
        let mut mcp = McpConfig::new();
        let mut credentialed_servers = BTreeSet::new();
        if trace_propagation {
            mcp = mcp.with_trace_propagation(McpTracePropagation::TraceContext);
        }
        for server in connections {
            let config = match server.kind {
                McpConnectionKind::Stdio => {
                    let mut parts = server.command.split_whitespace();
                    let command = parts.next().ok_or_else(|| {
                        AgentSessionRuntimeError::FailedPrecondition(format!(
                            "mcp server '{}' command must not be empty",
                            server.key
                        ))
                    })?;
                    McpServerConfig::stdio(server.key.clone(), command, parts)
                }
                McpConnectionKind::StreamableHttp => {
                    let credential_key = AuthTarget::mcp_server(&server.key).credential_key();
                    if self
                        .credential_store
                        .read(&credential_key)
                        .await
                        .map_err(|error| {
                            AgentSessionRuntimeError::Internal(format!(
                                "failed to read mcp credentials: {error}"
                            ))
                        })?
                        .is_none()
                    {
                        mcp = mcp.with_server(McpServerConfig::streamable_http(
                            server.key.clone(),
                            server.url.clone(),
                        ));
                        continue;
                    }
                    self.mcp_bearer_token(server).await?;
                    credentialed_servers.insert(server.key.clone());
                    let factory = self.clone();
                    let server_settings = server.clone();
                    let provider = Arc::new(move || {
                        let factory = factory.clone();
                        let server = server_settings.clone();
                        async move {
                            factory
                                .mcp_bearer_token(&server)
                                .await
                                .map_err(|error| match error {
                                    AgentSessionRuntimeError::Authentication {
                                        reason: crate::AuthenticationFailureReason::NetworkFailed,
                                        ..
                                    } => McpBearerProviderError::Unavailable,
                                    AgentSessionRuntimeError::Authentication { .. } => {
                                        McpBearerProviderError::Authorization
                                    }
                                    _ => McpBearerProviderError::Unavailable,
                                })?
                                .ok_or(McpBearerProviderError::Authorization)
                        }
                    });
                    McpServerConfig::streamable_http_with_bearer_provider(
                        server.key.clone(),
                        server.url.clone(),
                        provider,
                    )
                }
            };
            mcp = mcp.with_server(config);
        }
        Ok((mcp, credentialed_servers))
    }

    #[allow(clippy::too_many_lines)]
    async fn mcp_bearer_token(
        &self,
        server: &McpResolvedConnection,
    ) -> Result<Option<String>, AgentSessionRuntimeError> {
        self.refresh_coordinator()
            .refresh_mcp(server, RefreshMode::Normal)
            .await
            .map(|credential| {
                credential.map(|credential| credential.token.access_token.secret().to_owned())
            })
            .map_err(|error| refresh_runtime_error(&error))
    }

    fn refresh_coordinator(&self) -> CredentialRefreshCoordinator {
        let coordinator = CredentialRefreshCoordinator::new(self.credential_store.clone())
            .with_trusted_mcp_oauth_origins(self.trusted_mcp_oauth_origins.clone());
        #[cfg(test)]
        if self.outbound_oauth_loopback_test_mode {
            return coordinator.with_loopback_test_mode();
        }
        coordinator
    }
}

struct TargetedProvider<T> {
    inner: T,
    target: AuthTarget,
}

impl<T> TargetedProvider<T> {
    fn new(inner: T, provider_key: &str) -> Self {
        Self {
            inner,
            target: AuthTarget::provider(provider_key),
        }
    }
}

impl<T> LlmStream for TargetedProvider<T>
where
    T: LlmStream,
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<serde_json::Value> {
        self.inner.project_request(request)
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let stream = self
                .inner
                .stream_request(request)
                .await
                .map_err(|error| provider_authentication_error(error, &self.target))?;
            Ok(Box::pin(stream.map({
                let target = self.target.clone();
                move |result| result.map_err(|error| provider_authentication_error(error, &target))
            })) as BoxCompletionStream)
        })
    }
}

fn provider_authentication_error(error: LlmError, target: &AuthTarget) -> LlmError {
    let rejected = match &error {
        LlmError::Api {
            status: Some(401), ..
        } => true,
        LlmError::WebSocket(error) => matches!(
            error,
            tokio_tungstenite::tungstenite::Error::Http(response)
                if response.status() == http::StatusCode::UNAUTHORIZED
        ),
        _ => false,
    };
    if rejected {
        LlmError::AuthenticationFailure {
            failure: AuthenticationFailure {
                target: target.clone(),
                reason: crate::AuthenticationFailureReason::CredentialRejected,
                mcp_authorization: None,
            },
            message: "provider rejected the configured credential".to_owned(),
        }
    } else {
        error
    }
}

fn refresh_runtime_error(error: &CredentialRefreshError) -> AgentSessionRuntimeError {
    if error.is_internal() {
        AgentSessionRuntimeError::Internal(error.message().to_owned())
    } else {
        AgentSessionRuntimeError::Authentication {
            target: error.target().clone(),
            message: error.message().to_owned(),
            reason: error.reason(),
            mcp_authorization: None,
        }
    }
}

fn should_refresh_mcp_token(credential: &OAuthCredential) -> bool {
    credential
        .token
        .expires_by(now_ms().saturating_add(OAUTH_REFRESH_AHEAD_MS))
}

fn validate_mcp_access_token(
    server_key: &str,
    credential: &OAuthCredential,
) -> Result<(), AgentSessionRuntimeError> {
    if credential.token.access_token.secret().is_empty()
        || !credential.token.token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(AgentSessionRuntimeError::Authentication {
            target: AuthTarget::mcp_server(server_key),
            message: "MCP OAuth credential does not contain a supported bearer access token"
                .to_owned(),
            reason: crate::AuthenticationFailureReason::ResponseInvalid,
            mcp_authorization: None,
        });
    }
    Ok(())
}

fn validate_mcp_credential(
    server: &McpResolvedConnection,
    credential: &OAuthCredential,
) -> Result<(), AgentSessionRuntimeError> {
    let Some(context) = credential.mcp_context() else {
        return Err(mcp_authorization_error(
            &server.key,
            "MCP OAuth credential binding is invalid",
            crate::AuthenticationFailureReason::ConfigurationRequired,
        ));
    };
    let registration_current = context
        .registration
        .expires_at_ms
        .is_none_or(|expires_at_ms| expires_at_ms > now_ms());
    let valid = credential.version == super::OAUTH_CREDENTIAL_VERSION
        && context.resource == server.url
        && !context.issuer.is_empty()
        && endpoint_is_bound(&context.issuer)
        && !context.registration.client_id.is_empty()
        && !context.registration.redirect_uri.is_empty()
        && context
            .registration
            .token_endpoint_auth_method
            .eq_ignore_ascii_case("none")
        && registration_current
        && endpoint_is_bound(&context.endpoints.token);
    if valid {
        Ok(())
    } else {
        Err(mcp_authorization_error(
            &server.key,
            "MCP OAuth credential binding is invalid",
            crate::AuthenticationFailureReason::ConfigurationRequired,
        ))
    }
}

fn endpoint_is_bound(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        let safe_scheme = url.scheme() == "https"
            || (url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|address| address.is_loopback())
                }));
        safe_scheme
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn mcp_authorization_error(
    server_key: &str,
    message: impl Into<String>,
    reason: crate::AuthenticationFailureReason,
) -> AgentSessionRuntimeError {
    AgentSessionRuntimeError::Authentication {
        target: AuthTarget::mcp_server(server_key),
        message: message.into(),
        reason,
        mcp_authorization: None,
    }
}

const fn provider_id(api: ModelApi) -> ProviderId {
    match api {
        ModelApi::AnthropicMessages => ProviderId::Anthropic,
        ModelApi::OpenAiChatCompletions
        | ModelApi::OpenAiResponses
        | ModelApi::OpenAiResponsesWebSocket => ProviderId::OpenAi,
    }
}

fn builder_with_model_cost(
    mut builder: AgentRuntimeBuilder,
    model_cost: Option<ModelCost>,
) -> AgentRuntimeBuilder {
    if let Some(cost) = model_cost {
        builder = builder.with_model_cost(cost);
    }
    builder
}

fn feature_default_permissions(
    features: &crate::ResolvedAgentFeatures,
    feature: AgentFeature,
) -> bool {
    match feature {
        AgentFeature::Filesystem => features
            .filesystem
            .as_ref()
            .is_some_and(|config| config.default_permissions),
        AgentFeature::Skills => features
            .skills
            .as_ref()
            .is_some_and(|config| config.default_permissions),
        AgentFeature::Shell => features
            .shell
            .as_ref()
            .is_some_and(|config| config.default_permissions),
        AgentFeature::Task => features
            .task
            .as_ref()
            .is_some_and(|config| config.default_permissions),
        AgentFeature::Mcp => features
            .mcp
            .as_ref()
            .is_some_and(|config| config.default_permissions),
        AgentFeature::Agents => false,
    }
}

fn missing_mcp_servers(keys: &BTreeSet<String>) -> AgentSessionRuntimeError {
    AgentSessionRuntimeError::FailedPrecondition(format!(
        "selected MCP servers are not configured: {}",
        keys.iter().cloned().collect::<Vec<_>>().join(", ")
    ))
}

fn ensure_no_reasoning_effort(
    settings: &InferenceSettings,
    provider_name: &str,
) -> Result<(), AgentSessionRuntimeError> {
    if settings.provider.reasoning_effort.is_some() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
            "{provider_name} does not support session reasoning effort"
        )));
    }
    Ok(())
}

fn openai_reasoning_config(
    settings: &InferenceSettings,
) -> Result<Option<OpenAiReasoningConfig>, AgentSessionRuntimeError> {
    let Some(effort) = settings.provider.reasoning_effort.as_deref() else {
        return Ok(None);
    };
    let effort = match effort {
        "none" => OpenAiReasoningEffort::None,
        "minimal" => OpenAiReasoningEffort::Minimal,
        "low" => OpenAiReasoningEffort::Low,
        "medium" => OpenAiReasoningEffort::Medium,
        "high" => OpenAiReasoningEffort::High,
        "xhigh" => OpenAiReasoningEffort::XHigh,
        value => {
            return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
                "reasoning effort '{value}' is not supported by OpenAI Responses"
            )));
        }
    };
    Ok(Some(OpenAiReasoningConfig {
        effort,
        summary: Some(OpenAiReasoningSummary::Auto),
    }))
}

fn apply_config_headers<T>(
    mut provider: T,
    headers: &BTreeMap<String, String>,
    oauth_active: bool,
) -> T
where
    T: ConfigurableHeaders,
{
    for (name, value) in headers {
        if oauth_active && is_credential_header(name) {
            continue;
        }
        provider = provider.with_header(name, value);
    }
    provider
}

fn configured_claude_code_version(
    headers: &BTreeMap<String, String>,
) -> Result<Option<&str>, AgentSessionRuntimeError> {
    let mut user_agents = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"));
    let Some((_, user_agent)) = user_agents.next() else {
        return Ok(None);
    };
    if user_agents.next().is_some() {
        return Err(AgentSessionRuntimeError::FailedPrecondition(
            "Anthropic OAuth provider must not configure more than one user-agent header"
                .to_owned(),
        ));
    }
    let Some(version) = user_agent.strip_prefix("claude-code/") else {
        return Ok(None);
    };
    Ok((!version.is_empty()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_')))
    .then_some(version))
}

fn apply_oauth_headers<T>(mut provider: T, oauth: &OAuthCredential) -> T
where
    T: ConfigurableHeaders,
{
    provider = provider.with_header(
        "authorization",
        &format!("Bearer {}", oauth.token.access_token.secret()),
    );
    if let Some(account_id) = oauth
        .provider_metadata()
        .and_then(|metadata| metadata.get("account_id"))
    {
        provider = provider
            .with_header("chatgpt-account-id", account_id)
            .with_header("user-agent", &format!("waltr/{}", env!("CARGO_PKG_VERSION")));
    }
    provider
}

trait ConfigurableHeaders: Sized {
    fn with_header(self, name: &str, value: &str) -> Self;
}

impl ConfigurableHeaders for AnthropicMessages {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

impl ConfigurableHeaders for OpenAiChatCompletions {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

impl ConfigurableHeaders for OpenAiResponses {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

impl ConfigurableHeaders for OpenAiResponsesWebSocket {
    fn with_header(self, name: &str, value: &str) -> Self {
        self.with_header(name, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileCredentialStore, McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration,
        McpOAuthRegistrationMethod, OAuthCredentialRecord, OAuthCredentialStore,
        OAuthCredentialStoreFuture, OAuthRefreshClaim, OAuthRefreshClaimResult, OAuthTokenSet,
        SessionCreateRequest, SessionOAuthJwtMetadata, SessionOAuthJwtToken,
        SessionOAuthProviderSettings, SessionOAuthTokenRequestStyle,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc, RwLock,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use tokio::sync::Mutex;

    fn resolved_test_agent(resources: &str) -> ResolvedAgent {
        let mut registry = crate::AgentDefinitionRegistry::new();
        registry
            .load_yaml(&format!(
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: test }}\n{resources}"
            ))
            .expect("load test definition");
        registry.resolve_agent("test").expect("resolve test agent")
    }

    fn empty_test_agent() -> ResolvedAgent {
        resolved_test_agent("spec: {}")
    }

    fn mcp_feature(selection: McpServerSelection, trace_propagation: bool) -> McpFeatureConfig {
        McpFeatureConfig {
            skills: false,
            servers: selection,
            expose_tools: true,
            instructions: true,
            resources: true,
            prompts: true,
            trace_propagation,
            default_permissions: true,
        }
    }

    fn connection(key: &str, kind: McpConnectionKind, target: &str) -> McpResolvedConnection {
        McpResolvedConnection {
            key: key.to_owned(),
            kind,
            command: if kind == McpConnectionKind::Stdio {
                target.to_owned()
            } else {
                String::new()
            },
            url: if kind == McpConnectionKind::StreamableHttp {
                target.to_owned()
            } else {
                String::new()
            },
        }
    }

    #[tokio::test]
    async fn mcp_resolver_receives_selection_and_fingerprint_is_sorted_and_structural() {
        let directory = tempfile::tempdir().expect("temp directory");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let resolver_requests = Arc::clone(&requests);
        let factory = test_factory(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
        )
        .with_mcp_connection_resolver(move |request| {
            let requests = Arc::clone(&resolver_requests);
            Box::pin(async move {
                requests.lock().await.push(request);
                Ok(vec![
                    connection(
                        "zeta",
                        McpConnectionKind::StreamableHttp,
                        "https://example.test/mcp",
                    ),
                    connection("alpha", McpConnectionKind::Stdio, "server --stdio"),
                ])
            })
        });
        let selection = McpServerSelection::Exclude(BTreeSet::from(["ignored".to_owned()]));

        let prepared = factory
            .prepare_mcp_runtime(&mcp_feature(selection.clone(), true))
            .await
            .expect("resolve MCP")
            .expect("non-empty MCP preparation");

        assert_eq!(
            requests.lock().await.as_slice(),
            &[McpConnectionResolveRequest { servers: selection }]
        );
        assert!(prepared.fingerprint.trace_propagation);
        assert_eq!(prepared.fingerprint.connections[0].key, "alpha");
        assert_eq!(prepared.fingerprint.connections[1].key, "zeta");
        let fingerprint =
            |connections: Vec<McpResolvedConnection>, trace_propagation| McpRuntimeFingerprint {
                trace_propagation,
                connections: connections
                    .iter()
                    .map(McpRuntimeFingerprintConnection::from_resolved)
                    .collect(),
            };
        let command_changed = fingerprint(
            vec![connection(
                "alpha",
                McpConnectionKind::Stdio,
                "server --changed",
            )],
            true,
        );
        assert_ne!(prepared.fingerprint, command_changed);
        for changed in [
            McpRuntimeFingerprint {
                trace_propagation: false,
                connections: prepared.fingerprint.connections.clone(),
            },
            fingerprint(
                vec![connection(
                    "alpha",
                    McpConnectionKind::StreamableHttp,
                    "https://example.test/changed",
                )],
                true,
            ),
            fingerprint(
                vec![connection(
                    "different-key",
                    McpConnectionKind::Stdio,
                    "server --stdio",
                )],
                true,
            ),
        ] {
            assert_ne!(prepared.fingerprint, changed);
        }
    }

    #[test]
    fn mcp_fingerprint_ignores_inactive_transport_fields() {
        let stdio = McpResolvedConnection {
            key: "stdio".to_owned(),
            kind: McpConnectionKind::Stdio,
            command: "server --stdio".to_owned(),
            url: "https://inactive.example/one".to_owned(),
        };
        let mut stdio_inactive_changed = stdio.clone();
        stdio_inactive_changed.url = "https://inactive.example/two".to_owned();
        assert_eq!(
            McpRuntimeFingerprintConnection::from_resolved(&stdio),
            McpRuntimeFingerprintConnection::from_resolved(&stdio_inactive_changed)
        );
        let mut stdio_active_changed = stdio.clone();
        stdio_active_changed.command = "server --changed".to_owned();
        assert_ne!(
            McpRuntimeFingerprintConnection::from_resolved(&stdio),
            McpRuntimeFingerprintConnection::from_resolved(&stdio_active_changed)
        );

        let http = McpResolvedConnection {
            key: "http".to_owned(),
            kind: McpConnectionKind::StreamableHttp,
            command: "inactive-command".to_owned(),
            url: "https://example.test/one".to_owned(),
        };

        let mut http_inactive_changed = http.clone();
        http_inactive_changed.command = "changed-inactive-command".to_owned();
        assert_eq!(
            McpRuntimeFingerprintConnection::from_resolved(&http),
            McpRuntimeFingerprintConnection::from_resolved(&http_inactive_changed)
        );
        let mut http_active_changed = http.clone();
        http_active_changed.url = "https://example.test/two".to_owned();
        assert_ne!(
            McpRuntimeFingerprintConnection::from_resolved(&http),
            McpRuntimeFingerprintConnection::from_resolved(&http_active_changed)
        );
    }

    #[tokio::test]
    async fn mcp_resolver_empty_selection_and_missing_includes_are_deterministic() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = test_factory(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
        )
        .with_mcp_connection_resolver(|_| Box::pin(async { Ok(Vec::new()) }));

        assert!(
            factory
                .prepare_mcp_runtime(&mcp_feature(
                    McpServerSelection::Include(BTreeSet::new()),
                    false,
                ))
                .await
                .expect("empty include resolves")
                .is_none()
        );
        let error = factory
            .prepare_mcp_runtime(&mcp_feature(
                McpServerSelection::Include(BTreeSet::from([
                    "zeta".to_owned(),
                    "alpha".to_owned(),
                ])),
                false,
            ))
            .await
            .expect_err("missing includes fail");
        assert_eq!(
            error.to_string(),
            "selected MCP servers are not configured: alpha, zeta"
        );
    }

    fn test_factory(
        root: PathBuf,
        credential_store: impl Into<CredentialStore>,
    ) -> AgentSessionRuntimeFactory {
        AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root,
            credential_store,
            empty_test_agent(),
        )
    }

    #[tokio::test]
    async fn runtime_for_session_rejects_unavailable_worker_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = test_factory(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
        );
        let record = SessionCreateRequest::new("worker-unavailable").materialize();
        let error = factory
            .runtime_for_session(
                &record,
                SessionCoordinator::new(crate::MemorySessionStore::new()),
                None,
            )
            .await
            .expect_err("configured root must not replace an unavailable worker root");
        assert!(
            matches!(error, AgentSessionRuntimeError::FailedPrecondition(message)
            if message == "sandbox worker root is unavailable")
        );
    }

    #[tokio::test]
    async fn mcp_root_uri_requires_worker_and_ignores_configured_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = test_factory(
            PathBuf::from("/not-the-worker-root"),
            FileCredentialStore::new(directory.path().join("auth.json")),
        );
        assert!(matches!(
            factory.mcp_root_uri(),
            Err(AgentSessionRuntimeError::FailedPrecondition(message))
                if message == "sandbox worker root is unavailable"
        ));

        let worker_root = directory
            .path()
            .canonicalize()
            .expect("canonical temp root");
        let factory = AgentSessionRuntimeFactory::new(
            test_sandbox::worker_handle(directory.path()).await,
            PathBuf::from("/not-the-worker-root"),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        assert_eq!(
            factory.mcp_root_uri().expect("worker root URI"),
            format!("file://{}", worker_root.display())
        );
    }

    struct EmptyProvider;

    impl LlmStream for EmptyProvider {
        fn name(&self) -> &'static str {
            "empty"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async { Ok(Box::pin(futures_util::stream::empty()) as BoxCompletionStream) })
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn provider_override_receives_definition_features_and_policy() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().to_path_buf();
        let definition = resolved_test_agent(
            r"spec:
  features:
    - filesystem:
        root: $(session.workspace.root)
        profile: auto
        defaultPermissions: true
  permissions:
    - { name: read, permission: allow }",
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);
        let override_model = model.clone();
        let factory = AgentSessionRuntimeFactory::new(
            test_sandbox::worker_handle(&root).await,
            root.clone(),
            FileCredentialStore::new(root.join("auth.json")),
            definition,
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(
                override_model.clone(),
                Arc::new(EmptyProvider),
            ))
        });

        let mut request = SessionCreateRequest::new("provider-override");
        request.inference_settings = Some(InferenceSettings {
            provider: InferenceProviderSettings {
                key: "unused".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: "gpt-4".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        });
        let record = request.materialize();
        let runtime = factory
            .runtime_for_session(
                &record,
                SessionCoordinator::new(crate::MemorySessionStore::new()),
                None,
            )
            .await
            .expect("build overridden runtime");

        assert_eq!(runtime.model(), &model);
        assert!(
            runtime
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "read")
        );
        assert_eq!(
            runtime.permission_policy.evaluate_tool("read"),
            PermissionLevel::Allow
        );
        assert!(
            runtime
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "apply_patch")
        );
        assert_eq!(
            runtime.permission_policy.evaluate_tool("apply_patch"),
            PermissionLevel::Allow
        );
        for excluded in ["edit", "write", "delete"] {
            assert!(
                runtime
                    .tools()
                    .iter()
                    .all(|tool| tool.definition().name != excluded),
                "unexpected {excluded} tool"
            );
            assert_eq!(
                runtime.permission_policy.evaluate_tool(excluded),
                PermissionLevel::Deny,
                "{excluded} permission"
            );
        }
    }

    #[test]
    fn stored_definition_is_self_contained_for_features_and_permissions() {
        let root = std::env::temp_dir();
        let current = resolved_test_agent(
            r"spec:
  systemPrompt: current prompt
  features: [{ featureSetRef: { name: current-features } }]
  permissions: [{ permissionSetRef: { name: current-permissions } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: current-features }
spec: { skills: { root: $(session.workspace.root) } }
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: current-permissions }
spec: { tools: [{ name: load_skill, permission: allow }] }",
        );
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("waltr-agent-definition-merge-auth.json")),
            current.clone(),
        );
        let mut record = SessionRecord::new("definition-merge");
        record.agent_definition = Some(crate::definition::AgentDefinitionBundle::new(
            "stored",
            vec![
                r"apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata:
  name: stored
spec:
  description: stored description
  systemPrompt: stored prompt
  features: [{ featureSetRef: { name: old-features } }]
  permissions: [{ permissionSetRef: { name: old-permissions } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: old-features }
spec: { filesystem: { root: $(session.workspace.root), profile: auto } }
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: old-permissions }
spec: { tools: [{ name: read, permission: allow }] }
"
                .to_owned(),
            ],
        ));

        let effective = factory
            .definition_for_session(&record)
            .expect("stored definition resolves");

        assert_eq!(effective.name, "stored");
        assert_eq!(effective.description.as_deref(), Some("stored description"));
        assert_eq!(effective.system_prompt.as_deref(), Some("stored prompt"));
        assert_ne!(effective.features, current.features);
        assert_ne!(effective.permissions, current.permissions);
        assert!(effective.features.filesystem.is_some());
        assert!(effective.features.skills.is_none());
    }

    #[test]
    fn malformed_stored_definition_fails_closed() {
        let root = std::env::temp_dir();
        let factory = test_factory(
            root.clone(),
            FileCredentialStore::new(root.join("waltr-agent-invalid-definition-auth.json")),
        );
        let mut record = SessionRecord::new("invalid-definition");
        record.agent_definition = Some(crate::definition::AgentDefinitionBundle::new(
            "stored",
            vec!["not: [valid".to_owned()],
        ));

        let error = factory
            .definition_for_session(&record)
            .expect_err("present malformed definition must fail");

        assert!(matches!(
            error,
            AgentSessionRuntimeError::FailedPrecondition(_)
        ));
        assert!(
            error
                .to_string()
                .contains("stored session agent definition is invalid")
        );
    }

    #[test]
    fn bundle_less_uses_default_and_stored_bundle_is_independent_of_live_catalog() {
        let root = std::env::temp_dir();
        let source =
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: default }\nspec: {}\n";
        let catalog = AgentDefinitionCatalog::from_sources(vec![source.to_owned()], "default")
            .expect("catalog resolves");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("waltr-agent-catalog-auth.json")),
            catalog.resolved("default").unwrap().clone(),
        )
        .with_agent_catalog(catalog);

        assert_eq!(
            factory
                .definition_for_session(&SessionRecord::new("legacy"))
                .unwrap()
                .name,
            "default"
        );
        let mut missing = SessionRecord::new("missing");
        missing.agent_definition = Some(AgentDefinitionBundle::new(
            "removed",
            vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: removed }\nspec: {}\n".to_owned()],
        ));
        assert_eq!(
            factory
                .definition_for_session(&missing)
                .expect("stored bundle resolves from its own sources")
                .name,
            "removed"
        );
    }

    #[test]
    fn static_agent_catalog_still_drives_bundles_and_replaces_a_provider() {
        let source =
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: default }\nspec: {}\n";
        let catalog = AgentDefinitionCatalog::from_sources(vec![source.to_owned()], "default")
            .expect("catalog resolves");
        let provider_catalog = catalog.clone();
        let root = std::env::temp_dir();
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("waltr-agent-static-catalog-auth.json")),
            catalog.resolved("default").unwrap().clone(),
        )
        .with_agent_catalog_provider(move || provider_catalog.clone())
        .with_agent_catalog(catalog);

        let bundle = factory.bundle_for_agent("default").expect("bundle exists");
        assert_eq!(bundle.agent_name, "default");
        assert_eq!(bundle.sources, [source]);
        assert!(format!("{factory:?}").contains("catalog_provider: None"));
    }

    #[test]
    fn agent_catalog_provider_uses_later_catalog_without_changing_legacy_fallback() {
        let initial_source =
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: initial }\nspec: {}\n";
        let initial =
            AgentDefinitionCatalog::from_sources(vec![initial_source.to_owned()], "initial")
                .expect("initial catalog resolves");
        let later_source = format!(
            "{initial_source}---\napiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: later }}\nspec: {{ systemPrompt: later }}\n"
        );
        let later = AgentDefinitionCatalog::from_sources(vec![later_source.clone()], "later")
            .expect("later catalog resolves");
        let later_identity = later.bundle("later").unwrap().identity();
        let current = Arc::new(RwLock::new(initial.clone()));
        let root = std::env::temp_dir();
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("waltr-agent-dynamic-catalog-auth.json")),
            initial.resolved("initial").unwrap().clone(),
        )
        .with_agent_catalog_provider({
            let current = Arc::clone(&current);
            move || current.read().expect("catalog lock").clone()
        });

        assert!(factory.bundle_for_agent("later").is_err());
        *current.write().expect("catalog lock") = later;
        let bundle = factory
            .bundle_for_agent("later")
            .expect("later bundle exists");

        assert_eq!(bundle.sources, [later_source]);
        assert_eq!(bundle.identity(), later_identity);
        assert_eq!(factory.default_agent_name(), "initial");
        assert_eq!(
            factory
                .definition_for_session(&SessionRecord::new("legacy"))
                .expect("legacy fallback remains valid")
                .name,
            "initial"
        );
        let debug = format!("{factory:?}");
        assert!(debug.contains("catalog_provider: Some(\"<provider>\")"));
        assert!(!debug.contains("systemPrompt: later"));
    }

    #[tokio::test]
    async fn resolved_definition_drives_available_feature_inventory() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().to_path_buf();
        let definition = resolved_test_agent(
            r"spec:
  features: [{ featureSetRef: { name: features } }]
  permissions: [{ permissionSetRef: { name: permissions } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: features }
spec:
  filesystem: { root: $(session.workspace.root), profile: auto, defaultPermissions: true }
  shell: { root: $(session.workspace.root), defaultPermissions: true }
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: permissions }
spec:
  tools: [{ name: question, permission: allow }]",
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);
        let override_model = model.clone();
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("auth.json")),
            definition.clone(),
        )
        .with_runtime_provider(move |_| {
            Ok(AgentRuntimeProvider::new(
                override_model.clone(),
                Arc::new(EmptyProvider),
            ))
        });
        let settings = InferenceSettings {
            provider: InferenceProviderSettings {
                key: "unused".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: "gpt-5".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: None,
                reasoning_effort: None,
            },
        };
        let policy = factory.definition_permission_policy(&definition, &model, &[]);
        let runtime = factory
            .base_runtime_builder(&settings, &definition)
            .await
            .expect("shell-bearing definitions remain loadable")
            .with_permission_policy(policy)
            .build();
        let names = runtime
            .tools()
            .iter()
            .map(|tool| tool.definition().name.as_str())
            .collect::<BTreeSet<_>>();

        for kind in FilesystemConfig::new(&root).exposed_tools(&model).all() {
            assert!(names.contains(kind.name()), "missing {}", kind.name());
        }
        assert_eq!(names.contains("shell"), cfg!(feature = "shell"));
        assert!(names.contains("question"));
        assert!(!names.contains("task"));
        assert!(!names.contains("disabled_tool"));
    }

    #[test]
    fn empty_definition_omits_feature_owned_template_providers() {
        let root = tempfile::tempdir().expect("temp directory");
        let definition = empty_test_agent();
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.path().to_path_buf(),
            FileCredentialStore::new(root.path().join("auth.json")),
            definition.clone(),
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);

        let runtime = factory
            .apply_definition(
                AgentRuntimeBuilder::new(model, Arc::new(EmptyProvider)),
                &definition,
            )
            .build();

        assert_eq!(runtime.middleware_chain.len(), 3);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolved_coding_permissions_compile_for_concrete_tools_and_resources() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().to_path_buf();
        let definition = resolved_test_agent(
            r"spec:
  features: [{ featureSetRef: { name: features } }]
  permissions: [{ permissionSetRef: { name: permissions } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: features }
spec:
  filesystem: { root: $(session.workspace.root), profile: auto, defaultPermissions: true }
  skills: { root: $(session.workspace.root), defaultPermissions: true }
  shell: { root: $(session.workspace.root), defaultPermissions: true }
  task: { maxSubagentDepth: 1, defaultPermissions: true }
  mcp: { exposeTools: true, defaultPermissions: true }
---
apiVersion: waltr.dev/v1alpha1
kind: PermissionSet
metadata: { name: permissions }
spec:
  tools:
    - { name: question, permission: allow }
    - feature: filesystem
      permission: { resources: { paths: [{ match: { outsideRoot: $(session.workspace.root) }, effect: ask }] } }
    - name: shell
      permission:
        resources:
          paths: [{ match: { outsideRoot: $(session.workspace.root) }, effect: ask }]
          commands: [{ match: { names: [git, kubectl, ssh] }, effect: ask }]",
        );
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("auth.json")),
            definition.clone(),
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);
        let policy = factory.definition_permission_policy(
            &definition,
            &model,
            &["mcp_remote_lookup".to_owned()],
        );

        for name in FilesystemConfig::new(&root).exposed_tools(&model).all() {
            assert_eq!(policy.evaluate_tool(name.name()), PermissionLevel::Allow);
        }
        for name in ["question", "load_skill", "task", "mcp_remote_lookup"] {
            assert_eq!(policy.evaluate_tool(name), PermissionLevel::Allow, "{name}");
        }
        assert_eq!(
            policy.evaluate_tool("shell"),
            if cfg!(feature = "shell") {
                PermissionLevel::Allow
            } else {
                PermissionLevel::Deny
            }
        );
        assert_eq!(policy.evaluate_tool("disabled_tool"), PermissionLevel::Deny);

        let outside = crate::PermissionClaim::new(
            crate::PermissionOperation::Read,
            crate::PermissionResource::FilesystemPath(
                root.parent().expect("root parent").join("outside"),
            ),
            "outside workspace",
        );
        assert_eq!(
            policy.evaluate_claim("read", &outside),
            PermissionLevel::Ask
        );

        for command in ["git status", "kubectl get pods", "ssh host"] {
            let claim = crate::PermissionClaim::new(
                crate::PermissionOperation::Execute,
                crate::PermissionResource::ShellCommand(command.to_owned()),
                command,
            );
            assert_eq!(policy.evaluate_claim("shell", &claim), PermissionLevel::Ask);
        }
        let ordinary = crate::PermissionClaim::new(
            crate::PermissionOperation::Execute,
            crate::PermissionResource::ShellCommand("cargo test".to_owned()),
            "cargo test",
        );
        assert_eq!(
            policy.evaluate_claim("shell", &ordinary),
            if cfg!(feature = "shell") {
                PermissionLevel::Allow
            } else {
                PermissionLevel::Deny
            }
        );
    }

    #[test]
    fn synthetic_mcp_helpers_follow_enabled_features_and_explicit_permissions() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().to_path_buf();
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);
        for (flags, names, expected) in [
            (
                "resources: true, exposeTools: false, skills: false, defaultPermissions: true",
                vec!["read_mcp_resource"],
                PermissionLevel::Allow,
            ),
            (
                "resources: false, exposeTools: false, skills: true, defaultPermissions: true",
                vec!["load_skill", "read_mcp_resource"],
                PermissionLevel::Allow,
            ),
            (
                "resources: true, exposeTools: false, skills: true, defaultPermissions: false",
                vec!["load_skill", "read_mcp_resource"],
                PermissionLevel::Deny,
            ),
        ] {
            let definition =
                resolved_test_agent(&format!("spec:\n  features:\n    - mcp: {{ {flags} }}"));
            let factory = AgentSessionRuntimeFactory::new(
                crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
                root.clone(),
                FileCredentialStore::new(root.join("auth.json")),
                definition.clone(),
            );
            let names: Vec<String> = names.into_iter().map(str::to_owned).collect();
            let policy = factory.definition_permission_policy(&definition, &model, &names);
            for name in &names {
                assert_eq!(policy.evaluate_tool(name), expected);
            }
            assert_eq!(policy.evaluate_tool("not_enabled"), PermissionLevel::Deny);
        }
        let definition = resolved_test_agent(
            "spec:\n  features:\n    - mcp: { resources: false, exposeTools: false, skills: true, defaultPermissions: false }\n  permissions:\n    - { name: load_skill, permission: allow }\n    - { name: read_mcp_resource, permission: ask }",
        );
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("auth.json")),
            definition.clone(),
        );
        let policy = factory.definition_permission_policy(
            &definition,
            &model,
            &["load_skill".into(), "read_mcp_resource".into()],
        );
        assert_eq!(policy.evaluate_tool("load_skill"), PermissionLevel::Allow);
        assert_eq!(
            policy.evaluate_tool("read_mcp_resource"),
            PermissionLevel::Ask
        );
    }

    #[test]
    fn resolved_input_permissions_compile_for_concrete_mcp_tools() {
        let directory = tempfile::tempdir().expect("temp directory");
        let root = directory.path().to_path_buf();
        let definition = resolved_test_agent(
            r"spec:
  features:
    - mcp: { exposeTools: true }
  permissions:
    - feature: mcp
      permission:
        inputs:
          - match:
              - pointer: /action
                value: { wildcard: 'sessions.*' }
            effect: deny",
        );
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("auth.json")),
            definition.clone(),
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);
        let policy = factory.definition_permission_policy(
            &definition,
            &model,
            &["mcp_remote_lookup".to_owned()],
        );

        assert_eq!(
            policy.evaluate_tool("mcp_remote_lookup"),
            PermissionLevel::Deny
        );
        assert_eq!(
            policy
                .evaluate_input(
                    "mcp_remote_lookup",
                    &serde_json::json!({ "action": "sessions.read" }),
                )
                .expect("input rule matches")
                .effect
                .level,
            PermissionLevel::Deny
        );
    }

    struct StoredCredential;

    impl StoredCredential {
        #[allow(non_snake_case)]
        fn OAuth(credential: OAuthCredential) -> OAuthCredential {
            credential
        }

        #[allow(non_snake_case)]
        fn McpOAuth(credential: OAuthCredential) -> OAuthCredential {
            credential
        }
    }

    trait TestCredentialStoreExt {
        async fn modify<F>(&self, target_key: &str, update: F) -> Result<(), String>
        where
            F: FnOnce(Option<OAuthCredential>) -> Option<OAuthCredential>;
    }

    impl TestCredentialStoreExt for CredentialStore {
        async fn modify<F>(&self, target_key: &str, update: F) -> Result<(), String>
        where
            F: FnOnce(Option<OAuthCredential>) -> Option<OAuthCredential>,
        {
            match update(self.read(target_key).await?.map(|record| record.credential)) {
                Some(credential) => self.write(target_key, credential).await,
                None => self.delete(target_key).await,
            }
        }
    }

    impl TestCredentialStoreExt for FileCredentialStore {
        async fn modify<F>(&self, target_key: &str, update: F) -> Result<(), String>
        where
            F: FnOnce(Option<OAuthCredential>) -> Option<OAuthCredential>,
        {
            let current = OAuthCredentialStore::read(self, target_key.to_owned())
                .await?
                .map(|record| record.credential);
            match update(current) {
                Some(credential) => {
                    OAuthCredentialStore::write(self, target_key.to_owned(), credential).await
                }
                None => OAuthCredentialStore::delete(self, target_key.to_owned()).await,
            }
        }
    }

    fn make_provider_credential(
        access: &str,
        refresh: Option<&str>,
        expires_at_ms: Option<i64>,
        metadata: BTreeMap<String, String>,
    ) -> OAuthCredential {
        OAuthCredential::provider(
            OAuthTokenSet::bearer(access, refresh.map(ToOwned::to_owned), expires_at_ms),
            metadata,
        )
    }

    #[derive(Clone)]
    struct TokenServerState {
        requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
        request_count: Arc<AtomicUsize>,
        response: serde_json::Value,
        status: StatusCode,
        delay: Duration,
    }

    struct CurrentMcpStore {
        initial: OAuthCredentialRecord,
        current: OAuthCredentialRecord,
    }

    impl OAuthCredentialStore for CurrentMcpStore {
        fn read(
            &self,
            _target_key: String,
        ) -> OAuthCredentialStoreFuture<'_, Option<OAuthCredentialRecord>> {
            let initial = self.initial.clone();
            Box::pin(async move { Ok(Some(initial)) })
        }

        fn write(
            &self,
            _target_key: String,
            _credential: OAuthCredential,
        ) -> OAuthCredentialStoreFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn delete(&self, _target_key: String) -> OAuthCredentialStoreFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn claim_refresh(
            &self,
            _target_key: String,
            _claim: OAuthRefreshClaim,
        ) -> OAuthCredentialStoreFuture<'_, OAuthRefreshClaimResult> {
            let current = self.current.clone();
            Box::pin(async move { Ok(OAuthRefreshClaimResult::Current(Box::new(current))) })
        }

        fn commit_refresh(
            &self,
            _target_key: String,
            _claim: OAuthRefreshClaim,
            _credential: OAuthCredential,
        ) -> OAuthCredentialStoreFuture<'_, bool> {
            Box::pin(async { Ok(false) })
        }

        fn abort_refresh(
            &self,
            _target_key: String,
            _claim: OAuthRefreshClaim,
            _indeterminate: bool,
        ) -> OAuthCredentialStoreFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    async fn token_handler(
        State(state): State<TokenServerState>,
        axum::Form(form): axum::Form<BTreeMap<String, String>>,
    ) -> impl IntoResponse {
        state.request_count.fetch_add(1, Ordering::SeqCst);
        state.requests.lock().await.push(form);
        tokio::time::sleep(state.delay).await;
        (state.status, Json(state.response))
    }

    async fn token_server(
        response: serde_json::Value,
        delay: Duration,
    ) -> (String, TokenServerState) {
        token_server_with_status(StatusCode::OK, response, delay).await
    }

    async fn token_server_with_status(
        status: StatusCode,
        response: serde_json::Value,
        delay: Duration,
    ) -> (String, TokenServerState) {
        let state = TokenServerState {
            requests: Arc::new(Mutex::new(Vec::new())),
            request_count: Arc::new(AtomicUsize::new(0)),
            response,
            status,
            delay,
        };
        let app = Router::new()
            .route("/token", post(token_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind token server");
        let address = listener.local_addr().expect("token server address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve token endpoint");
        });
        (format!("http://{address}/token"), state)
    }

    async fn token_error_server(
        status: StatusCode,
        response: serde_json::Value,
    ) -> (String, TokenServerState) {
        token_server_with_status(status, response, Duration::ZERO).await
    }

    #[derive(Clone, Default)]
    struct MessagesServerState {
        requests: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>,
    }

    async fn messages_handler(
        State(state): State<MessagesServerState>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        state.requests.lock().await.push((headers, body));
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
    }

    async fn messages_server() -> (String, MessagesServerState) {
        let state = MessagesServerState::default();
        let app = Router::new()
            .route("/v1/messages", post(messages_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind messages server");
        let address = listener.local_addr().expect("messages server address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve messages endpoint");
        });
        (format!("http://{address}/v1/messages?beta=true"), state)
    }

    async fn malicious_token_server(oversized: bool) -> (String, Arc<AtomicUsize>) {
        let redirected_requests = Arc::new(AtomicUsize::new(0));
        let redirected = Arc::clone(&redirected_requests);
        let app = if oversized {
            Router::new().route(
                "/token",
                post(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        "x".repeat(64 * 1024 + 1),
                    )
                }),
            )
        } else {
            Router::new()
                .route(
                    "/token",
                    post(|| async {
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(header::LOCATION, "/secret-sink")],
                        )
                            .into_response()
                    }),
                )
                .route(
                    "/secret-sink",
                    post(move || {
                        let redirected = Arc::clone(&redirected);
                        async move {
                            redirected.fetch_add(1, Ordering::SeqCst);
                            Json(serde_json::json!({"access_token": "stolen"}))
                        }
                    }),
                )
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind malicious token server");
        let address = listener.local_addr().expect("token server address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve malicious token endpoint");
        });
        (format!("http://{address}/token"), redirected_requests)
    }

    fn mcp_state(token_url: String) -> OAuthCredential {
        OAuthCredential::mcp(
            OAuthTokenSet {
                access_token: oauth2::AccessToken::new("old-access".to_owned()),
                refresh_token: Some(oauth2::RefreshToken::new("old-refresh".to_owned())),
                token_type: "Bearer".to_owned(),
                scopes: vec![
                    oauth2::Scope::new("mcp:read".to_owned()),
                    oauth2::Scope::new("mcp:write".to_owned()),
                ],
                expires_at_ms: Some(now_ms() - 1),
            },
            McpOAuthContext {
                resource: "https://mcp.example/resource".to_owned(),
                issuer: "https://issuer.example".to_owned(),
                registration: McpOAuthRegistration {
                    method: McpOAuthRegistrationMethod::Dynamic,
                    client_id: "client-id".to_owned(),
                    redirect_uri: "https://waltr.example/outbound-oauth/callback".to_owned(),
                    token_endpoint_auth_method: "none".to_owned(),
                    created_at_ms: Some(10),
                    expires_at_ms: None,
                },
                endpoints: McpOAuthEndpoints {
                    authorization: "https://issuer.example/authorize".to_owned(),
                    token: token_url,
                    revocation: Some("https://issuer.example/revoke".to_owned()),
                },
            },
        )
    }

    fn mcp_server_settings() -> McpResolvedConnection {
        McpResolvedConnection {
            key: "remote".to_owned(),
            kind: McpConnectionKind::StreamableHttp,
            command: String::new(),
            url: "https://mcp.example/resource".to_owned(),
        }
    }

    #[tokio::test]
    async fn mcp_config_reports_exact_servers_configured_with_bearer_providers() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential.token.expires_at_ms = None;
        factory
            .credential_store
            .modify(&AuthTarget::mcp_server("remote").credential_key(), |_| {
                Some(StoredCredential::McpOAuth(credential))
            })
            .await
            .expect("store credential");
        let mut anonymous = mcp_server_settings();
        anonymous.key = "anonymous".to_owned();

        let (_, credentialed) = factory
            .mcp_config_with_credentials(&[mcp_server_settings(), anonymous], false)
            .await
            .expect("build MCP config");

        assert_eq!(credentialed, BTreeSet::from(["remote".to_owned()]));
    }

    #[tokio::test]
    async fn mcp_preparation_detects_bearer_capability_upgrade_without_fingerprint_change() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_mcp_connection_resolver(|_| Box::pin(async { Ok(vec![mcp_server_settings()]) }));
        let feature = mcp_feature(McpServerSelection::Exclude(BTreeSet::new()), true);
        let anonymous = factory
            .prepare_mcp_runtime(&feature)
            .await
            .expect("prepare anonymous MCP")
            .expect("MCP is selected");
        assert!(anonymous.credentialed_servers.is_empty());

        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential.token.expires_at_ms = None;
        factory
            .credential_store
            .modify(&AuthTarget::mcp_server("remote").credential_key(), |_| {
                Some(StoredCredential::McpOAuth(credential))
            })
            .await
            .expect("store credential");
        let authenticated = factory
            .prepare_mcp_runtime(&feature)
            .await
            .expect("prepare authenticated MCP")
            .expect("MCP is selected");

        assert_eq!(anonymous.fingerprint, authenticated.fingerprint);
        assert_eq!(
            authenticated.credentialed_servers,
            BTreeSet::from(["remote".to_owned()])
        );
    }

    #[tokio::test]
    async fn mcp_skills_fingerprint_changes_on_credential_replacement_logout_and_opt_out() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = test_factory(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
        )
        .with_mcp_connection_resolver(|_| Box::pin(async { Ok(vec![mcp_server_settings()]) }));
        let mut feature = mcp_feature(McpServerSelection::Exclude(BTreeSet::new()), true);
        feature.skills = true;
        let anonymous = factory
            .prepare_mcp_runtime(&feature)
            .await
            .unwrap()
            .unwrap()
            .fingerprint;
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state("https://issuer.example/token".into());
        credential.token.expires_at_ms = None;
        factory
            .credential_store
            .modify(&key, |_| {
                Some(StoredCredential::McpOAuth(credential.clone()))
            })
            .await
            .unwrap();
        let authenticated = factory
            .prepare_mcp_runtime(&feature)
            .await
            .unwrap()
            .unwrap()
            .fingerprint;
        assert_ne!(anonymous, authenticated);
        credential.token.access_token = oauth2::AccessToken::new("replacement-access".into());
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .unwrap();
        let replaced = factory
            .prepare_mcp_runtime(&feature)
            .await
            .unwrap()
            .unwrap()
            .fingerprint;
        assert_ne!(authenticated, replaced);
        assert!(!format!("{replaced:?}").contains("replacement-access"));
        factory
            .credential_store
            .modify(&key, |_| None)
            .await
            .unwrap();
        assert_eq!(
            anonymous,
            factory
                .prepare_mcp_runtime(&feature)
                .await
                .unwrap()
                .unwrap()
                .fingerprint
        );
        feature.skills = false;
        assert_ne!(
            anonymous,
            factory
                .prepare_mcp_runtime(&feature)
                .await
                .unwrap()
                .unwrap()
                .fingerprint
        );
    }

    fn provider_oauth_settings(token_url: String) -> SessionOAuthProviderSettings {
        SessionOAuthProviderSettings {
            token_url,
            client_id: "provider-client".to_owned(),
            redirect_uri: "http://localhost/callback".to_owned(),
            token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
            token_fields: Vec::new(),
            jwt_metadata: vec![SessionOAuthJwtMetadata {
                token: SessionOAuthJwtToken::Id,
                metadata_key: "account_id".to_owned(),
                claim_names: vec!["account.id".to_owned()],
                required: true,
            }],
        }
    }

    fn provider_credential(expires_at_ms: i64) -> OAuthCredential {
        make_provider_credential(
            "old-provider-access",
            Some("old-provider-refresh"),
            Some(expires_at_ms),
            BTreeMap::from([
                ("account_id".to_owned(), "old-account".to_owned()),
                ("preserved".to_owned(), "preserved-value".to_owned()),
            ]),
        )
    }

    fn anthropic_settings(
        key: &str,
        url: String,
        headers: BTreeMap<String, String>,
        oauth: Option<SessionOAuthProviderSettings>,
    ) -> InferenceSettings {
        InferenceSettings {
            provider: InferenceProviderSettings {
                key: key.to_owned(),
                api: ModelApi::AnthropicMessages,
                url,
                model_id: "claude-test".to_owned(),
                model_cost: None,
                headers,
                oauth,
                reasoning_effort: None,
            },
        }
    }

    fn jwt(claims: &serde_json::Value) -> String {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
        )
        .expect("encode JWT")
    }

    async fn provider_coordinator(
        token_url: String,
        credential: OAuthCredential,
    ) -> (
        CredentialRefreshCoordinator,
        FileCredentialStore,
        SessionOAuthProviderSettings,
    ) {
        let directory = tempfile::tempdir().expect("temp directory").keep();
        let store = FileCredentialStore::new(directory.join("auth.json"));
        store
            .modify(&AuthTarget::provider("test").credential_key(), |_| {
                Some(StoredCredential::OAuth(credential))
            })
            .await
            .expect("store provider credential");
        (
            CredentialRefreshCoordinator::new(store.clone()).with_loopback_test_mode(),
            store,
            provider_oauth_settings(token_url),
        )
    }

    #[tokio::test]
    #[allow(clippy::literal_string_with_formatting_args)]
    async fn runtime_builder_resolves_headers_at_agent_boundary() {
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            std::env::temp_dir(),
            FileCredentialStore::new(
                std::env::temp_dir().join("waltr-agent-runtime-test-auth.json"),
            ),
            empty_test_agent(),
        );
        let settings = InferenceSettings {
            provider: InferenceProviderSettings {
                key: "test".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: String::new(),
                model_id: "test-model".to_owned(),
                model_cost: None,
                headers: BTreeMap::from([(
                    "Authorization".to_owned(),
                    "Bearer ${env:WALTR_AGENT_TEST_MISSING_HEADER_TOKEN_UNSET}".to_owned(),
                )]),
                oauth: None,
                reasoning_effort: None,
            },
        };

        let error = factory
            .base_runtime_builder(&settings, &factory.definition)
            .await
            .expect_err("missing env should fail at runtime resolution");

        assert!(matches!(
            error,
            AgentSessionRuntimeError::FailedPrecondition(_)
        ));
        assert!(
            error
                .to_string()
                .contains("provider header 'Authorization' references unset env var")
        );
    }

    #[test]
    fn claude_code_version_accepts_one_safe_case_insensitive_user_agent() {
        assert_eq!(
            configured_claude_code_version(&BTreeMap::from([(
                "User-Agent".to_owned(),
                "claude-code/9.8.7".to_owned(),
            )]))
            .expect("single header is valid"),
            Some("9.8.7")
        );
        let duplicate = BTreeMap::from([
            ("User-Agent".to_owned(), "claude-code/9.8.7".to_owned()),
            ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
        ]);
        assert!(matches!(
            configured_claude_code_version(&duplicate),
            Err(AgentSessionRuntimeError::FailedPrecondition(_))
        ));
        assert_eq!(
            configured_claude_code_version(&BTreeMap::from([(
                "uSeR-aGeNt".to_owned(),
                "claude-code/3.4.5-beta_1+local".to_owned(),
            )]))
            .expect("single header is valid"),
            Some("3.4.5-beta_1+local")
        );
        for headers in [
            BTreeMap::new(),
            BTreeMap::from([("user-agent".to_owned(), "waltr/1.0".to_owned())]),
            BTreeMap::from([("user-agent".to_owned(), "claude-code/2.1;bad".to_owned())]),
        ] {
            assert_eq!(
                configured_claude_code_version(&headers).expect("header set is unambiguous"),
                None
            );
        }
    }

    #[tokio::test]
    async fn anthropic_oauth_runtime_sends_its_projected_billing_marker() {
        let directory = tempfile::tempdir().expect("temp directory");
        let (url, server) = messages_server().await;
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        factory
            .credential_store
            .modify(
                &AuthTarget::provider("anthropic-oauth").credential_key(),
                |_| {
                    Some(StoredCredential::OAuth(make_provider_credential(
                        "oauth-access",
                        Some("oauth-refresh"),
                        Some(now_ms() + 60 * 60 * 1000),
                        BTreeMap::new(),
                    )))
                },
            )
            .await
            .expect("store OAuth credential");
        let settings = anthropic_settings(
            "anthropic-oauth",
            url,
            BTreeMap::from([
                ("anthropic-beta".to_owned(), "oauth-2025-04-20".to_owned()),
                ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
                ("x-app".to_owned(), "cli".to_owned()),
            ]),
            Some(SessionOAuthProviderSettings {
                token_url: "https://example.test/token".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "http://localhost/callback".to_owned(),
                token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
                token_fields: Vec::new(),
                jwt_metadata: Vec::new(),
            }),
        );
        let runtime = factory
            .provider_builder(&settings)
            .await
            .expect("build OAuth provider")
            .build();
        let request = CompletionRequest::new("Hello from Claude Code");
        let projected = runtime
            .provider
            .project_request(&request)
            .expect("project request");

        let _stream = runtime
            .provider
            .stream_request(request)
            .await
            .expect("send request");
        let (headers, sent) = server
            .requests
            .lock()
            .await
            .first()
            .cloned()
            .expect("captured request");

        assert_eq!(sent, projected);
        assert_eq!(headers[header::AUTHORIZATION], "Bearer oauth-access");
        assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
        assert_eq!(headers[header::USER_AGENT], "claude-code/2.1.76");
        assert_eq!(headers["x-app"], "cli");
        assert_eq!(
            sent["system"][0]["text"],
            "x-anthropic-billing-header: cc_version=2.1.76.ad6; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[tokio::test]
    async fn anthropic_api_key_runtime_sends_projection_without_billing_marker() {
        let directory = tempfile::tempdir().expect("temp directory");
        let (url, server) = messages_server().await;
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        let settings = anthropic_settings(
            "anthropic-api-key",
            url,
            BTreeMap::from([
                ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
                ("x-api-key".to_owned(), "api-key-value".to_owned()),
            ]),
            Some(SessionOAuthProviderSettings {
                token_url: "https://example.test/token".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "http://localhost/callback".to_owned(),
                token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
                token_fields: Vec::new(),
                jwt_metadata: Vec::new(),
            }),
        );
        let runtime = factory
            .provider_builder(&settings)
            .await
            .expect("build API-key provider")
            .build();
        let request = CompletionRequest::new("Hello from Claude Code");
        let projected = runtime
            .provider
            .project_request(&request)
            .expect("project request");

        let _stream = runtime
            .provider
            .stream_request(request)
            .await
            .expect("send request");
        let (headers, sent) = server
            .requests
            .lock()
            .await
            .first()
            .cloned()
            .expect("captured request");

        assert_eq!(sent, projected);
        assert_eq!(headers["x-api-key"], "api-key-value");
        assert_eq!(headers[header::USER_AGENT], "claude-code/2.1.76");
        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(sent.get("system").is_none());
    }

    #[tokio::test]
    async fn anthropic_oauth_without_claude_code_user_agent_omits_billing_marker() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        factory
            .credential_store
            .modify(
                &AuthTarget::provider("anthropic-oauth").credential_key(),
                |_| {
                    Some(StoredCredential::OAuth(make_provider_credential(
                        "oauth-access",
                        Some("oauth-refresh"),
                        Some(now_ms() + 60 * 60 * 1000),
                        BTreeMap::new(),
                    )))
                },
            )
            .await
            .expect("store OAuth credential");
        let oauth = SessionOAuthProviderSettings {
            token_url: "https://example.test/token".to_owned(),
            client_id: "client".to_owned(),
            redirect_uri: "http://localhost/callback".to_owned(),
            token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
            token_fields: Vec::new(),
            jwt_metadata: Vec::new(),
        };

        for headers in [
            BTreeMap::new(),
            BTreeMap::from([("user-agent".to_owned(), "waltr/1.0".to_owned())]),
        ] {
            let runtime = factory
                .provider_builder(&anthropic_settings(
                    "anthropic-oauth",
                    "https://example.test/v1/messages".to_owned(),
                    headers,
                    Some(oauth.clone()),
                ))
                .await
                .expect("build OAuth provider")
                .build();
            let projected = runtime
                .provider
                .project_request(&CompletionRequest::new("Hello from Claude Code"))
                .expect("project request");

            assert!(projected.get("system").is_none());
        }

        let error = factory
            .provider_builder(&anthropic_settings(
                "anthropic-oauth",
                "https://example.test/v1/messages".to_owned(),
                BTreeMap::from([
                    ("User-Agent".to_owned(), "claude-code/9.8.7".to_owned()),
                    ("user-agent".to_owned(), "claude-code/2.1.76".to_owned()),
                ]),
                Some(oauth),
            ))
            .await
            .expect_err("ambiguous user-agent configuration must fail");
        assert!(matches!(
            error,
            AgentSessionRuntimeError::FailedPrecondition(_)
        ));
    }

    #[tokio::test]
    async fn provider_oauth_uses_namespaced_credential_key() {
        let auth_dir = std::env::temp_dir().join(format!(
            "waltr-agent-runtime-test-auth-{}",
            uuid::Uuid::new_v4()
        ));
        let auth_path = auth_dir.join("auth.json");
        let factory = test_factory(auth_dir, FileCredentialStore::new(auth_path));
        factory
            .credential_store
            .modify(&AuthTarget::provider("test").credential_key(), |_| {
                Some(StoredCredential::OAuth(make_provider_credential(
                    "access-token",
                    Some("refresh-token"),
                    Some(now_ms() + 60 * 60 * 1000),
                    BTreeMap::new(),
                )))
            })
            .await
            .expect("store credential");
        let provider = InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: BTreeMap::new(),
            oauth: Some(SessionOAuthProviderSettings {
                token_url: "https://example.test/token".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "http://localhost/callback".to_owned(),
                token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
                token_fields: Vec::new(),
                jwt_metadata: Vec::new(),
            }),
            reasoning_effort: None,
        };

        let credential = factory
            .oauth_access(&provider, &BTreeMap::new())
            .await
            .expect("oauth lookup succeeds")
            .expect("credential is found");

        assert_eq!(credential.token.access_token.secret(), "access-token");
    }

    #[tokio::test]
    async fn configured_provider_oauth_without_credential_fails_before_provider_construction() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        let settings = InferenceSettings {
            provider: InferenceProviderSettings {
                key: "configured-provider".to_owned(),
                api: ModelApi::OpenAiResponses,
                url: "https://provider.example/v1/responses".to_owned(),
                model_id: "test-model".to_owned(),
                model_cost: None,
                headers: BTreeMap::new(),
                oauth: Some(provider_oauth_settings(
                    "https://provider.example/oauth/token".to_owned(),
                )),
                reasoning_effort: None,
            },
        };

        let error = factory
            .base_runtime_builder(&settings, &factory.definition)
            .await
            .expect_err("missing configured OAuth credential must fail");

        assert!(matches!(
            error,
            AgentSessionRuntimeError::Authentication {
                target: AuthTarget::Provider { ref key },
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                ..
            } if key == "configured-provider"
        ));
    }

    #[test]
    fn provider_http_401_is_classified_for_exact_configured_target() {
        let error = provider_authentication_error(
            LlmError::Api {
                provider: "test",
                message: "secret response".to_owned(),
                status: Some(401),
                headers: Vec::new(),
                code: None,
                body: Some("secret response".to_owned()),
            },
            &AuthTarget::provider("configured-provider"),
        );

        assert!(matches!(
            error,
            LlmError::AuthenticationFailure {
                failure: AuthenticationFailure {
                    target: AuthTarget::Provider { ref key },
                    reason: crate::AuthenticationFailureReason::CredentialRejected,
                    ..
                },
                ref message,
            } if key == "configured-provider" && !message.contains("secret response")
        ));
    }

    #[test]
    fn provider_websocket_401_is_classified_for_exact_configured_target() {
        let response = http::Response::builder()
            .status(http::StatusCode::UNAUTHORIZED)
            .body(Some(Vec::new()))
            .expect("websocket handshake response");
        let error = provider_authentication_error(
            LlmError::WebSocket(tokio_tungstenite::tungstenite::Error::Http(Box::new(
                response,
            ))),
            &AuthTarget::provider("configured-websocket-provider"),
        );

        assert!(matches!(
            error,
            LlmError::AuthenticationFailure {
                failure: AuthenticationFailure {
                    target: AuthTarget::Provider { ref key },
                    reason: crate::AuthenticationFailureReason::CredentialRejected,
                    ..
                },
                ..
            } if key == "configured-websocket-provider"
        ));
    }

    #[tokio::test]
    async fn expired_provider_oauth_without_refresh_token_fails_before_unauthenticated_call() {
        let auth_dir = std::env::temp_dir().join(format!(
            "waltr-agent-runtime-test-auth-{}",
            uuid::Uuid::new_v4()
        ));
        let auth_path = auth_dir.join("auth.json");
        let factory = test_factory(auth_dir, FileCredentialStore::new(auth_path));
        factory
            .credential_store
            .modify(&AuthTarget::provider("test").credential_key(), |_| {
                Some(StoredCredential::OAuth(make_provider_credential(
                    "stale-token",
                    None,
                    Some(now_ms() - 1000),
                    BTreeMap::new(),
                )))
            })
            .await
            .expect("store credential");
        let provider = InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: BTreeMap::new(),
            oauth: Some(SessionOAuthProviderSettings {
                token_url: "https://example.test/token".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "http://localhost/callback".to_owned(),
                token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
                token_fields: Vec::new(),
                jwt_metadata: Vec::new(),
            }),
            reasoning_effort: None,
        };

        let error = factory
            .oauth_access(&provider, &BTreeMap::new())
            .await
            .expect_err("expired configured OAuth credential must fail");

        assert!(matches!(
            error,
            AgentSessionRuntimeError::Authentication {
                target: AuthTarget::Provider { ref key },
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                ..
            } if key == "test"
        ));
    }

    #[tokio::test]
    async fn forced_provider_refresh_preserves_and_reextracts_metadata_and_rotates_refresh() {
        let (token_url, server) = token_server(
            serde_json::json!({
                "access_token": "new-provider-access",
                "refresh_token": "rotated-provider-refresh",
                "expires_in": 3600,
                "id_token": jwt(&serde_json::json!({"account": {"id": "new-account"}}))
            }),
            Duration::ZERO,
        )
        .await;
        let (coordinator, store, settings) =
            provider_coordinator(token_url, provider_credential(now_ms() + 3_600_000)).await;

        let refreshed = coordinator
            .refresh_provider("test", &settings, RefreshMode::Forced)
            .await
            .expect("force provider refresh")
            .expect("provider credential");

        assert_eq!(refreshed.token.access_token.secret(), "new-provider-access");
        assert_eq!(
            refreshed
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("rotated-provider-refresh")
        );
        assert_eq!(
            refreshed
                .provider_metadata()
                .and_then(|metadata| metadata.get("account_id")),
            Some(&"new-account".to_owned())
        );
        assert_eq!(
            refreshed
                .provider_metadata()
                .and_then(|metadata| metadata.get("preserved")),
            Some(&"preserved-value".to_owned())
        );
        assert_eq!(server.request_count.load(Ordering::SeqCst), 1);
        let stored =
            OAuthCredentialStore::read(&store, AuthTarget::provider("test").credential_key())
                .await
                .expect("read provider credential")
                .expect("expected provider credential")
                .credential;
        assert_eq!(
            stored
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("rotated-provider-refresh")
        );
    }

    #[tokio::test]
    async fn normal_provider_refresh_retains_omitted_refresh_token_and_metadata() {
        let (token_url, _) = token_server(
            serde_json::json!({
                "access_token": "new-provider-access",
                "expires_in": 3600
            }),
            Duration::ZERO,
        )
        .await;
        let (coordinator, _, settings) =
            provider_coordinator(token_url, provider_credential(now_ms() + 1_000)).await;

        let refreshed = coordinator
            .refresh_provider("test", &settings, RefreshMode::Normal)
            .await
            .expect("normal provider refresh")
            .expect("provider credential");

        assert_eq!(
            refreshed
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("old-provider-refresh")
        );
        assert_eq!(
            refreshed
                .provider_metadata()
                .and_then(|metadata| metadata.get("account_id")),
            Some(&"old-account".to_owned())
        );
    }

    #[tokio::test]
    async fn provider_refresh_rejects_error_and_invalid_responses_without_leaking_body() {
        let (token_url, _) = token_error_server(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "invalid_grant", "detail": "private-response-body"}),
        )
        .await;
        let (coordinator, _, settings) =
            provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
        let error = coordinator
            .refresh_provider("test", &settings, RefreshMode::Normal)
            .await
            .expect_err("rejected refresh");
        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::TokenExchangeFailed
        );
        assert!(!format!("{error:?}").contains("private-response-body"));
        assert!(!error.to_string().contains("old-provider-refresh"));

        let (token_url, _) =
            token_server(serde_json::json!({"expires_in": 3600}), Duration::ZERO).await;
        let (coordinator, _, settings) =
            provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
        let error = coordinator
            .refresh_provider("test", &settings, RefreshMode::Normal)
            .await
            .expect_err("invalid refresh response");
        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::ResponseInvalid
        );
    }

    #[tokio::test]
    async fn independently_constructed_provider_stores_share_singleflight() {
        let (token_url, server) = token_server(
            serde_json::json!({
                "access_token": "new-provider-access",
                "expires_in": 3600
            }),
            Duration::from_millis(50),
        )
        .await;
        let (coordinator, store, settings) =
            provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
        let second = CredentialRefreshCoordinator::new(FileCredentialStore::new(store.path()))
            .with_loopback_test_mode();

        let (first, second) = tokio::join!(
            coordinator.refresh_provider("test", &settings, RefreshMode::Normal),
            second.refresh_provider("test", &settings, RefreshMode::Normal),
        );

        assert_eq!(
            first
                .expect("first refresh")
                .expect("credential")
                .token
                .access_token
                .secret(),
            "new-provider-access"
        );
        assert_eq!(
            second
                .expect("second refresh")
                .expect("credential")
                .token
                .access_token
                .secret(),
            "new-provider-access"
        );
        assert_eq!(server.request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_provider_refresh_cannot_replace_newer_credential() {
        let (token_url, server) = token_server(
            serde_json::json!({
                "access_token": "stale-refreshed-access",
                "expires_in": 3600
            }),
            Duration::from_millis(100),
        )
        .await;
        let (coordinator, store, settings) =
            provider_coordinator(token_url, provider_credential(now_ms() - 1)).await;
        let task = tokio::spawn({
            let coordinator = coordinator.clone();
            let settings = settings.clone();
            async move {
                coordinator
                    .refresh_provider("test", &settings, RefreshMode::Normal)
                    .await
            }
        });
        while server.request_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let mut replacement = provider_credential(now_ms() + 3_600_000);
        replacement.token.access_token =
            oauth2::AccessToken::new("newly-authorized-access".to_owned());
        replacement.token.refresh_token = Some(oauth2::RefreshToken::new(
            "newly-authorized-refresh".to_owned(),
        ));
        store
            .modify(&AuthTarget::provider("test").credential_key(), |_| {
                Some(StoredCredential::OAuth(replacement))
            })
            .await
            .expect("replace provider credential");

        let current = task
            .await
            .expect("refresh task")
            .expect("coordinated refresh")
            .expect("provider credential");
        assert_eq!(
            current.token.access_token.secret(),
            "newly-authorized-access"
        );
        assert_eq!(
            current
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("newly-authorized-refresh")
        );
    }

    #[tokio::test]
    async fn mcp_refresh_accepts_five_minute_token_and_preserves_rotation_metadata() {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "rotated-refresh",
                "token_type": "Bearer",
                "expires_in": 300
            }),
            Duration::ZERO,
        )
        .await;
        let expected = mcp_state(token_url);
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_outbound_oauth_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(expected.clone())))
            .await
            .expect("store credential");

        let access = factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect("refresh credential");

        assert_eq!(access.as_deref(), Some("new-access"));
        let actual = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
            .expect("expected typed MCP credential");
        let actual_credential = &actual.credential;
        assert_eq!(actual_credential.mcp_context(), expected.mcp_context());
        assert_eq!(
            actual_credential
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("rotated-refresh")
        );
        assert_eq!(
            actual_credential.token.token_type,
            expected.token.token_type
        );
        assert_eq!(actual_credential.token.scopes, expected.token.scopes);
        assert_eq!(actual.revision, 1);
        let requests = server_state.requests.lock().await;
        assert_eq!(
            requests[0].get("resource"),
            Some(&expected.mcp_context().expect("MCP context").resource)
        );
        drop(requests);
    }

    async fn assert_omitted_expiry_refreshes_once(expires_at_ms: i64) {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "new-access",
                "token_type": "Bearer"
            }),
            Duration::ZERO,
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_outbound_oauth_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state(token_url);
        credential.token.expires_at_ms = Some(expires_at_ms);
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .expect("store credential");

        for _ in 0..2 {
            let access = factory
                .mcp_bearer_token(&mcp_server_settings())
                .await
                .expect("resolve refreshed credential");
            assert_eq!(access.as_deref(), Some("new-access"));
        }

        assert_eq!(server_state.request_count.load(Ordering::SeqCst), 1);
        let actual = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
            .expect("expected typed MCP credential");
        assert_eq!(actual.credential.token.expires_at_ms, None);
        assert_eq!(
            actual
                .credential
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("old-refresh")
        );
        assert_eq!(actual.revision, 1);
    }

    #[tokio::test]
    async fn mcp_refresh_without_expires_in_replaces_expired_deadline() {
        assert_omitted_expiry_refreshes_once(now_ms() - 1).await;
    }

    #[tokio::test]
    async fn mcp_refresh_without_expires_in_replaces_refresh_ahead_deadline() {
        assert_omitted_expiry_refreshes_once(now_ms() + 60_000).await;
    }

    #[tokio::test]
    async fn concurrent_mcp_refresh_is_singleflight() {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "rotated-refresh",
                "token_type": "Bearer",
                "expires_in": 3600
            }),
            Duration::from_millis(50),
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_outbound_oauth_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        factory
            .credential_store
            .modify(&key, |_| {
                Some(StoredCredential::McpOAuth(mcp_state(token_url)))
            })
            .await
            .expect("store credential");
        let first = factory.clone();
        let second = factory.clone();
        let first_settings = mcp_server_settings();
        let second_settings = mcp_server_settings();

        let (first, second) = tokio::join!(
            first.mcp_bearer_token(&first_settings),
            second.mcp_bearer_token(&second_settings)
        );

        assert_eq!(first.expect("first refresh").as_deref(), Some("new-access"));
        assert_eq!(
            second.expect("second refresh").as_deref(),
            Some("new-access")
        );
        assert_eq!(server_state.request_count.load(Ordering::SeqCst), 1);
        let actual = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
            .expect("expected typed MCP credential");
        assert_eq!(actual.revision, 1);
    }

    #[tokio::test]
    async fn forced_mcp_refresh_across_independent_stores_uses_generation_claim_coordination() {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "forced-access",
                "refresh_token": "forced-rotated-refresh",
                "token_type": "Bearer",
                "expires_in": 3600
            }),
            Duration::from_millis(50),
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("auth.json");
        let store = FileCredentialStore::new(&path);
        let first = CredentialRefreshCoordinator::new(FileCredentialStore::new(&path))
            .with_loopback_test_mode();
        let second = CredentialRefreshCoordinator::new(FileCredentialStore::new(path))
            .with_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state(token_url);
        credential.token.expires_at_ms = Some(now_ms() + 3_600_000);
        store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .expect("store credential");
        let settings = mcp_server_settings();

        let (first, second) = tokio::join!(
            first.refresh_mcp(&settings, RefreshMode::Forced),
            second.refresh_mcp(&settings, RefreshMode::Forced),
        );

        assert_eq!(
            first
                .expect("first forced refresh")
                .expect("credential")
                .token
                .access_token
                .secret(),
            "forced-access"
        );
        assert_eq!(
            second
                .expect("second forced refresh")
                .expect("credential")
                .token
                .access_token
                .secret(),
            "forced-access"
        );
        assert_eq!(server_state.request_count.load(Ordering::SeqCst), 1);
        let current = OAuthCredentialStore::read(&store, key)
            .await
            .expect("read state")
            .expect("expected MCP OAuth state");
        assert_eq!(current.revision, 1);
        assert_eq!(
            current
                .credential
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("forced-rotated-refresh")
        );
    }

    #[tokio::test]
    async fn forced_mcp_refresh_rejects_expired_direct_exchange_result() {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "already-expired-access",
                "refresh_token": "rotated-refresh",
                "token_type": "Bearer",
                "expires_in": 0
            }),
            Duration::ZERO,
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let store = FileCredentialStore::new(directory.path().join("auth.json"));
        let coordinator =
            CredentialRefreshCoordinator::new(store.clone()).with_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut initial = mcp_state(token_url);
        initial.token.expires_at_ms = Some(now_ms() + 3_600_000);
        store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(initial)))
            .await
            .expect("store credential");

        let error = coordinator
            .refresh_mcp(&mcp_server_settings(), RefreshMode::Forced)
            .await
            .expect_err("expired refresh result must fail");

        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::ResponseInvalid
        );
        assert_eq!(server_state.request_count.load(Ordering::SeqCst), 1);
        let current = OAuthCredentialStore::read(&store, key)
            .await
            .expect("read state")
            .expect("expected retained MCP OAuth state");
        assert_eq!(current.revision, 0);
        assert_eq!(current.credential.token.access_token.secret(), "old-access");
    }

    #[tokio::test]
    async fn forced_mcp_refresh_rejects_expired_current_claim_result() {
        let mut initial = mcp_state("https://issuer.example/token".to_owned());
        initial.token.expires_at_ms = Some(now_ms() + 3_600_000);
        let mut current = initial.clone();
        current.token.access_token =
            oauth2::AccessToken::new("concurrent-expired-access".to_owned());
        current.token.expires_at_ms = Some(now_ms() - 1);
        let store = CredentialStore::new(CurrentMcpStore {
            initial: OAuthCredentialRecord {
                credential: initial,
                revision: 7,
                reauthorization_required: false,
            },
            current: OAuthCredentialRecord {
                credential: current,
                revision: 8,
                reauthorization_required: false,
            },
        });
        let coordinator = CredentialRefreshCoordinator::new(store);

        let error = coordinator
            .refresh_mcp(&mcp_server_settings(), RefreshMode::Forced)
            .await
            .expect_err("expired current result must fail");

        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::CredentialRequired
        );
        assert!(error.message().contains("usable credential"));
    }

    #[tokio::test]
    async fn normal_mcp_refresh_accepts_unexpired_current_claim_result() {
        let mut initial = mcp_state("https://issuer.example/token".to_owned());
        initial.token.expires_at_ms = Some(now_ms() + 60_000);
        let mut current = initial.clone();
        current.token.access_token =
            oauth2::AccessToken::new("concurrent-near-expiry-access".to_owned());
        let store = CredentialStore::new(CurrentMcpStore {
            initial: OAuthCredentialRecord {
                credential: initial,
                revision: 7,
                reauthorization_required: false,
            },
            current: OAuthCredentialRecord {
                credential: current,
                revision: 8,
                reauthorization_required: false,
            },
        });
        let coordinator = CredentialRefreshCoordinator::new(store);

        let refreshed = coordinator
            .refresh_mcp(&mcp_server_settings(), RefreshMode::Normal)
            .await
            .expect("near-expiry current result remains usable")
            .expect("MCP credential");

        assert_eq!(
            refreshed.token.access_token.secret(),
            "concurrent-near-expiry-access"
        );
    }

    #[tokio::test]
    async fn normal_mcp_refresh_accepts_unexpired_current_after_failed_cas() {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "stale-refreshed-access",
                "refresh_token": "stale-rotated-refresh",
                "token_type": "Bearer",
                "expires_in": 3600
            }),
            Duration::from_millis(100),
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let store = FileCredentialStore::new(directory.path().join("auth.json"));
        let coordinator =
            CredentialRefreshCoordinator::new(store.clone()).with_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        let initial = mcp_state(token_url.clone());
        store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(initial)))
            .await
            .expect("store initial credential");
        let task = tokio::spawn({
            let settings = mcp_server_settings();
            async move {
                coordinator
                    .refresh_mcp(&settings, RefreshMode::Normal)
                    .await
            }
        });
        while server_state.request_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let mut concurrent = mcp_state(token_url);
        concurrent.token.access_token =
            oauth2::AccessToken::new("concurrent-near-expiry-access".to_owned());
        concurrent.token.expires_at_ms = Some(now_ms() + 60_000);
        store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(concurrent)))
            .await
            .expect("store concurrent credential");

        let refreshed = task
            .await
            .expect("refresh task")
            .expect("near-expiry failed-CAS current remains usable")
            .expect("MCP credential");

        assert_eq!(
            refreshed.token.access_token.secret(),
            "concurrent-near-expiry-access"
        );
        let stored = OAuthCredentialStore::read(&store, key)
            .await
            .expect("read state")
            .expect("expected concurrent MCP OAuth state");
        assert_eq!(
            stored.credential.token.access_token.secret(),
            "concurrent-near-expiry-access"
        );
        assert_eq!(stored.revision, 1);
    }

    #[tokio::test]
    async fn invalid_grant_retains_secrets_and_blocks_replay() {
        let (token_url, server_state) = token_error_server(
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "invalid_grant",
                "error_description": "do not leak old-refresh"
            }),
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_outbound_oauth_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        factory
            .credential_store
            .modify(&key, |_| {
                Some(StoredCredential::McpOAuth(mcp_state(token_url)))
            })
            .await
            .expect("store credential");

        let error = factory
            .mcp_config_with_credentials(&[mcp_server_settings()], false)
            .await
            .expect_err("runtime MCP refresh rejection is an authentication failure");
        assert!(matches!(
            error,
            AgentSessionRuntimeError::Authentication {
                reason: crate::AuthenticationFailureReason::CredentialRejected,
                ..
            }
        ));
        assert!(!error.to_string().contains("old-refresh"));
        let current = factory
            .credential_store
            .read(&key)
            .await
            .expect("read retained credential")
            .expect("expected retained MCP OAuth state");
        assert_eq!(
            current
                .credential
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("old-refresh")
        );
        assert!(current.reauthorization_required);

        factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect_err("blocked refresh is an authentication failure");
        assert_eq!(server_state.request_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn indeterminate_server_failure_keeps_valid_access_and_refresh_tokens() {
        let (token_url, _) = token_error_server(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": "server_error"}),
        )
        .await;
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        )
        .with_outbound_oauth_loopback_test_mode();
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state(token_url);
        credential.token.expires_at_ms = Some(now_ms() + 60_000);
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .expect("store credential");

        let access = factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect("fall back to still-valid access token");
        assert_eq!(access.as_deref(), Some("old-access"));
        let current = factory
            .credential_store
            .read(&key)
            .await
            .expect("read retained credential")
            .expect("expected retained MCP OAuth state");
        assert_eq!(current.credential.token.access_token.secret(), "old-access");
        assert_eq!(
            current
                .credential
                .token
                .refresh_token
                .as_ref()
                .map(|token| token.secret().as_str()),
            Some("old-refresh")
        );
        assert_eq!(current.revision, 0);
        assert!(current.reauthorization_required);
    }

    #[tokio::test]
    async fn typed_mcp_credential_requires_exact_resource_and_issuer_binding() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential.mcp_context_mut().expect("MCP context").resource =
            "https://other.example/resource".to_owned();
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .expect("store mismatched credential");

        let error = factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect_err("resource mismatch must fail");
        assert!(matches!(
            error,
            AgentSessionRuntimeError::Authentication {
                reason: crate::AuthenticationFailureReason::ConfigurationRequired,
                ..
            }
        ));

        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential
            .mcp_context_mut()
            .expect("MCP context")
            .issuer
            .clear();
        assert!(validate_mcp_credential(&mcp_server_settings(), &credential).is_err());
        let context = credential.mcp_context_mut().expect("MCP context");
        context.issuer = "https://issuer.example".to_owned();
        context.registration.client_id.clear();
        assert!(validate_mcp_credential(&mcp_server_settings(), &credential).is_err());
    }

    #[tokio::test]
    async fn expired_mcp_credential_without_refresh_token_requires_authorization() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential.token.refresh_token = None;
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .expect("store expired credential");

        let error = factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect_err("expired credential is an authentication failure");
        assert!(matches!(
            error,
            AgentSessionRuntimeError::Authentication {
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn mcp_refresh_refuses_redirects_without_replaying_refresh_secret() {
        let (token_url, redirected_requests) = malicious_token_server(false).await;
        let error = oauth::refresh_mcp_oauth(
            &AuthTarget::mcp_server("remote"),
            &mcp_state(token_url),
            true,
            &[],
        )
        .await
        .expect_err("redirect must fail");

        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::ResponseInvalid
        );
        assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mcp_refresh_rejects_oversized_token_response() {
        let (token_url, _) = malicious_token_server(true).await;
        let error = oauth::refresh_mcp_oauth(
            &AuthTarget::mcp_server("remote"),
            &mcp_state(token_url),
            true,
            &[],
        )
        .await
        .expect_err("oversized response must fail");

        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::ResponseInvalid
        );
        assert!(error.message().contains("size limit"));
    }

    #[tokio::test]
    async fn refresh_ahead_does_not_make_mcp_token_expired() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            crate::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            empty_test_agent(),
        );
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential.token.refresh_token = None;
        credential.token.expires_at_ms = Some(now_ms() + 60_000);
        factory
            .credential_store
            .modify(&key, |_| Some(StoredCredential::McpOAuth(credential)))
            .await
            .expect("store credential");

        let access = factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect("valid access token");

        assert_eq!(access.as_deref(), Some("old-access"));
    }
}
