use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use crate::{
    AgentRuntimeBuilder, AgentsPromptConfig, AuthTarget, AuthenticationFailure,
    BoxCompletionStream, BoxStreamFuture, CompletionRequest, FilesystemPromptConfig, LlmError,
    LlmStream, ModelApi, ModelCost, ModelRef, PermissionEffect, PermissionPolicy, ProviderId,
    SkillsConfig,
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
use futures_util::StreamExt as _;
use mcp::{McpTracePropagation, client::McpBearerProviderError};

mod headers;
mod oauth;

use headers::{configured_credential, is_credential_header, resolve_provider_headers};
use oauth::now_ms;
pub use oauth::{CredentialRefreshCoordinator, CredentialRefreshError, RefreshMode};

use super::{
    AgentSessionRuntimeError, CredentialStore, OAuthCredential, SessionCoordinator,
    SessionMcpServerRuntimeKind, SessionProviderRuntimeSettings, SessionRuntimeSettings,
    SessionStore,
};

const ENV_PREFIX: &str = "${env:";
const OAUTH_REFRESH_AHEAD_MS: i64 = 5 * 60 * 1000;

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
    pub credential_store: CredentialStore,
    task_max_subagent_depth: usize,
    mcp_server_instructions_enabled: bool,
    outbound_oauth_loopback_test_mode: bool,
    trusted_mcp_oauth_origin: Option<reqwest::Url>,
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
                "outbound_oauth_loopback_test_mode",
                &self.outbound_oauth_loopback_test_mode,
            )
            .field("trusted_mcp_oauth_origin", &self.trusted_mcp_oauth_origin)
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
    pub fn new(root: PathBuf, credential_store: impl Into<CredentialStore>) -> Self {
        Self {
            root,
            credential_store: credential_store.into(),
            task_max_subagent_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            mcp_server_instructions_enabled: true,
            outbound_oauth_loopback_test_mode: false,
            trusted_mcp_oauth_origin: None,
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

    /// Permit private HTTPS peers for one host-supplied MCP OAuth origin.
    #[must_use]
    pub fn with_trusted_mcp_oauth_origin(mut self, origin: reqwest::Url) -> Self {
        self.trusted_mcp_oauth_origin = Some(origin);
        self
    }

    #[cfg(test)]
    const fn with_outbound_oauth_loopback_test_mode(mut self) -> Self {
        self.outbound_oauth_loopback_test_mode = true;
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
    ///
    /// # Errors
    ///
    /// Returns an error when provider, credential, MCP, or runtime construction fails.
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

    #[cfg(test)]
    pub(crate) async fn mcp_config(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<McpConfig, AgentSessionRuntimeError> {
        self.mcp_config_with_credentials(settings)
            .await
            .map(|(config, _)| config)
    }

    pub(crate) async fn mcp_config_and_credentialed_servers(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<(McpConfig, BTreeSet<String>), AgentSessionRuntimeError> {
        self.mcp_config_with_credentials(settings).await
    }

    pub(crate) async fn preflight_cached_mcp_authentication(
        &self,
        settings: &SessionRuntimeSettings,
        credentialed_servers: &BTreeSet<String>,
    ) -> Result<(), AgentSessionRuntimeError> {
        for server in settings
            .mcp_servers
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

    pub(crate) fn mcp_root_uri(&self) -> String {
        format!("file://{}", self.root.display())
    }

    #[allow(clippy::too_many_lines)]
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
        provider: &SessionProviderRuntimeSettings,
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

    async fn mcp_config_with_credentials(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<(McpConfig, BTreeSet<String>), AgentSessionRuntimeError> {
        let mut mcp = McpConfig::new();
        let mut credentialed_servers = BTreeSet::new();
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
        server: &super::SessionMcpServerRuntimeSettings,
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
        let mut coordinator = CredentialRefreshCoordinator::new(self.credential_store.clone());
        if let Some(origin) = &self.trusted_mcp_oauth_origin {
            coordinator = coordinator.with_trusted_mcp_oauth_origin(origin.clone());
        }
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
    server: &super::SessionMcpServerRuntimeSettings,
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
        SessionOAuthJwtMetadata, SessionOAuthJwtToken, SessionOAuthProviderSettings,
        SessionOAuthTokenRequestStyle,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        extract::State,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use tokio::sync::Mutex;

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

    fn mcp_server_settings() -> super::super::SessionMcpServerRuntimeSettings {
        super::super::SessionMcpServerRuntimeSettings {
            key: "remote".to_owned(),
            kind: SessionMcpServerRuntimeKind::StreamableHttp,
            command: String::new(),
            url: "https://mcp.example/resource".to_owned(),
        }
    }

    #[tokio::test]
    async fn mcp_config_reports_exact_servers_configured_with_bearer_providers() {
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            .mcp_config_and_credentialed_servers(&SessionRuntimeSettings {
                provider: SessionProviderRuntimeSettings {
                    key: "test".to_owned(),
                    api: ModelApi::OpenAiResponses,
                    url: String::new(),
                    model_id: "test-model".to_owned(),
                    model_cost: None,
                    headers: BTreeMap::new(),
                    oauth: None,
                    reasoning_effort: None,
                },
                mcp_servers: vec![mcp_server_settings(), anonymous],
                mcp_trace_propagation: false,
            })
            .await
            .expect("build MCP config");

        assert_eq!(credentialed, BTreeSet::from(["remote".to_owned()]));
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
                Some(StoredCredential::OAuth(make_provider_credential(
                    "access-token",
                    Some("refresh-token"),
                    Some(now_ms() + 60 * 60 * 1000),
                    BTreeMap::new(),
                )))
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
        );
        let settings = SessionRuntimeSettings {
            provider: SessionProviderRuntimeSettings {
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
            mcp_servers: Vec::new(),
            mcp_trace_propagation: false,
        };

        let error = factory
            .runtime_builder(&settings)
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
        let factory =
            AgentSessionRuntimeFactory::new(auth_dir, FileCredentialStore::new(auth_path));
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
    async fn mcp_refresh_preserves_rotation_metadata() {
        let (token_url, server_state) = token_server(
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "rotated-refresh",
                "token_type": "Bearer",
                "expires_in": 3600
            }),
            Duration::ZERO,
        )
        .await;
        let expected = mcp_state(token_url);
        let directory = tempfile::tempdir().expect("temp directory");
        let factory = AgentSessionRuntimeFactory::new(
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
    async fn normal_mcp_refresh_rejects_near_expiry_current_claim_result() {
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

        let error = coordinator
            .refresh_mcp(&mcp_server_settings(), RefreshMode::Normal)
            .await
            .expect_err("near-expiry current result must require refresh");

        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::CredentialRequired
        );
        assert!(error.message().contains("usable credential"));
    }

    #[tokio::test]
    async fn normal_mcp_refresh_rejects_near_expiry_current_after_failed_cas() {
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

        let error = task
            .await
            .expect("refresh task")
            .expect_err("near-expiry failed-CAS current must require refresh");

        assert_eq!(
            error.reason(),
            crate::AuthenticationFailureReason::CredentialRequired
        );
        assert!(error.message().contains("changed during refresh"));
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            .mcp_config(&SessionRuntimeSettings {
                provider: SessionProviderRuntimeSettings {
                    key: "test".to_owned(),
                    api: ModelApi::OpenAiResponses,
                    url: String::new(),
                    model_id: "test-model".to_owned(),
                    model_cost: None,
                    headers: BTreeMap::new(),
                    oauth: None,
                    reasoning_effort: None,
                },
                mcp_servers: vec![mcp_server_settings()],
                mcp_trace_propagation: false,
            })
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
            None,
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
            None,
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
            directory.path().to_path_buf(),
            FileCredentialStore::new(directory.path().join("auth.json")),
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
