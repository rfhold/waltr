use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use crate::{
    AgentRuntimeBuilder, AgentsPromptConfig, AuthTarget, FilesystemPromptConfig, ModelApi,
    ModelCost, ModelRef, PermissionEffect, PermissionPolicy, ProviderId, SkillsConfig,
    filesystem::FilesystemConfig,
    mcp::{McpClientRuntime, McpConfig, McpServerConfig},
    provider::{
        AnthropicMessages, OpenAiChatCompletions, OpenAiReasoningConfig, OpenAiReasoningEffort,
        OpenAiReasoningSummary, OpenAiResponses, OpenAiResponsesWebSocket,
    },
    question::QuestionConfig,
    tools::task::{DEFAULT_MAX_SUBAGENT_DEPTH, TaskConfig},
};
#[cfg(feature = "shell")]
use crate::{ShellPromptConfig, shell::ShellConfig};
use mcp::McpTracePropagation;

mod headers;
mod oauth;

use headers::{configured_credential, is_credential_header, resolve_provider_headers};
use oauth::{now_ms, refresh_oauth};

use super::{
    AgentSessionRuntimeError, FileCredentialStore, OAuthCredential, SessionCoordinator,
    SessionMcpServerRuntimeKind, SessionOAuthProviderSettings, SessionOAuthTokenRequestStyle,
    SessionProviderRuntimeSettings, SessionRuntimeSettings, SessionStore, StoredCredential,
};

const ENV_PREFIX: &str = "${env:";

type RuntimeBuilderOverride = Arc<
    dyn Fn(&SessionRuntimeSettings) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError>
        + Send
        + Sync,
>;

/// Agent runtime factory shared by sessions.
#[derive(Clone)]
pub struct AgentSessionRuntimeFactory {
    /// Workspace root for tools and prompts.
    pub root: PathBuf,
    /// Credential store used for provider auth.
    pub credential_store: FileCredentialStore,
    task_max_subagent_depth: usize,
    mcp_server_instructions_enabled: bool,
    runtime_builder_override: Option<RuntimeBuilderOverride>,
}

impl std::fmt::Debug for AgentSessionRuntimeFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentSessionRuntimeFactory")
            .field("root", &self.root)
            .field("credential_store", &self.credential_store)
            .field("task_max_subagent_depth", &self.task_max_subagent_depth)
            .field(
                "mcp_server_instructions_enabled",
                &self.mcp_server_instructions_enabled,
            )
            .field(
                "runtime_builder_override",
                &self.runtime_builder_override.as_ref().map(|_| "<override>"),
            )
            .finish()
    }
}

impl AgentSessionRuntimeFactory {
    /// Create a runtime factory.
    #[must_use]
    pub fn new(root: PathBuf, credential_store: FileCredentialStore) -> Self {
        Self {
            root,
            credential_store,
            task_max_subagent_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            mcp_server_instructions_enabled: true,
            runtime_builder_override: None,
        }
    }

    /// Override the default task tool subagent depth for this runtime factory.
    #[must_use]
    pub const fn with_task_max_subagent_depth(mut self, max_subagent_depth: usize) -> Self {
        self.task_max_subagent_depth = max_subagent_depth;
        self
    }

    /// Enable or disable adding MCP server instructions to the system prompt.
    #[must_use]
    pub const fn with_mcp_server_instructions_enabled(mut self, enabled: bool) -> Self {
        self.mcp_server_instructions_enabled = enabled;
        self
    }

    /// Override runtime construction for embedders that provide their own provider builder.
    #[must_use]
    pub fn with_runtime_builder(
        mut self,
        build: impl Fn(&SessionRuntimeSettings) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.runtime_builder_override = Some(Arc::new(build));
        self
    }

    /// Build a runtime builder from session settings.
    pub async fn runtime_builder(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError> {
        if let Some(build) = self.runtime_builder_override.as_ref() {
            return build(settings)
                .map(|builder| builder.with_template_base_dir(self.root.clone()));
        }
        let builder = self.provider_builder(settings).await?;
        let mut builder = builder
            .with_template_base_dir(self.root.clone())
            .with_base_system_prompt()
            .with_permission_policy(PermissionPolicy::filesystem_external(
                self.root.clone(),
                PermissionEffect::ask(),
            ))
            .with_filesystem_tools(
                FilesystemConfig::new(self.root.clone())
                    .with_agents_read_context()
                    .with_skills_context(),
            )
            .with_filesystem_system_prompt(FilesystemPromptConfig::new(self.root.clone()))
            .with_agents_system_prompt(AgentsPromptConfig::new(self.root.clone()))
            .with_skills(SkillsConfig::new(self.root.clone()))
            .with_question_tool(QuestionConfig::new());

        #[cfg(feature = "shell")]
        {
            builder = builder
                .with_shell_tool(ShellConfig::new(self.root.clone()))
                .with_shell_system_prompt(ShellPromptConfig::new(self.root.clone()));
        }

        Ok(builder)
    }

    /// Build a runtime builder with default session-backed tools.
    pub(crate) async fn runtime_builder_for_sessions<S>(
        &self,
        settings: &SessionRuntimeSettings,
        sessions: SessionCoordinator<S>,
        mcp_runtime: Option<Arc<McpClientRuntime>>,
    ) -> Result<AgentRuntimeBuilder, AgentSessionRuntimeError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let mut builder = self.runtime_builder(settings).await?.with_task_tool(
            TaskConfig::new(sessions).with_max_subagent_depth(self.task_max_subagent_depth),
        );
        if let Some(mcp_runtime) = mcp_runtime {
            for tool in mcp_runtime.agent_tools().await {
                builder = builder.with_tool(tool);
            }
            if self.mcp_server_instructions_enabled {
                builder = builder.with_middleware(
                    crate::middleware::mcp_instructions::McpInstructionsMiddleware::new(
                        Arc::clone(&mcp_runtime),
                    ),
                );
            }
            builder = builder
                .with_middleware(
                    crate::middleware::mcp_resources::McpResourcesMiddleware::new(Arc::clone(
                        &mcp_runtime,
                    )),
                )
                .with_middleware(crate::middleware::mcp_prompts::McpPromptsMiddleware::new(
                    mcp_runtime,
                ));
        }
        Ok(builder)
    }

    pub(crate) async fn mcp_config(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<McpConfig, AgentSessionRuntimeError> {
        self.mcp_config_with_credentials(settings).await
    }

    pub(crate) fn mcp_root_uri(&self) -> String {
        format!("file://{}", self.root.display())
    }

    async fn provider_builder(
        &self,
        settings: &SessionRuntimeSettings,
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
                let mut provider_impl = AnthropicMessages::new(model_id.clone());
                if let Some(oauth) = oauth.as_ref() {
                    provider_impl = apply_oauth_headers(provider_impl, oauth);
                }
                if let Some(endpoint_url) = endpoint_url {
                    provider_impl = provider_impl.with_url(endpoint_url);
                }
                provider_impl = apply_config_headers(provider_impl, &headers, oauth_active);
                let model = ModelRef::new(provider_id(provider.api), model_id, provider.api);
                Ok(builder_with_model_cost(
                    AgentRuntimeBuilder::new(model, Arc::new(provider_impl)),
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
                    AgentRuntimeBuilder::new(model, Arc::new(provider_impl)),
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
                    AgentRuntimeBuilder::new(model, Arc::new(provider_impl)),
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
                    AgentRuntimeBuilder::new(model, Arc::new(provider_impl)),
                    provider.model_cost.clone(),
                ))
            }
        }
    }

    async fn oauth_access(
        &self,
        provider: &SessionProviderRuntimeSettings,
        resolved_headers: &BTreeMap<String, String>,
    ) -> Result<Option<OAuthCredential>, AgentSessionRuntimeError> {
        if configured_credential(resolved_headers).is_some() {
            return Ok(None);
        }
        let Some(definition) = provider.oauth.as_ref() else {
            return Ok(None);
        };
        let Some(StoredCredential::OAuth(mut credential)) = self
            .credential_store
            .read(&AuthTarget::provider(&provider.key).credential_key())
            .await
            .map_err(|error| {
                AgentSessionRuntimeError::Internal(format!(
                    "failed to read provider credentials: {error}"
                ))
            })?
        else {
            return Ok(None);
        };

        if credential.expires_at_ms <= now_ms() + 5 * 60 * 1000 {
            if credential.refresh.is_empty() {
                return Ok(None);
            }
            credential = refresh_oauth(definition, &credential).await?;
            let stored = credential.clone();
            self.credential_store
                .modify(
                    &AuthTarget::provider(&provider.key).credential_key(),
                    |_| Some(StoredCredential::OAuth(stored)),
                )
                .await
                .map_err(|error| {
                    AgentSessionRuntimeError::Internal(format!(
                        "failed to store refreshed credential: {error}"
                    ))
                })?;
        }

        Ok(Some(credential))
    }

    async fn mcp_config_with_credentials(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<McpConfig, AgentSessionRuntimeError> {
        let mut mcp = McpConfig::new();
        if settings.mcp_trace_propagation {
            mcp = mcp.with_trace_propagation(McpTracePropagation::TraceContext);
        }
        for server in &settings.mcp_servers {
            let config = match server.kind {
                SessionMcpServerRuntimeKind::Stdio => {
                    let mut parts = server.command.split_whitespace();
                    let command = parts.next().ok_or_else(|| {
                        AgentSessionRuntimeError::FailedPrecondition(format!(
                            "mcp server '{}' command must not be empty",
                            server.key
                        ))
                    })?;
                    McpServerConfig::stdio(server.key.clone(), command, parts)
                }
                SessionMcpServerRuntimeKind::StreamableHttp => {
                    let bearer_token = self.mcp_bearer_token(&server.key).await?;
                    McpServerConfig::streamable_http_with_bearer_token(
                        server.key.clone(),
                        server.url.clone(),
                        bearer_token,
                    )
                }
            };
            mcp = mcp.with_server(config);
        }
        Ok(mcp)
    }

    async fn mcp_bearer_token(
        &self,
        server_key: &str,
    ) -> Result<Option<String>, AgentSessionRuntimeError> {
        let Some(StoredCredential::OAuth(mut credential)) = self
            .credential_store
            .read(&AuthTarget::mcp_server(server_key).credential_key())
            .await
            .map_err(|error| {
                AgentSessionRuntimeError::Internal(format!(
                    "failed to read mcp credentials: {error}"
                ))
            })?
        else {
            return Ok(None);
        };

        if credential.expires_at_ms <= now_ms() + 5 * 60 * 1000 {
            let Some(definition) = mcp_refresh_definition(&credential) else {
                return Ok(None);
            };
            if credential.refresh.is_empty() {
                return Ok(None);
            }
            credential = match refresh_oauth(&definition, &credential).await {
                Ok(credential) => credential,
                Err(_) => return Ok(None),
            };
            let stored = credential.clone();
            self.credential_store
                .modify(&AuthTarget::mcp_server(server_key).credential_key(), |_| {
                    Some(StoredCredential::OAuth(stored))
                })
                .await
                .map_err(|error| {
                    AgentSessionRuntimeError::Internal(format!(
                        "failed to store refreshed mcp credential: {error}"
                    ))
                })?;
        }
        Ok(Some(credential.access))
    }
}

fn mcp_refresh_definition(credential: &OAuthCredential) -> Option<SessionOAuthProviderSettings> {
    let token_url = credential.metadata.get("token_url")?.clone();
    let client_id = credential.metadata.get("client_id")?.clone();
    Some(SessionOAuthProviderSettings {
        token_url,
        client_id,
        redirect_uri: credential
            .metadata
            .get("redirect_uri")
            .cloned()
            .unwrap_or_default(),
        token_request_style: SessionOAuthTokenRequestStyle::FormUrlencoded,
        token_fields: Vec::new(),
    })
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

fn ensure_no_reasoning_effort(
    settings: &SessionRuntimeSettings,
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
    settings: &SessionRuntimeSettings,
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

fn apply_oauth_headers<T>(mut provider: T, oauth: &OAuthCredential) -> T
where
    T: ConfigurableHeaders,
{
    provider = provider.with_header("authorization", &format!("Bearer {}", oauth.access));
    if let Some(account_id) = oauth.metadata.get("account_id") {
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
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn runtime_builder_resolves_headers_at_agent_boundary() {
        let factory = AgentSessionRuntimeFactory::new(
            std::env::temp_dir(),
            FileCredentialStore::new(
                std::env::temp_dir().join("waltr-agent-runtime-test-auth.json"),
            ),
        );
        let settings = SessionRuntimeSettings {
            provider: SessionProviderRuntimeSettings {
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
            mcp_servers: Vec::new(),
            mcp_trace_propagation: false,
        };

        let error = factory
            .runtime_builder(&settings)
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

    #[tokio::test]
    async fn provider_oauth_uses_namespaced_credential_key() {
        let auth_dir = std::env::temp_dir().join(format!(
            "waltr-agent-runtime-test-auth-{}",
            uuid::Uuid::new_v4()
        ));
        let auth_path = auth_dir.join("auth.json");
        let factory =
            AgentSessionRuntimeFactory::new(auth_dir, FileCredentialStore::new(auth_path));
        factory
            .credential_store
            .modify(&AuthTarget::provider("test").credential_key(), |_| {
                Some(StoredCredential::OAuth(OAuthCredential {
                    access: "access-token".to_owned(),
                    refresh: "refresh-token".to_owned(),
                    expires_at_ms: now_ms() + 60 * 60 * 1000,
                    metadata: BTreeMap::new(),
                }))
            })
            .await
            .expect("store credential");
        let provider = SessionProviderRuntimeSettings {
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
            }),
            reasoning_effort: None,
        };

        let credential = factory
            .oauth_access(&provider, &BTreeMap::new())
            .await
            .expect("oauth lookup succeeds")
            .expect("credential is found");

        assert_eq!(credential.access, "access-token");
    }

    #[tokio::test]
    async fn provider_oauth_without_refresh_token_is_not_used_when_expired() {
        let auth_dir = std::env::temp_dir().join(format!(
            "waltr-agent-runtime-test-auth-{}",
            uuid::Uuid::new_v4()
        ));
        let auth_path = auth_dir.join("auth.json");
        let factory =
            AgentSessionRuntimeFactory::new(auth_dir, FileCredentialStore::new(auth_path));
        factory
            .credential_store
            .modify(&AuthTarget::provider("test").credential_key(), |_| {
                Some(StoredCredential::OAuth(OAuthCredential {
                    access: "stale-token".to_owned(),
                    refresh: String::new(),
                    expires_at_ms: now_ms() - 1000,
                    metadata: BTreeMap::new(),
                }))
            })
            .await
            .expect("store credential");
        let provider = SessionProviderRuntimeSettings {
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
            }),
            reasoning_effort: None,
        };

        let credential = factory
            .oauth_access(&provider, &BTreeMap::new())
            .await
            .expect("oauth lookup succeeds");

        assert!(credential.is_none());
    }
}
