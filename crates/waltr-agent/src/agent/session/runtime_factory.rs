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
use mcp::{McpTracePropagation, client::McpBearerProviderError};

mod headers;
mod oauth;

use headers::{configured_credential, is_credential_header, resolve_provider_headers};
use oauth::{RefreshFailureDisposition, now_ms, refresh_mcp_oauth, refresh_oauth};

use super::{
    AgentSessionRuntimeError, FileCredentialStore, McpOAuthEndpoints, McpOAuthRegistration,
    McpOAuthRegistrationMethod, McpOAuthState, McpOAuthToken, McpRefreshClaim,
    McpRefreshClaimResult, OAuthCredential, SessionCoordinator, SessionMcpServerRuntimeKind,
    SessionProviderRuntimeSettings, SessionRuntimeSettings, SessionStore, StoredCredential,
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
    pub credential_store: FileCredentialStore,
    task_max_subagent_depth: usize,
    mcp_server_instructions_enabled: bool,
    outbound_oauth_loopback_test_mode: bool,
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
            outbound_oauth_loopback_test_mode: false,
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

    pub(crate) async fn mcp_config(
        &self,
        settings: &SessionRuntimeSettings,
    ) -> Result<McpConfig, AgentSessionRuntimeError> {
        self.mcp_config_with_credentials(settings).await
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
                                    AgentSessionRuntimeError::AuthorizationRequired {
                                        reason: crate::AuthenticationFailureReason::NetworkFailed,
                                        ..
                                    } => McpBearerProviderError::Unavailable,
                                    AgentSessionRuntimeError::AuthorizationRequired { .. } => {
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
        Ok(mcp)
    }

    #[allow(clippy::too_many_lines)]
    async fn mcp_bearer_token(
        &self,
        server: &super::SessionMcpServerRuntimeSettings,
    ) -> Result<Option<String>, AgentSessionRuntimeError> {
        let server_key = &server.key;
        let credential_key = AuthTarget::mcp_server(server_key).credential_key();
        let Some(stored) = self
            .credential_store
            .read(&credential_key)
            .await
            .map_err(|error| {
                AgentSessionRuntimeError::Internal(format!(
                    "failed to read mcp credentials: {error}"
                ))
            })?
        else {
            return Ok(None);
        };
        let legacy = matches!(&stored, StoredCredential::OAuth(_));
        let Some(mut credential) = mcp_oauth_state(stored, &server.url) else {
            return Ok(None);
        };
        validate_mcp_credential(server, &credential)?;
        if legacy {
            self.credential_store
                .modify(&credential_key, |_| {
                    Some(StoredCredential::McpOAuth(credential.clone()))
                })
                .await
                .map_err(|error| {
                    AgentSessionRuntimeError::Internal(format!(
                        "failed to upgrade mcp credential: {error}"
                    ))
                })?;
        }
        validate_mcp_access_token(server_key, &credential)?;

        if should_refresh_mcp_token(&credential) {
            let Some(stored) =
                self.credential_store
                    .read(&credential_key)
                    .await
                    .map_err(|error| {
                        AgentSessionRuntimeError::Internal(format!(
                            "failed to re-read mcp credentials: {error}"
                        ))
                    })?
            else {
                return Err(AgentSessionRuntimeError::AuthorizationRequired {
                    target: AuthTarget::mcp_server(server_key),
                    message: "MCP OAuth credential is required".to_owned(),
                    reason: crate::AuthenticationFailureReason::CredentialRequired,
                    mcp_authorization: None,
                });
            };
            let Some(current) = mcp_oauth_state(stored, &server.url) else {
                return Ok(None);
            };
            validate_mcp_credential(server, &current)?;
            validate_mcp_access_token(server_key, &current)?;
            if !should_refresh_mcp_token(&current) {
                return Ok(Some(current.token.access_token));
            }
            if current
                .token
                .refresh_token
                .as_deref()
                .is_none_or(str::is_empty)
            {
                if !current.token.expires_by(now_ms()) {
                    return Ok(Some(current.token.access_token));
                }
                return Err(AgentSessionRuntimeError::AuthorizationRequired {
                    target: AuthTarget::mcp_server(server_key),
                    message: "MCP OAuth credential has expired".to_owned(),
                    reason: crate::AuthenticationFailureReason::CredentialRequired,
                    mcp_authorization: None,
                });
            }
            let generation = current.token.refresh_generation;
            let claim = McpRefreshClaim {
                generation,
                claim_id: uuid::Uuid::now_v7().to_string(),
                lease_expires_at_ms: now_ms().saturating_add(30_000),
            };
            match self
                .credential_store
                .claim_mcp_refresh(&credential_key, claim.clone(), now_ms())
                .await
                .map_err(|error| {
                    AgentSessionRuntimeError::Internal(format!(
                        "failed to claim mcp credential refresh: {error}"
                    ))
                })? {
                McpRefreshClaimResult::Current(current) => {
                    validate_mcp_credential(server, &current)?;
                    validate_mcp_access_token(server_key, &current)?;
                    if should_refresh_mcp_token(&current) {
                        return Err(mcp_authorization_error(
                            server_key,
                            "MCP OAuth refresh produced an expired credential",
                            crate::AuthenticationFailureReason::CredentialRequired,
                        ));
                    }
                    return Ok(Some(current.token.access_token));
                }
                McpRefreshClaimResult::ReauthorizationRequired => {
                    return Err(AgentSessionRuntimeError::AuthorizationRequired {
                        target: AuthTarget::mcp_server(server_key),
                        message: "MCP OAuth refresh requires reauthorization".to_owned(),
                        reason: crate::AuthenticationFailureReason::CredentialRequired,
                        mcp_authorization: None,
                    });
                }
                McpRefreshClaimResult::Claimed(_) => {}
            }
            let refreshed =
                match refresh_mcp_oauth(&current, self.outbound_oauth_loopback_test_mode).await {
                    Ok(credential) => credential,
                    Err(error) => {
                        let indeterminate = matches!(
                            error.disposition,
                            RefreshFailureDisposition::Indeterminate
                                | RefreshFailureDisposition::ReauthorizationRequired
                        );
                        self.credential_store
                            .abort_mcp_refresh(
                                &credential_key,
                                generation,
                                &claim.claim_id,
                                indeterminate,
                            )
                            .await
                            .map_err(|store_error| {
                                AgentSessionRuntimeError::Internal(format!(
                                    "failed to abort mcp credential refresh: {store_error}"
                                ))
                            })?;
                        if error.disposition == RefreshFailureDisposition::Indeterminate
                            && !current.token.expires_by(now_ms())
                        {
                            return Ok(Some(current.token.access_token));
                        }
                        return Err(mcp_authorization_error(
                            server_key,
                            error.message,
                            error.reason,
                        ));
                    }
                };
            let committed = self
                .credential_store
                .commit_mcp_refresh(&credential_key, generation, &claim.claim_id, refreshed)
                .await
                .map_err(|error| {
                    AgentSessionRuntimeError::Internal(format!(
                        "failed to store refreshed mcp credential: {error}"
                    ))
                })?;
            if !committed {
                let Some(StoredCredential::McpOAuth(current)) = self
                    .credential_store
                    .read(&credential_key)
                    .await
                    .map_err(|error| {
                        AgentSessionRuntimeError::Internal(format!(
                            "failed to read concurrently refreshed mcp credential: {error}"
                        ))
                    })?
                else {
                    return Err(mcp_authorization_error(
                        server_key,
                        "MCP OAuth credential changed during refresh",
                        crate::AuthenticationFailureReason::CredentialRequired,
                    ));
                };
                validate_mcp_credential(server, &current)?;
                validate_mcp_access_token(server_key, &current)?;
                if !should_refresh_mcp_token(&current) {
                    return Ok(Some(current.token.access_token));
                }
                return Err(mcp_authorization_error(
                    server_key,
                    "MCP OAuth credential changed during refresh",
                    crate::AuthenticationFailureReason::CredentialRequired,
                ));
            }
            let Some(StoredCredential::McpOAuth(stored)) = self
                .credential_store
                .read(&credential_key)
                .await
                .map_err(|error| {
                    AgentSessionRuntimeError::Internal(format!(
                        "failed to read refreshed mcp credential: {error}"
                    ))
                })?
            else {
                return Err(AgentSessionRuntimeError::AuthorizationRequired {
                    target: AuthTarget::mcp_server(server_key),
                    message: "MCP OAuth credential disappeared during refresh".to_owned(),
                    reason: crate::AuthenticationFailureReason::CredentialRequired,
                    mcp_authorization: None,
                });
            };
            credential = stored;
        }
        Ok(Some(credential.token.access_token))
    }
}

fn mcp_oauth_state(credential: StoredCredential, resource: &str) -> Option<McpOAuthState> {
    match credential {
        StoredCredential::McpOAuth(state) => Some(state),
        StoredCredential::OAuth(legacy) => {
            let token_url = legacy.metadata.get("token_url")?.clone();
            let client_id = legacy.metadata.get("client_id")?.clone();
            let redirect_uri = legacy.metadata.get("redirect_uri")?.clone();
            Some(McpOAuthState {
                // ca2a376 persisted this reduced shape before typed MCP state existed.
                version: 0,
                resource: resource.to_owned(),
                issuer: legacy
                    .metadata
                    .get("issuer")
                    .cloned()
                    .or_else(|| endpoint_origin(&token_url))?,
                registration: McpOAuthRegistration {
                    method: McpOAuthRegistrationMethod::Preregistered,
                    client_id,
                    redirect_uri,
                    token_endpoint_auth_method: legacy
                        .metadata
                        .get("token_endpoint_auth_method")
                        .cloned()
                        .unwrap_or_else(|| "none".to_owned()),
                    created_at_ms: metadata_i64(&legacy, "registration_created_at_ms"),
                    expires_at_ms: metadata_i64(&legacy, "registration_expires_at_ms"),
                },
                endpoints: McpOAuthEndpoints {
                    authorization: legacy
                        .metadata
                        .get("authorization_url")
                        .cloned()
                        .unwrap_or_default(),
                    token: token_url,
                    revocation: legacy.metadata.get("revocation_url").cloned(),
                },
                token: McpOAuthToken {
                    access_token: legacy.access,
                    refresh_token: (!legacy.refresh.is_empty()).then_some(legacy.refresh),
                    token_type: legacy
                        .metadata
                        .get("token_type")
                        .cloned()
                        .unwrap_or_else(|| "Bearer".to_owned()),
                    granted_scopes: legacy
                        .metadata
                        .get("scope")
                        .map(|scope| scope.split_whitespace().map(ToOwned::to_owned).collect())
                        .unwrap_or_default(),
                    expires_at_ms: legacy.expires_at_ms,
                    refresh_generation: 0,
                },
            })
        }
    }
}

fn endpoint_origin(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

fn metadata_i64(credential: &OAuthCredential, key: &str) -> Option<i64> {
    credential.metadata.get(key)?.parse().ok()
}

fn should_refresh_mcp_token(credential: &McpOAuthState) -> bool {
    credential
        .token
        .expires_by(now_ms().saturating_add(OAUTH_REFRESH_AHEAD_MS))
}

fn validate_mcp_access_token(
    server_key: &str,
    credential: &McpOAuthState,
) -> Result<(), AgentSessionRuntimeError> {
    if credential.token.access_token.is_empty()
        || !credential.token.token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(AgentSessionRuntimeError::AuthorizationRequired {
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
    credential: &McpOAuthState,
) -> Result<(), AgentSessionRuntimeError> {
    let valid_version = matches!(credential.version, 0 | 1);
    let registration_current = credential
        .registration
        .expires_at_ms
        .is_none_or(|expires_at_ms| expires_at_ms > now_ms());
    let valid = valid_version
        && credential.resource == server.url
        && !credential.issuer.is_empty()
        && endpoint_is_bound(&credential.issuer)
        && !credential.registration.client_id.is_empty()
        && !credential.registration.redirect_uri.is_empty()
        && credential
            .registration
            .token_endpoint_auth_method
            .eq_ignore_ascii_case("none")
        && registration_current
        && endpoint_is_bound(&credential.endpoints.token);
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
    AgentSessionRuntimeError::AuthorizationRequired {
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
    use crate::{SessionOAuthProviderSettings, SessionOAuthTokenRequestStyle};
    use std::{
        collections::BTreeMap,
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

    #[derive(Clone)]
    struct TokenServerState {
        requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
        request_count: Arc<AtomicUsize>,
        response: serde_json::Value,
        status: StatusCode,
        delay: Duration,
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

    fn mcp_state(token_url: String) -> McpOAuthState {
        McpOAuthState {
            version: 1,
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
            token: McpOAuthToken {
                access_token: "old-access".to_owned(),
                refresh_token: Some("old-refresh".to_owned()),
                token_type: "Bearer".to_owned(),
                granted_scopes: vec!["mcp:read".to_owned(), "mcp:write".to_owned()],
                expires_at_ms: now_ms() - 1,
                refresh_generation: 7,
            },
        }
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
        let Some(StoredCredential::McpOAuth(actual)) = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
        else {
            panic!("expected typed MCP credential");
        };
        assert_eq!(actual.resource, expected.resource);
        assert_eq!(actual.issuer, expected.issuer);
        assert_eq!(actual.registration, expected.registration);
        assert_eq!(actual.endpoints, expected.endpoints);
        assert_eq!(
            actual.token.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
        assert_eq!(actual.token.token_type, expected.token.token_type);
        assert_eq!(actual.token.granted_scopes, expected.token.granted_scopes);
        assert_eq!(actual.token.refresh_generation, 8);
        let requests = server_state.requests.lock().await;
        assert_eq!(requests[0].get("resource"), Some(&expected.resource));
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
        credential.token.expires_at_ms = expires_at_ms;
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
        let Some(StoredCredential::McpOAuth(actual)) = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
        else {
            panic!("expected typed MCP credential");
        };
        assert_eq!(actual.token.expires_at_ms, -1);
        assert_eq!(actual.token.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(actual.token.refresh_generation, 8);
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
    async fn legacy_mcp_refresh_upgrades_and_accepts_rotated_refresh_token() {
        let (token_url, _) = token_server(
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "rotated-refresh",
                "token_type": "bearer",
                "scope": "mcp:read",
                "expires_in": 3600
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
        factory
            .credential_store
            .modify(&key, |_| {
                Some(StoredCredential::OAuth(OAuthCredential {
                    access: "old-access".to_owned(),
                    refresh: "old-refresh".to_owned(),
                    expires_at_ms: now_ms() - 1,
                    metadata: BTreeMap::from([
                        ("token_url".to_owned(), token_url),
                        ("client_id".to_owned(), "client-id".to_owned()),
                        (
                            "redirect_uri".to_owned(),
                            "http://127.0.0.1/oauth/callback".to_owned(),
                        ),
                        ("token_type".to_owned(), "Bearer".to_owned()),
                    ]),
                }))
            })
            .await
            .expect("store legacy credential");

        factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect("refresh legacy credential");

        let Some(StoredCredential::McpOAuth(actual)) = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
        else {
            panic!("legacy credential was not upgraded");
        };
        assert_eq!(
            actual.token.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
        assert_eq!(actual.token.granted_scopes, ["mcp:read"]);
        assert_eq!(actual.token.refresh_generation, 1);
        assert_eq!(actual.version, 0);
        assert_eq!(actual.resource, "https://mcp.example/resource");
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
        let Some(StoredCredential::McpOAuth(actual)) = factory
            .credential_store
            .read(&key)
            .await
            .expect("read credential")
        else {
            panic!("expected typed MCP credential");
        };
        assert_eq!(actual.token.refresh_generation, 8);
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
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect_err("invalid grant requires authorization");
        assert!(matches!(
            error,
            AgentSessionRuntimeError::AuthorizationRequired {
                reason: crate::AuthenticationFailureReason::CredentialRejected,
                ..
            }
        ));
        assert!(!error.to_string().contains("old-refresh"));
        let Some(StoredCredential::McpOAuth(current)) = factory
            .credential_store
            .read(&key)
            .await
            .expect("read retained credential")
        else {
            panic!("expected retained MCP OAuth state");
        };
        assert_eq!(current.token.refresh_token.as_deref(), Some("old-refresh"));

        factory
            .mcp_bearer_token(&mcp_server_settings())
            .await
            .expect_err("blocked refresh requires authorization");
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
        credential.token.expires_at_ms = now_ms() + 60_000;
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
        let Some(StoredCredential::McpOAuth(current)) = factory
            .credential_store
            .read(&key)
            .await
            .expect("read retained credential")
        else {
            panic!("expected retained MCP OAuth state");
        };
        assert_eq!(current.token.access_token, "old-access");
        assert_eq!(current.token.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(current.token.refresh_generation, 7);
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
        credential.resource = "https://other.example/resource".to_owned();
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
            AgentSessionRuntimeError::AuthorizationRequired {
                reason: crate::AuthenticationFailureReason::ConfigurationRequired,
                ..
            }
        ));

        let mut credential = mcp_state("https://issuer.example/token".to_owned());
        credential.issuer.clear();
        assert!(validate_mcp_credential(&mcp_server_settings(), &credential).is_err());
        credential.issuer = "https://issuer.example".to_owned();
        credential.registration.client_id.clear();
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
            .expect_err("expired credential requires authorization");
        assert!(matches!(
            error,
            AgentSessionRuntimeError::AuthorizationRequired {
                reason: crate::AuthenticationFailureReason::CredentialRequired,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn mcp_refresh_refuses_redirects_without_replaying_refresh_secret() {
        let (token_url, redirected_requests) = malicious_token_server(false).await;
        let error = refresh_mcp_oauth(&mcp_state(token_url), true)
            .await
            .expect_err("redirect must fail");

        assert_eq!(
            error.reason,
            crate::AuthenticationFailureReason::ResponseInvalid
        );
        assert_eq!(redirected_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn mcp_refresh_rejects_oversized_token_response() {
        let (token_url, _) = malicious_token_server(true).await;
        let error = refresh_mcp_oauth(&mcp_state(token_url), true)
            .await
            .expect_err("oversized response must fail");

        assert_eq!(
            error.reason,
            crate::AuthenticationFailureReason::ResponseInvalid
        );
        assert!(error.message.contains("size limit"));
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
        credential.token.expires_at_ms = now_ms() + 60_000;
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
