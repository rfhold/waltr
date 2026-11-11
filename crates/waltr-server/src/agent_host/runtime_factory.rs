use crate::auth::AuthTarget;
use crate::auth::refresh::{CredentialRefreshCoordinator, CredentialRefreshError, RefreshMode};
use std::{collections::BTreeSet, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use crate::agent_host::{
    AgentFeature, AgentPermissionSelector, AgentRuntime, AgentsPromptConfig,
    FilesystemPromptConfig, HostRuntimeBuilder, LlmError, LlmStream, McpFeatureConfig, ModelRef,
    PermissionEffect, PermissionLevel, PermissionPolicy, PermissionToolSelector, ResolvedAgent,
    ResolvedPermission, SkillsConfig,
    definition::{AgentDefinitionBundle, AgentDefinitionCatalog, McpServerSelection},
    filesystem::FilesystemConfig,
    mcp::{McpClientRuntime, McpConfig, McpServerConfig},
    question::QuestionConfig,
    tools::task::TaskConfig,
};
#[cfg(feature = "shell")]
use crate::agent_host::{ShellPromptConfig, shell::ShellConfig};
use mcp::{McpTracePropagation, client::McpBearerProviderError};

use super::{Capability, ToolDefinition};
use crate::auth::{CredentialStore, OAuthCredential};
use waltr_agent::{
    AgentRuntimeProvider, AgentSessionRuntimeError, InferenceSettings, SessionCoordinator,
    SessionRecord, SessionStore,
};
#[cfg(test)]
#[path = "runtime_factory/test_sandbox.rs"]
pub(super) mod test_sandbox;
use std::time::{SystemTime, UNIX_EPOCH};
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
const OAUTH_REFRESH_AHEAD_MS: i64 = 5 * 60 * 1000;

type AgentCatalogProvider = Arc<dyn Fn() -> AgentDefinitionCatalog + Send + Sync>;
type CodeToolProvider = Arc<
    dyn Fn(
            &SessionRecord,
            AgentRuntime,
            super::dispatcher::CapabilityDispatcher,
        ) -> Result<crate::agent_host::Capability, AgentSessionRuntimeError>
        + Send
        + Sync,
>;
type HostToolProvider = Arc<
    dyn Fn(
            &SessionRecord,
            AgentRuntime,
            super::dispatcher::CapabilityDispatcher,
        ) -> Result<Vec<crate::agent_host::Capability>, AgentSessionRuntimeError>
        + Send
        + Sync,
>;

struct BoundServerRuntimeFactory<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    factory: ServerAgentRuntimeFactory,
    sessions: SessionCoordinator<S>,
    built: std::sync::Mutex<BTreeSet<waltr_agent::SessionId>>,
    lifecycle: tokio::sync::RwLock<()>,
    owner: std::sync::Weak<BoundServerRuntimeFactory<S>>,
}

impl<S> waltr_agent::AgentHostRuntimeFactory for BoundServerRuntimeFactory<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn configuration_for_cycle(
        &self,
        record: &SessionRecord,
    ) -> waltr_agent::Result<Option<waltr_agent::HostConfigurationUpdate>> {
        let Some(configuration) = &record.agent_definition else {
            return Ok(None);
        };
        let mut bundle: AgentDefinitionBundle = serde_json::from_value(configuration.clone())?;
        let Some(audit) = bundle.pending_switch_audit.take() else {
            return Ok(None);
        };
        let already_appended = record.timeline_events().any(|event| matches!(&event.content,
            waltr_agent::TimelineContent::Json { value }
                if value.get("auditId").and_then(serde_json::Value::as_str) == Some(audit.id.as_str())));
        let timeline = if already_appended {
            Vec::new()
        } else {
            vec![waltr_agent::PendingTimelineEvent::new(
                record.id.clone(),
                record.snapshot().turn,
                waltr_agent::PendingTimelineItemId::New,
                waltr_agent::TimelineEventKind::ItemStarted,
                Some(waltr_agent::TimelineChannel::new("session_info")),
                waltr_agent::TimelineContent::Json {
                    value: serde_json::json!({
                        "kind": "agent_switch", "version": 1, "auditId": audit.id,
                        "oldAgentName": audit.old_name, "newAgentName": audit.new_name,
                    }),
                },
            )]
        };
        Ok(Some(waltr_agent::HostConfigurationUpdate {
            configuration: Some(serde_json::to_value(bundle)?),
            timeline,
        }))
    }
    fn build<'a>(&'a self, record: &'a SessionRecord) -> waltr_agent::HostRuntimeFuture<'a> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.read().await;
            self.built
                .lock()
                .expect("host runtime lifecycle lock poisoned")
                .insert(record.id.clone());
            let mcp = self
                .factory
                .mcp_registry
                .runtime(&self.factory, record, &self.sessions)
                .await
                .map_err(host_runtime_error)?;
            let host = self.owner.upgrade().ok_or_else(|| LlmError::Protocol {
                provider: "server-runtime",
                message: "host runtime owner retired".into(),
            })?;
            let mut runtime_factory = self
                .factory
                .providers
                .clone()
                .with_host_runtime_factory(host);
            if let (Some(events), Some(completion)) =
                (&self.factory.run_events, &self.factory.run_completion)
            {
                runtime_factory =
                    runtime_factory.with_run_coordination(events.clone(), completion.clone());
            }
            let sessions = self.sessions.clone().with_runtime_factory(runtime_factory);
            self.factory
                .runtime_for_session(record, sessions, mcp)
                .await
                .map_err(host_runtime_error)
        })
    }

    fn build_for_discovery<'a>(
        &'a self,
        record: &'a SessionRecord,
    ) -> waltr_agent::HostRuntimeFuture<'a> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.read().await;
            let current = self.sessions.load(&record.id).await?;
            if current.record.metadata.incarnation != record.metadata.incarnation
                || current.record.agent_definition != record.agent_definition
            {
                return Err(LlmError::Protocol {
                    provider: "server-runtime",
                    message: "session changed during discovery preparation".into(),
                });
            }
            let mcp = self
                .factory
                .mcp_registry
                .runtime(&self.factory, record, &self.sessions)
                .await
                .ok()
                .flatten();
            self.factory
                .runtime_with_dispatcher(record, self.sessions.clone(), mcp)
                .await
                .map(|(runtime, _)| runtime)
                .map_err(host_runtime_error)
        })
    }

    fn retire<'a>(
        &'a self,
        session: &'a waltr_agent::SessionId,
    ) -> waltr_agent::HostRuntimeRetireFuture<'a> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.write().await;
            let result = self.factory.mcp_registry.retire(session).await;
            self.factory.policy.retire_session(session);
            self.factory.session_roles.forget(session);
            self.built
                .lock()
                .expect("host runtime lifecycle lock poisoned")
                .remove(session);
            result
        })
    }

    fn retire_all(&self) -> waltr_agent::HostRuntimeRetireFuture<'_> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.write().await;
            let sessions = self
                .built
                .lock()
                .expect("host runtime lifecycle lock poisoned")
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            let mut failure = None;
            for session in sessions {
                self.factory.policy.retire_session(&session);
                self.factory.session_roles.forget(&session);
                if let Err(error) = self.factory.mcp_registry.retire(&session).await {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
                self.built
                    .lock()
                    .expect("host runtime lifecycle lock poisoned")
                    .remove(&session);
            }
            if let Err(error) = self.factory.mcp_registry.retire_all().await {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
            self.factory.session_roles.clear();
            failure.map_or(Ok(()), Err)
        })
    }
}

fn host_runtime_error(error: AgentSessionRuntimeError) -> LlmError {
    waltr_agent::AgentSessionRunError::from(error).into()
}

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

/// Agent runtime factory shared by sessions.
#[derive(Clone)]
pub struct ServerAgentRuntimeFactory {
    mcp_registry: Arc<super::mcp::registry::ServerMcpRegistry>,
    pub(crate) session_roles: super::session_roles::SessionRoles,
    /// Workspace root for tools and prompts.
    pub root: PathBuf,
    /// Required worker connection for workspace-dependent runtime behavior.
    pub sandbox: crate::agent_host::sandbox_gateway::SandboxHandle,
    /// Credential store used for provider auth.
    pub credential_store: CredentialStore,
    outbound_oauth_loopback_test_mode: bool,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    definition: ResolvedAgent,
    catalog: Option<AgentDefinitionCatalog>,
    catalog_provider: Option<AgentCatalogProvider>,
    providers: waltr_agent::AgentSessionRuntimeFactory,
    mcp_connection_resolver: Option<McpConnectionResolver>,
    code_tool_provider: Option<CodeToolProvider>,
    host_tool_provider: Option<HostToolProvider>,
    policy: Arc<dyn super::dispatcher::CapabilityPolicy>,
    restriction: Option<PermissionPolicy>,
    concrete_only: bool,
    self_mcp: Arc<std::sync::OnceLock<crate::self_mcp::SelfMcpBinding>>,
    pub(crate) run_events: Option<Arc<dyn crate::agent_host::SessionEventSink>>,
    pub(crate) run_completion: Option<Arc<dyn super::SessionCompletionCoordinationHandler>>,
}

impl std::fmt::Debug for ServerAgentRuntimeFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerAgentRuntimeFactory")
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
                "mcp_connection_resolver",
                &self.mcp_connection_resolver.as_ref().map(|_| "<resolver>"),
            )
            .finish_non_exhaustive()
    }
}

impl ServerAgentRuntimeFactory {
    /// Compose provider inference with this server-owned host runtime and lifecycle.
    pub fn session_factory<S>(
        &self,
        sessions: SessionCoordinator<S>,
    ) -> waltr_agent::AgentSessionRuntimeFactory
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let mut factory = self
            .providers
            .clone()
            .with_host_runtime_factory(self.clone().bind(sessions));
        if let (Some(events), Some(completion)) = (&self.run_events, &self.run_completion) {
            factory = factory.with_run_coordination(events.clone(), completion.clone());
        }
        factory
    }
    pub async fn switch_agent<S>(
        &self,
        sessions: &SessionCoordinator<S>,
        id: &waltr_agent::SessionId,
        name: &str,
    ) -> Result<waltr_agent::AgentSession, waltr_agent::AgentSessionRunError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let mut next = self.bundle_for_agent(name)?;
        Ok(sessions.update_host_configuration(id, |record| {
            let current = record.agent_definition.as_ref().map(|configuration|
                serde_json::from_value::<AgentDefinitionBundle>(configuration.clone())).transpose()?;
            let old = current.as_ref().map_or_else(|| self.default_agent_name().to_owned(),
                |bundle| bundle.agent_name.clone());
            let pending = current.as_ref().and_then(|bundle| bundle.pending_switch_audit.as_ref());
            let emitted = pending.is_some_and(|audit| record.timeline_events().any(|event|
                matches!(&event.content, waltr_agent::TimelineContent::Json { value }
                    if value.get("auditId").and_then(serde_json::Value::as_str) == Some(audit.id.as_str()))));
            let first_old = pending.filter(|_| !emitted).map_or(old, |audit| audit.old_name.clone());
            if first_old != name {
                next.pending_switch_audit = Some(super::AgentSwitchAudit {
                    id: uuid::Uuid::now_v7().to_string(), old_name: first_old, new_name: name.into(),
                });
            }
            Ok(Some(serde_json::to_value(next)?))
        }).await?)
    }
    /// Bind D4's server policy to every concrete capability entrypoint.
    pub fn with_capability_policy(
        mut self,
        policy: Arc<dyn super::dispatcher::CapabilityPolicy>,
    ) -> Self {
        self.policy = policy;
        self
    }
    pub fn with_permission_restriction(mut self, restriction: PermissionPolicy) -> Self {
        self.restriction = Some(match self.restriction.take() {
            Some(current) => current.intersection(restriction),
            None => restriction,
        });
        self
    }
    pub fn for_concrete_calls(mut self) -> Self {
        self.concrete_only = true;
        self
    }

    pub fn mcp_registry(&self) -> Arc<super::mcp::registry::ServerMcpRegistry> {
        self.mcp_registry.clone()
    }
    pub(crate) fn capability_policy(&self) -> Arc<dyn super::dispatcher::CapabilityPolicy> {
        self.policy.clone()
    }

    pub(crate) async fn external_dispatcher(
        &self,
        broker: crate::interaction::InteractionBroker,
        destination: crate::interaction::InteractionDestination,
    ) -> Result<super::dispatcher::CapabilityDispatcher, AgentSessionRuntimeError> {
        let _root = self.sandbox_root()?;
        let catalog = self.catalog_provider.as_ref().map(|provide| provide());
        let definition = catalog
            .as_ref()
            .and_then(|catalog| catalog.resolved(catalog.default_agent_name()))
            .unwrap_or(&self.definition);
        let policy =
            crate::permission::ServerPermissionPolicy::authenticated_external(broker, destination);
        let mut capabilities = Vec::new();
        if definition.features.filesystem.is_some() {
            use super::filesystem::FilesystemToolKind::*;
            for kind in [Read, Glob, Grep, Edit, Write, Delete, ApplyPatch] {
                capabilities.push(super::tools::sandbox::agent_tool(
                    kind.definition(),
                    self.sandbox.clone(),
                ));
            }
        }
        #[cfg(feature = "shell")]
        if definition.features.shell.is_some() {
            capabilities
                .push(ShellConfig::new(_root.clone()).into_agent_tool(self.sandbox.clone()));
        }
        // Public capabilities are host-owned only; upstream MCP belongs to session runtimes.
        super::dispatcher::CapabilityDispatcher::new(capabilities, None, policy)
            .map(|dispatcher| {
                dispatcher.with_context(
                    super::middleware::chain::MiddlewareChain::new(),
                    PermissionPolicy::new(PermissionLevel::Allow),
                )
            })
            .map_err(|error| AgentSessionRuntimeError::FailedPrecondition(error.to_string()))
    }

    pub(crate) fn install_self_mcp(&self, binding: crate::self_mcp::SelfMcpBinding) {
        assert!(
            self.self_mcp.set(binding).is_ok(),
            "self-MCP is initialized once"
        );
    }

    #[cfg(test)]
    pub(crate) fn self_mcp_test_binding(&self) -> Option<crate::self_mcp::SelfMcpBinding> {
        self.self_mcp.get().cloned()
    }

    /// Reconstructed/queued runs enter the same server-owned assembly path.
    pub fn bind<S>(
        self,
        sessions: SessionCoordinator<S>,
    ) -> Arc<dyn waltr_agent::AgentHostRuntimeFactory>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        Arc::new_cyclic(|owner| BoundServerRuntimeFactory {
            factory: self,
            sessions,
            built: Default::default(),
            owner: owner.clone(),
            lifecycle: tokio::sync::RwLock::new(()),
        })
    }
    /// Bind host callbacks to this exact run origin, including queued submissions.
    #[must_use]
    pub fn with_run_coordination(
        mut self,
        events: Arc<dyn crate::agent_host::SessionEventSink>,
        completion: Arc<dyn super::SessionCompletionCoordinationHandler>,
    ) -> Self {
        self.run_events = Some(events);
        self.run_completion = Some(completion);
        self
    }
    /// Install a host-owned code tool factory. Only explicitly opted-in definitions use it.
    /// The supplied runtime retains concrete tools, middleware and named permissions,
    /// but does not contain the code tool itself. The provider must bind current caller authority.
    #[must_use]
    pub fn with_code_tool_provider(
        mut self,
        provide: impl Fn(
            &SessionRecord,
            AgentRuntime,
            super::dispatcher::CapabilityDispatcher,
        ) -> Result<crate::agent_host::Capability, AgentSessionRuntimeError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.code_tool_provider = Some(Arc::new(provide));
        self
    }

    /// Add host tools after the normal definition, MCP, and permission assembly.
    /// Tools retain ordinary permission checks; this grants no permissions.
    #[must_use]
    pub fn with_host_tool_provider(
        mut self,
        provide: impl Fn(
            &SessionRecord,
            AgentRuntime,
            super::dispatcher::CapabilityDispatcher,
        )
            -> Result<Vec<crate::agent_host::Capability>, AgentSessionRuntimeError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.host_tool_provider = Some(Arc::new(provide));
        self
    }
    /// Create a runtime factory.
    #[must_use]
    pub fn new(
        sandbox: crate::agent_host::sandbox_gateway::SandboxHandle,
        root: PathBuf,
        credential_store: impl Into<CredentialStore>,
        definition: ResolvedAgent,
    ) -> Self {
        let credential_store = credential_store.into();
        let providers =
            waltr_agent::AgentSessionRuntimeFactory::new(credential_store.provider_store());
        Self {
            mcp_registry: Arc::new(super::mcp::registry::ServerMcpRegistry::default()),
            session_roles: super::session_roles::SessionRoles::default(),
            root,
            sandbox,
            credential_store,
            outbound_oauth_loopback_test_mode: false,
            trusted_mcp_oauth_origins: Vec::new(),
            definition,
            catalog: None,
            catalog_provider: None,
            providers,
            mcp_connection_resolver: None,
            code_tool_provider: None,
            host_tool_provider: None,
            policy: Arc::new(super::dispatcher::UnconfiguredPolicy),
            restriction: None,
            concrete_only: false,
            self_mcp: Arc::new(std::sync::OnceLock::new()),
            run_events: None,
            run_completion: None,
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
        self.providers = self.providers.with_runtime_provider(build);
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
    ) -> Result<HostRuntimeBuilder, AgentSessionRuntimeError> {
        if definition.features.code_mode == Some(true)
            && !self.concrete_only
            && self.code_tool_provider.is_none()
        {
            return Err(AgentSessionRuntimeError::FailedPrecondition(
                "code mode requires an explicit host tool provider".into(),
            ));
        }
        let builder = self.provider_builder(settings).await?;
        Ok(self.apply_definition(builder, definition))
    }

    /// Resolve the ordinary authenticated provider for a host-owned restricted helper.
    /// This grants no tools, definitions, or permission policy.
    ///
    /// # Errors
    /// Returns the normal provider configuration or credential resolution error.
    pub async fn provider_for_settings(
        &self,
        settings: &InferenceSettings,
    ) -> Result<Arc<dyn LlmStream>, AgentSessionRuntimeError> {
        self.providers.provider_for_settings(settings).await
    }

    fn apply_definition(
        &self,
        mut builder: HostRuntimeBuilder,
        definition: &ResolvedAgent,
    ) -> HostRuntimeBuilder {
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

    /// Project a pinned helper's policy without building providers or connecting MCP.
    /// Reserved host capture tools are not members of MCP feature tool sets.
    pub(crate) fn helper_capture_policy(
        &self,
        bundle: &AgentDefinitionBundle,
    ) -> Result<PermissionPolicy, AgentSessionRuntimeError> {
        let definition = bundle.resolve().map_err(|_| {
            AgentSessionRuntimeError::FailedPrecondition("invalid pinned helper definition".into())
        })?;
        let mut policy = PermissionPolicy::new(PermissionLevel::Deny);
        for entry in &definition.permissions {
            if !matches!(&entry.selector, AgentPermissionSelector::Name(name) if name == "_helper_capture") {
                continue;
            }
            policy = self.apply_definition_permission(policy, "_helper_capture".into(), &entry.permission);
        }
        Ok(match &self.restriction {
            Some(restriction) => policy.intersection(restriction.clone()),
            None => policy,
        })
    }

    fn definition_permission_policy(
        &self,
        definition: &ResolvedAgent,
        model: &ModelRef,
        mcp_tool_names: &[String],
    ) -> PermissionPolicy {
        let mut policy = PermissionPolicy::new(PermissionLevel::Deny);
        if definition.features.mcp.is_some() {
            policy = policy.with_tool_rule(
                PermissionToolSelector::exact("read_mcp_app_resource"),
                PermissionEffect::allow(),
            );
        }
        if definition.features.code_mode == Some(true) && !self.concrete_only {
            policy = policy.with_tool_rule(
                PermissionToolSelector::exact("code"),
                PermissionEffect::allow(),
            );
        }
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
                let names = feature_tools(feature);
                for name in names {
                    if feature == AgentFeature::Mcp
                        && definition
                            .features
                            .mcp
                            .as_ref()
                            .is_some_and(|mcp| !mcp.expose_tools)
                        && !matches!(
                            name.as_str(),
                            "read_mcp_app_resource"
                                | "read_mcp_resource"
                                | "get_mcp_prompt"
                                | "load_skill"
                        )
                    {
                        continue;
                    }
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
                policy = self.apply_definition_permission(policy, name, &entry.permission);
            }
        }
        policy
    }

    fn apply_definition_permission(&self, mut policy: PermissionPolicy, name: String, permission: &ResolvedPermission) -> PermissionPolicy {
                let selector = PermissionToolSelector::exact(name);
                match permission {
                    ResolvedPermission::Tool(effect) => {
                        policy = policy.with_tool_rule(selector, effect.clone());
                    }
                    ResolvedPermission::Resources(resources) => {
                        for rule in &resources.paths {
                            let pattern = match &rule.pattern {
                                crate::agent_host::PermissionPathPattern::OutsideRoot(_) => {
                                    crate::agent_host::PermissionPathPattern::OutsideRoot(
                                        self.root.clone(),
                                    )
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
            AgentFeature::Mcp if definition.features.mcp.as_ref().is_some() => {
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

    pub(crate) async fn runtime_for_session<S>(
        &self,
        record: &SessionRecord,
        sessions: SessionCoordinator<S>,
        mcp_runtime: Option<Arc<McpClientRuntime>>,
    ) -> Result<AgentRuntime, AgentSessionRuntimeError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        self.runtime_with_dispatcher(record, sessions, mcp_runtime)
            .await
            .map(|(runtime, _)| runtime)
    }

    /// Assemble a finished runtime from one durable session record.
    pub async fn runtime_for_record<S>(
        &self,
        record: &SessionRecord,
        sessions: SessionCoordinator<S>,
    ) -> Result<(AgentRuntime, super::dispatcher::CapabilityDispatcher), AgentSessionRuntimeError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let mcp = self.mcp_registry.runtime(self, record, &sessions).await?;
        self.runtime_with_dispatcher(record, sessions, mcp).await
    }

    pub(crate) async fn runtime_with_dispatcher<S>(
        &self,
        record: &SessionRecord,
        sessions: SessionCoordinator<S>,
        mcp_runtime: Option<Arc<McpClientRuntime>>,
    ) -> Result<(AgentRuntime, super::dispatcher::CapabilityDispatcher), AgentSessionRuntimeError>
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
            |value| {
                serde_json::from_value::<AgentDefinitionBundle>(value.clone())
                    .map(|bundle| bundle.identity())
                    .unwrap_or_default()
            },
        );
        let mut builder = factory.base_runtime_builder(settings, &definition).await?;
        builder = builder.with_system_prompt_identity(identity);
        if let Some(task) = definition.features.task.as_ref() {
            let captured_bundle = record.agent_definition.clone().or_else(|| {
                factory.catalog.as_ref().map(|catalog| {
                    serde_json::to_value(catalog.default_bundle()).expect("definition serializes")
                })
            });
            builder = builder.with_task_tool(
                TaskConfig::new(sessions)
                    .with_session_roles(factory.session_roles.clone(), record)
                    .with_agent_definition(captured_bundle)
                    .with_max_subagent_depth(task.max_subagent_depth),
            );
        }
        let mut mcp_tool_names = Vec::new();
        if let Some(mcp_runtime) = mcp_runtime
            && let Some(mcp) = definition.features.mcp.as_ref()
        {
            builder = builder.with_mcp_source_revision(mcp_runtime.dispatch_source_revision());
            mcp_tool_names.push("read_mcp_app_resource".to_owned());
            builder = builder.with_tool(Capability::new_async(ToolDefinition::new(
                "read_mcp_app_resource", "Read only the linked UI resource of a validated recorded execution.",
                serde_json::json!({"type":"object","properties":{"server":{"type":"string"},"uri":{"type":"string"}},"required":["server","uri"],"additionalProperties":false})),
                |context| Box::pin(async move { Ok(context.result("a validated recorded association is required").with_error()) }))
                .with_model_exposure(false));
            if mcp.skills {
                mcp_tool_names.extend(["load_skill".to_owned()]);
                builder = builder.with_mcp_skills(Arc::clone(&mcp_runtime));
            }
            {
                for tool in mcp_runtime.agent_tools(factory.policy.clone()).await {
                    mcp_tool_names.push(tool.definition().name.clone());
                    mcp_tool_names.push(tool.identity());

                    let model_visible = tool
                        .mcp_app_source
                        .as_ref()
                        .is_none_or(|source| source.ui.visible_to(mcp::apps::UiVisibility::Model));
                    builder = builder.with_tool(tool.with_model_exposure(
                        model_visible
                            && mcp.expose_tools
                            && definition.features.code_mode != Some(true),
                    ));
                }
            }
            if mcp.instructions {
                builder = builder.with_middleware(
                    crate::agent_host::middleware::mcp_instructions::McpInstructionsMiddleware::new(
                        Arc::clone(&mcp_runtime),
                    ),
                );
            }
            if mcp.resources || mcp.skills {
                mcp_tool_names.push("read_mcp_resource".to_owned());
                builder = builder.with_mcp_resources(Arc::clone(&mcp_runtime), mcp.resources);
            }
            if mcp.prompts {
                mcp_tool_names.push("get_mcp_prompt".to_owned());
                let prompt_runtime = mcp_runtime.clone();
                let prompt_policy = factory.policy.clone();
                builder = builder.with_tool(Capability::new_async(
                    ToolDefinition::new("get_mcp_prompt", "Read a configured MCP prompt", serde_json::json!({
                        "type":"object", "properties":{"server":{"type":"string"},"name":{"type":"string"},"arguments":{"type":"object"}},
                        "required":["server","name"],"additionalProperties":false})), move |context| {
                        let runtime = prompt_runtime.clone();
                        let policy = prompt_policy.clone();
                        Box::pin(async move {
                            #[derive(serde::Deserialize)]
                            #[serde(deny_unknown_fields)]
                            struct Selector { server: String, name: String, arguments: Option<serde_json::Value> }
                            let selector: Selector = serde_json::from_value(context.call().arguments.clone())?;
                            if !runtime.has_prompt(&selector.server, &selector.name).await {
                                return Ok(context.result("MCP prompt is unavailable").with_error());
                            }
                            let route = super::mcp::elicitation::McpOperation::for_context(policy.as_ref(), &context);
                            let content = runtime.get_prompt(&selector.server, &selector.name, selector.arguments,
                                context.cancellation_token().clone(), route).await?;
                            Ok(context.result(content))
                        })
                    }).with_model_exposure(false));
                builder = builder.with_middleware(
                    crate::agent_host::middleware::mcp_prompts::McpPromptsMiddleware::new(
                        mcp_runtime,
                    ),
                );
            }
        }
        let mut policy =
            factory.definition_permission_policy(&definition, builder.model(), &mcp_tool_names);
        if let Some(restriction) = &factory.restriction {
            policy = policy.intersection(restriction.clone());
        }
        let mut builder = builder
            .with_permission_policy(policy)
            .with_dispatch_context(Some(record.id.clone()), factory.policy.clone());
        if let Some(provide) = &factory.host_tool_provider {
            let (runtime, dispatcher) = builder.clone().build_with_dispatcher();
            for tool in provide(record, runtime, dispatcher)? {
                if builder
                    .clone()
                    .build()
                    .tools()
                    .iter()
                    .any(|existing| existing.definition().name == tool.definition().name)
                {
                    return Err(AgentSessionRuntimeError::FailedPrecondition(
                        "host tool name conflicts with a normal tool".into(),
                    ));
                }
                builder = builder.with_tool(tool);
            }
        }
        if definition.features.code_mode == Some(true) && !factory.concrete_only {
            let (runtime, dispatcher) = builder.clone().build_with_dispatcher();
            let provide = factory.code_tool_provider.as_ref().ok_or_else(|| {
                AgentSessionRuntimeError::FailedPrecondition(
                    "code mode requires an explicit host tool provider".into(),
                )
            })?;
            let tool = provide(record, runtime.clone(), dispatcher)?;
            if tool.definition().name != "code"
                || !tool.tool.requires_host_dispatch()
                || runtime
                    .tools()
                    .iter()
                    .any(|existing| existing.definition().name == tool.definition().name)
            {
                return Err(AgentSessionRuntimeError::FailedPrecondition(
                    "code tool requires a unique name and durable host dispatch".into(),
                ));
            }
            builder = builder.with_tool(tool);
        }
        Ok(builder.build_with_dispatcher())
    }

    #[cfg(test)]
    pub(crate) async fn permission_policy_for_record(
        &self,
        record: &SessionRecord,
        mcp_runtime: Option<&Arc<McpClientRuntime>>,
    ) -> Result<PermissionPolicy, AgentSessionRuntimeError> {
        let mut factory = self.clone();
        factory.root = self.sandbox_root()?;
        let settings = record.inference_settings.as_ref().ok_or_else(|| {
            AgentSessionRuntimeError::FailedPrecondition(
                "session runtime settings are required".into(),
            )
        })?;
        let definition = factory.definition_for_session(record)?;
        let builder = factory.provider_builder(settings).await?;
        let mut names = Vec::new();
        if let (Some(runtime), Some(mcp)) = (mcp_runtime, definition.features.mcp.as_ref()) {
            names.push("read_mcp_app_resource".into());
            if mcp.prompts {
                names.push("get_mcp_prompt".into());
            }
            if mcp.skills {
                names.push("load_skill".into());
            }
            for tool in runtime.agent_tools(factory.policy.clone()).await {
                names.push(tool.definition().name.clone());
                names.push(tool.identity());
            }
            if mcp.resources || mcp.skills {
                names.push("read_mcp_resource".into());
            }
        }
        let policy = factory.definition_permission_policy(&definition, builder.model(), &names);
        Ok(match &factory.restriction {
            Some(restriction) => policy.intersection(restriction.clone()),
            None => policy,
        })
    }

    fn definition_for_session(
        &self,
        record: &SessionRecord,
    ) -> Result<ResolvedAgent, AgentSessionRuntimeError> {
        let Some(bundle) = record.agent_definition.as_ref() else {
            return Ok(self.definition.clone());
        };
        let bundle: AgentDefinitionBundle = serde_json::from_value(bundle.clone())
            .map_err(|error| AgentSessionRuntimeError::FailedPrecondition(error.to_string()))?;
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
                    target: AuthTarget::mcp_server(&server.key).into(),
                    message: "MCP OAuth credential is required".to_owned(),
                    reason: crate::agent_host::AuthenticationFailureReason::CredentialRequired,
                    host_context: None,
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

    async fn provider_builder(
        &self,
        settings: &InferenceSettings,
    ) -> Result<HostRuntimeBuilder, AgentSessionRuntimeError> {
        Ok(HostRuntimeBuilder::from_provider(
            self.providers.provider_builder(settings).await?,
        ))
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
            if let Some(binding) = self
                .self_mcp
                .get()
                .filter(|binding| binding.matches(server))
            {
                let binding = binding.clone();
                let provider_binding = binding.clone();
                let factory = self.clone();
                let connection = server.clone();
                let provider = Arc::new(move || {
                    let binding = provider_binding.clone();
                    let factory = factory.clone();
                    let connection = connection.clone();
                    async move {
                        if !factory.mcp_connection_current(&connection).await {
                            return Err(McpBearerProviderError::Authorization);
                        }
                        Ok((binding.authority().to_owned(), binding.token()?))
                    }
                });
                let config = McpServerConfig::streamable_http_with_app_credentials(
                    server.key.clone(),
                    server.url.clone(),
                    binding.authority().to_owned(),
                    provider,
                )
                .with_private_endpoint(binding.endpoint().to_owned());
                credentialed_servers.insert(server.key.clone());
                mcp = mcp.with_server(self.pin_app_source(
                    config,
                    server,
                    Some(binding.authority().to_owned()),
                ));
                continue;
            }
            let mut expected_authority = None;
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
                        mcp = mcp.with_server(self.pin_app_source(
                            McpServerConfig::streamable_http(
                                server.key.clone(),
                                server.url.clone(),
                            ),
                            server,
                            None,
                        ));
                        continue;
                    }
                    let credential = self
                        .refresh_coordinator()
                        .refresh_mcp(server, RefreshMode::Normal)
                        .await
                        .map_err(|error| refresh_runtime_error(&error))?
                        .ok_or_else(|| {
                            AgentSessionRuntimeError::FailedPrecondition(
                                "MCP credential disappeared".into(),
                            )
                        })?;
                    let authority_id = credential.authority_id.clone();
                    expected_authority.clone_from(&authority_id);
                    credentialed_servers.insert(server.key.clone());
                    let factory = self.clone();
                    let server_settings = server.clone();
                    let provider = Arc::new(move || {
                        let factory = factory.clone();
                        let server = server_settings.clone();
                        async move {
                            let credential = factory
                                .refresh_coordinator()
                                .refresh_mcp(&server, RefreshMode::Normal)
                                .await
                                .map_err(|error| match refresh_runtime_error(&error) {
                                    AgentSessionRuntimeError::Authentication {
                                        reason: crate::agent_host::AuthenticationFailureReason::NetworkFailed,
                                        ..
                                    } => McpBearerProviderError::Unavailable,
                                    AgentSessionRuntimeError::Authentication { .. } => {
                                        McpBearerProviderError::Authorization
                                    }
                                    _ => McpBearerProviderError::Unavailable,
                                })?
                                .ok_or(McpBearerProviderError::Authorization)?;
                            Ok((
                                credential.authority_id.unwrap_or_default(),
                                credential.token.access_token.secret().to_owned(),
                            ))
                        }
                    });
                    match authority_id {
                        Some(id) if !id.is_empty() => {
                            McpServerConfig::streamable_http_with_app_credentials(
                                server.key.clone(),
                                server.url.clone(),
                                id,
                                provider,
                            )
                        }
                        _ => {
                            let bearer = Arc::new(move || {
                                let provider = provider.clone();
                                async move { provider().await.map(|(_, token)| token) }
                            });
                            McpServerConfig::streamable_http_with_bearer_provider(
                                server.key.clone(),
                                server.url.clone(),
                                bearer,
                            )
                        }
                    }
                }
            };
            mcp = mcp.with_server(self.pin_app_source(config, server, expected_authority));
        }
        Ok((mcp, credentialed_servers))
    }

    fn pin_app_source(
        &self,
        config: McpServerConfig,
        server: &McpResolvedConnection,
        expected_authority: Option<String>,
    ) -> McpServerConfig {
        let factory = self.clone();
        let server = server.clone();
        config.with_app_authority_validator(Arc::new(move || {
            let factory = factory.clone();
            let server = server.clone();
            let expected_authority = expected_authority.clone();
            Box::pin(async move {
                // Resolve identity without creating/reconfiguring the cached tool runtime.
                if !factory.mcp_connection_current(&server).await {
                    return false;
                }
                if let Some(binding) = factory
                    .self_mcp
                    .get()
                    .filter(|binding| binding.matches(&server))
                {
                    return binding.active()
                        && expected_authority.as_deref() == Some(binding.authority());
                }
                if server.kind == McpConnectionKind::Stdio {
                    return true;
                }
                let key = AuthTarget::mcp_server(&server.key).credential_key();
                match factory.credential_store.read(&key).await {
                    Ok(None) => expected_authority.is_none(),
                    Ok(Some(credential)) => {
                        expected_authority.is_some()
                            && credential.credential.authority_id == expected_authority
                    }
                    Err(_) => false,
                }
            })
        }))
    }

    async fn mcp_connection_current(&self, server: &McpResolvedConnection) -> bool {
        let Some(resolve) = &self.mcp_connection_resolver else {
            return true;
        };
        let Ok(current) = resolve(McpConnectionResolveRequest {
            servers: McpServerSelection::Include(BTreeSet::from([server.key.clone()])),
        })
        .await
        else {
            return false;
        };
        let matching = current
            .iter()
            .filter(|item| item.key == server.key)
            .collect::<Vec<_>>();
        matching.len() == 1
            && McpRuntimeFingerprintConnection::from_resolved(matching[0])
                == McpRuntimeFingerprintConnection::from_resolved(server)
    }

    #[allow(clippy::too_many_lines)]
    async fn mcp_bearer_token(
        &self,
        server: &McpResolvedConnection,
    ) -> Result<Option<String>, AgentSessionRuntimeError> {
        if let Some(binding) = self
            .self_mcp
            .get()
            .filter(|binding| binding.matches(server))
        {
            return if binding.active() && self.mcp_connection_current(server).await {
                Ok(binding.token().ok())
            } else {
                Ok(None)
            };
        }
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
        let coordinator = if self.outbound_oauth_loopback_test_mode {
            coordinator.with_loopback_test_mode()
        } else {
            coordinator
        };
        coordinator
    }
}

fn refresh_runtime_error(error: &CredentialRefreshError) -> AgentSessionRuntimeError {
    if error.is_internal() {
        AgentSessionRuntimeError::Internal(error.message().to_owned())
    } else {
        AgentSessionRuntimeError::Authentication {
            target: error.target().clone().into(),
            message: error.message().to_owned(),
            reason: error.reason(),
            host_context: None,
        }
    }
}

pub(crate) fn should_refresh_mcp_token(credential: &OAuthCredential) -> bool {
    credential
        .token
        .expires_by(now_ms().saturating_add(OAUTH_REFRESH_AHEAD_MS))
}

pub(crate) fn validate_mcp_access_token(
    server_key: &str,
    credential: &OAuthCredential,
) -> Result<(), AgentSessionRuntimeError> {
    if credential.token.access_token.secret().is_empty()
        || !credential.token.token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(AgentSessionRuntimeError::Authentication {
            target: AuthTarget::mcp_server(server_key).into(),
            message: "MCP OAuth credential does not contain a supported bearer access token"
                .to_owned(),
            reason: crate::agent_host::AuthenticationFailureReason::ResponseInvalid,
            host_context: None,
        });
    }
    Ok(())
}

pub(crate) fn validate_mcp_credential(
    server: &McpResolvedConnection,
    credential: &OAuthCredential,
) -> Result<(), AgentSessionRuntimeError> {
    let Some(context) = credential.mcp_context() else {
        return Err(mcp_authorization_error(
            &server.key,
            "MCP OAuth credential binding is invalid",
            crate::agent_host::AuthenticationFailureReason::ConfigurationRequired,
        ));
    };
    let registration_current = context
        .registration
        .expires_at_ms
        .is_none_or(|expires_at_ms| expires_at_ms > now_ms());
    let valid = credential.version == crate::auth::OAUTH_CREDENTIAL_VERSION
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
            crate::agent_host::AuthenticationFailureReason::ConfigurationRequired,
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
    reason: crate::agent_host::AuthenticationFailureReason,
) -> AgentSessionRuntimeError {
    AgentSessionRuntimeError::Authentication {
        target: AuthTarget::mcp_server(server_key).into(),
        message: message.into(),
        reason,
        host_context: None,
    }
}

fn feature_default_permissions(
    features: &crate::agent_host::ResolvedAgentFeatures,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::refresh as oauth;
    use crate::auth::{
        FileCredentialStore, McpOAuthContext, McpOAuthEndpoints, McpOAuthRegistration,
        McpOAuthRegistrationMethod, OAuthCredentialRecord, OAuthCredentialStore,
        OAuthCredentialStoreFuture, OAuthRefreshClaim, OAuthRefreshClaimResult, OAuthTokenSet,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc, RwLock,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use waltr_agent::{
        BoxCompletionStream, BoxStreamFuture, CompletionRequest, InferenceProviderSettings,
        MemorySessionStore, ModelApi, ProviderId, SessionCreateRequest,
    };

    use axum::{
        Json, Router,
        extract::State,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use tokio::sync::Mutex;

    fn resolved_test_agent(resources: &str) -> ResolvedAgent {
        let mut registry = crate::agent_host::AgentDefinitionRegistry::new();
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

    #[test]
    fn helper_capture_projection_matches_runtime_policy_without_a_model() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let model = ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses);
        for permissions in [
            "[]",
            "[{name: _helper_capture, permission: allow}]",
            "[{name: _helper_capture, permission: ask}]",
            "[{name: _helper_capture, permission: deny}]",
            "[{name: _helper_capture, permission: allow}, {name: _helper_capture, permission: deny}]",
        ] {
            let source = format!("apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{name: test}}\nspec:\n  features: [{{shell: {{root: $(session.workspace.root), defaultPermissions: true}}}}]\n  permissions: {permissions}\n");
            let bundle = AgentDefinitionBundle::new("test", vec![source]);
            let definition = bundle.resolve().unwrap();
            for restriction in [PermissionLevel::Allow, PermissionLevel::Ask, PermissionLevel::Deny] {
                let restriction = PermissionPolicy::new(restriction);
                let factory = ServerAgentRuntimeFactory::new(
                    crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
                    root.clone(), FileCredentialStore::new(root.join("auth.json")), definition.clone(),
                ).with_permission_restriction(restriction.clone());
                let expected = factory.definition_permission_policy(&definition, &model, &[]).intersection(restriction);
                let projected = factory.helper_capture_policy(&bundle).unwrap();
                assert_eq!(projected.evaluate_tool("_helper_capture"), expected.evaluate_tool("_helper_capture"));
                assert_eq!(projected.evaluate_tool("shell"), PermissionLevel::Deny);
            }
        }
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
    ) -> ServerAgentRuntimeFactory {
        ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root,
            credential_store,
            empty_test_agent(),
        )
    }

    #[tokio::test]
    async fn internal_mcp_catalog_retains_self_and_remote_resources() {
        let root = tempfile::tempdir().unwrap();
        let factory = test_factory(
            root.path().into(),
            FileCredentialStore::new(root.path().join("auth.json")),
        )
        .with_mcp_connection_resolver(|_| {
            Box::pin(async {
                Ok(vec![
                    McpResolvedConnection {
                        key: "self-alias".into(),
                        kind: McpConnectionKind::StreamableHttp,
                        url: "http://127.0.0.1:1234/mcp".into(),
                        command: String::new(),
                    },
                    McpResolvedConnection {
                        key: "waltr".into(),
                        kind: McpConnectionKind::StreamableHttp,
                        url: "https://remote.example/mcp".into(),
                        command: String::new(),
                    },
                ])
            })
        });
        for selection in [
            McpServerSelection::default(),
            McpServerSelection::Include(BTreeSet::from(["self-alias".into(), "waltr".into()])),
        ] {
            let feature = mcp_feature(selection, false);
            assert_eq!(
                factory
                    .prepare_mcp_runtime(&feature)
                    .await
                    .unwrap()
                    .unwrap()
                    .connections
                    .len(),
                2
            );
        }
        let missing = mcp_feature(
            McpServerSelection::Include(BTreeSet::from(["missing".into()])),
            false,
        );
        assert!(factory.prepare_mcp_runtime(&missing).await.is_err());
    }

    #[tokio::test]
    async fn runtime_for_session_rejects_unavailable_worker_root() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = test_factory(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
        );
        let record = SessionCoordinator::new(MemorySessionStore::new())
            .create(SessionCreateRequest::new("worker-unavailable"))
            .await
            .unwrap()
            .record;
        let error = factory
            .runtime_for_session(
                &record,
                SessionCoordinator::new(crate::agent_host::MemorySessionStore::new()),
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
        let factory = ServerAgentRuntimeFactory::new(
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

    #[tokio::test]
    async fn code_mode_requires_opt_in_and_an_explicit_host_provider() {
        let directory = tempfile::tempdir().unwrap();
        let sandbox = test_sandbox::worker_handle(directory.path()).await;
        for flag in [None, Some(false), Some(true)] {
            let definition = resolved_test_agent(&flag.map_or_else(
                || "spec: {}".into(),
                |flag| format!("spec:\n  features: [{{ codeMode: {flag} }}]"),
            ));
            let factory = ServerAgentRuntimeFactory::new(
                sandbox.clone(),
                directory.path().into(),
                FileCredentialStore::new(directory.path().join("unused-auth.json")),
                definition,
            )
            .with_runtime_provider(|_| {
                Ok(AgentRuntimeProvider::new(
                    ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
                    Arc::new(EmptyProvider),
                ))
            });
            let mut request = SessionCreateRequest::new("code-opt-in");
            request.inference_settings = Some(InferenceSettings {
                provider: InferenceProviderSettings {
                    key: "test".into(),
                    api: ModelApi::OpenAiResponses,
                    url: String::new(),
                    model_id: "test".into(),
                    model_cost: None,
                    headers: BTreeMap::new(),
                    oauth: None,
                    reasoning_effort: None,
                },
            });
            let record = SessionCoordinator::new(MemorySessionStore::new())
                .create(request)
                .await
                .unwrap()
                .record;
            let sessions = SessionCoordinator::new(crate::agent_host::MemorySessionStore::new());
            let without = factory
                .runtime_for_session(&record, sessions.clone(), None)
                .await;
            assert_eq!(without.is_err(), flag == Some(true));
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = calls.clone();
            let factory = factory.with_code_tool_provider(move |_, _, _| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(crate::agent_host::Capability::new(
                    crate::agent_host::ToolDefinition::new("code", "host", serde_json::json!({})),
                    |_| panic!("unused"),
                )
                .with_host_dispatch(|_| {
                    Ok(crate::agent_host::HostDispatchBinding {
                        invocation_id: "reserved".into(),
                        source_revision: "revision".into(),
                    })
                }))
            });
            let runtime = factory
                .runtime_for_session(&record, sessions, None)
                .await
                .unwrap();
            assert_eq!(
                runtime
                    .tools()
                    .iter()
                    .any(|tool| tool.definition().name == "code"),
                flag == Some(true)
            );
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(flag == Some(true))
            );
            assert_eq!(
                factory
                    .permission_policy_for_record(&record, None)
                    .await
                    .unwrap()
                    .evaluate_tool("unconfigured-concrete-target"),
                PermissionLevel::Deny
            );
        }
    }

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
        let factory = ServerAgentRuntimeFactory::new(
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
        let record = SessionCoordinator::new(MemorySessionStore::new())
            .create(request)
            .await
            .unwrap()
            .record;
        let runtime = factory
            .runtime_for_session(
                &record,
                SessionCoordinator::new(crate::agent_host::MemorySessionStore::new()),
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
            factory
                .permission_policy_for_record(&record, None)
                .await
                .unwrap()
                .evaluate_tool("read"),
            PermissionLevel::Allow
        );
        assert!(
            runtime
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "apply_patch")
        );
        assert_eq!(
            factory
                .permission_policy_for_record(&record, None)
                .await
                .unwrap()
                .evaluate_tool("apply_patch"),
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
                factory
                    .permission_policy_for_record(&record, None)
                    .await
                    .unwrap()
                    .evaluate_tool(excluded),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.clone(),
            FileCredentialStore::new(root.join("waltr-agent-definition-merge-auth.json")),
            current.clone(),
        );
        let mut record = SessionRecord::new("definition-merge");
        record.agent_definition = Some(
            serde_json::to_value(crate::agent_host::definition::AgentDefinitionBundle::new(
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
            ))
            .unwrap(),
        );

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
        record.agent_definition = Some(
            serde_json::to_value(crate::agent_host::definition::AgentDefinitionBundle::new(
                "stored",
                vec!["not: [valid".to_owned()],
            ))
            .unwrap(),
        );

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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        missing.agent_definition = Some(serde_json::to_value(AgentDefinitionBundle::new(
            "removed",
            vec!["apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: { name: removed }\nspec: {}\n".to_owned()],
        )).unwrap());
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            root.path().to_path_buf(),
            FileCredentialStore::new(root.path().join("auth.json")),
            definition.clone(),
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);

        let runtime = factory
            .apply_definition(
                HostRuntimeBuilder::new(model, Arc::new(EmptyProvider)),
                &definition,
            )
            .build();

        assert!(runtime.tools().iter().all(|tool| !tool.is_model_exposed()));
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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

        let outside = crate::agent_host::PermissionClaim::new(
            crate::agent_host::PermissionOperation::Read,
            crate::agent_host::PermissionResource::FilesystemPath(
                root.parent().expect("root parent").join("outside"),
            ),
            "outside workspace",
        );
        assert_eq!(
            policy.evaluate_claim("read", &outside),
            PermissionLevel::Ask
        );

        for command in ["git status", "kubectl get pods", "ssh host"] {
            let claim = crate::agent_host::PermissionClaim::new(
                crate::agent_host::PermissionOperation::Execute,
                crate::agent_host::PermissionResource::ShellCommand(command.to_owned()),
                command,
            );
            assert_eq!(policy.evaluate_claim("shell", &claim), PermissionLevel::Ask);
        }
        let ordinary = crate::agent_host::PermissionClaim::new(
            crate::agent_host::PermissionOperation::Execute,
            crate::agent_host::PermissionResource::ShellCommand("cargo test".to_owned()),
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
            let factory = ServerAgentRuntimeFactory::new(
                crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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

    #[test]
    fn hidden_mcp_tools_require_explicit_authority_and_prompt_features_keep_their_grants() {
        let directory = tempfile::tempdir().unwrap();
        let definition = resolved_test_agent(
            "spec:\n  features:\n    - mcp: { exposeTools: false, prompts: true, defaultPermissions: true }",
        );
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
            definition.clone(),
        );
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5", ModelApi::OpenAiResponses);
        let identity = super::super::authorized_tools::ConcreteToolTarget::new("remote", "lookup")
            .unwrap()
            .permission_name();
        let policy = factory.definition_permission_policy(
            &definition,
            &model,
            &[
                "remote__lookup".into(),
                identity.clone(),
                "get_mcp_prompt".into(),
            ],
        );
        assert_eq!(
            policy
                .evaluate_target_tool_effect(&identity, "remote__lookup")
                .level,
            PermissionLevel::Deny
        );
        assert_eq!(
            policy.evaluate_tool("get_mcp_prompt"),
            PermissionLevel::Allow
        );
        let explicit = resolved_test_agent(
            "spec:\n  features:\n    - mcp: { exposeTools: false, defaultPermissions: true }\n  permissions:\n    - feature: mcp\n      permission: allow",
        );
        let policy = factory.definition_permission_policy(
            &explicit,
            &model,
            &["remote__lookup".into(), identity.clone()],
        );
        assert_eq!(
            policy
                .evaluate_target_tool_effect(&identity, "remote__lookup")
                .level,
            PermissionLevel::Allow
        );
    }

    struct StoredCredential;

    impl StoredCredential {
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        assert_eq!(actual_credential.authority_id, expected.authority_id);
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
            crate::agent_host::AuthenticationFailureReason::ResponseInvalid
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
            crate::agent_host::AuthenticationFailureReason::CredentialRequired
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
                reason: crate::agent_host::AuthenticationFailureReason::CredentialRejected,
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
                reason: crate::agent_host::AuthenticationFailureReason::ConfigurationRequired,
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
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
                reason: crate::agent_host::AuthenticationFailureReason::CredentialRequired,
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
            crate::agent_host::AuthenticationFailureReason::ResponseInvalid
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
            crate::agent_host::AuthenticationFailureReason::ResponseInvalid
        );
        assert!(error.message().contains("size limit"));
    }

    #[tokio::test]
    async fn refresh_ahead_does_not_make_mcp_token_expired() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = ServerAgentRuntimeFactory::new(
            crate::agent_host::sandbox_gateway::SandboxGateway::new("test".into()).handle(),
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
