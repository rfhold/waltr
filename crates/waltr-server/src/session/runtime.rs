use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use crate::auth::{FileCredentialStore, StoredCredential, oauth};
use waltr::grpc::waltr::config::v1::{Configuration, McpServerType, ProviderType};
use waltr_agent::{
    AgentRuntime, AgentRuntimeBuilder, AgentsPromptConfig, FilesystemPromptConfig, ModelApi,
    ModelRef, PermissionEffect, PermissionPolicy, ProviderId, SessionSummary, SkillsConfig,
    filesystem::FilesystemConfig,
    mcp::{McpConfig, McpServerConfig},
    provider::{
        AnthropicMessages, OpenAiChatCompletions, OpenAiReasoningConfig, OpenAiReasoningEffort,
        OpenAiReasoningSummary, OpenAiResponses, OpenAiResponsesWebSocket,
    },
    question::QuestionConfig,
};
#[cfg(feature = "shell")]
use waltr_agent::{ShellPromptConfig, shell::ShellConfig};
use tonic::Status;
use tracing::Instrument as _;

use crate::config::headers::{
    configured_credential, is_credential_header, resolve_provider_headers,
};

use super::runtime_config::{StoredSessionRuntimeConfig, runtime_config_from_summary};

#[derive(Debug, Clone)]
pub(super) struct ServerRuntimeFactory {
    root: PathBuf,
    credential_store: FileCredentialStore,
}

impl ServerRuntimeFactory {
    pub(super) fn new(root: PathBuf, credential_store: FileCredentialStore) -> Self {
        Self {
            root,
            credential_store,
        }
    }

    pub(super) async fn build(
        &self,
        summary: &SessionSummary,
        config: &Configuration,
    ) -> Result<AgentRuntime, Status> {
        let runtime_config = runtime_config_from_summary(summary).ok_or_else(|| {
            Status::failed_precondition("session runtime config is required before prompting")
        })?;
        let span = tracing::info_span!(
            "server.runtime.build",
            provider_key = %runtime_config.provider.key,
            model_id = %runtime_config.provider.model_id,
            mcp_server_count = runtime_config.mcp_servers.len(),
        );
        self.build_from_config(runtime_config, config)
            .instrument(span)
            .await
    }

    async fn build_from_config(
        &self,
        runtime_config: StoredSessionRuntimeConfig,
        config: &Configuration,
    ) -> Result<AgentRuntime, Status> {
        let builder = provider_builder(&runtime_config, config, &self.credential_store).await?;

        let mut builder = builder
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

        let mcp_server_count = runtime_config.mcp_servers.len();
        let mcp = mcp_config(&runtime_config, config)?;
        builder = builder
            .with_mcp_servers(mcp)
            .instrument(tracing::info_span!(
                "server.runtime.mcp_init",
                mcp_server_count,
            ))
            .await
            .map_err(|error| {
                Status::failed_precondition(format!("failed to initialize MCP tools: {error}"))
            })?;

        Ok(builder.build())
    }
}

async fn provider_builder(
    runtime_config: &StoredSessionRuntimeConfig,
    config: &Configuration,
    credential_store: &FileCredentialStore,
) -> Result<AgentRuntimeBuilder, Status> {
    let provider_config = config
        .providers
        .get(&runtime_config.provider.key)
        .ok_or_else(|| {
            Status::failed_precondition(format!(
                "provider '{}' does not exist",
                runtime_config.provider.key
            ))
        })?;
    let model_config = provider_config
        .models
        .get(&runtime_config.provider.model_id)
        .ok_or_else(|| {
            Status::failed_precondition(format!(
                "model '{}' does not exist in provider '{}'",
                runtime_config.provider.model_id, runtime_config.provider.key
            ))
        })?;
    if model_config.r#type != waltr::grpc::waltr::config::v1::ModelType::Llm as i32 {
        return Err(Status::failed_precondition(format!(
            "model '{}' is not an LLM",
            runtime_config.provider.model_id
        )));
    }

    let provider_type =
        ProviderType::try_from(runtime_config.provider.r#type).unwrap_or(ProviderType::Unspecified);
    let resolved_headers = resolve_provider_headers(&provider_config.headers);
    let configured_api_key = configured_credential(&resolved_headers.headers);
    let oauth = if configured_api_key.is_empty() {
        let definition = oauth::provider_definition(&runtime_config.provider.key, provider_config);
        let oauth_configured = definition.is_some();
        resolve_oauth(&runtime_config.provider.key, definition, credential_store)
            .instrument(tracing::info_span!(
                "server.runtime.oauth",
                provider_key = %runtime_config.provider.key,
                oauth_configured,
            ))
            .await?
    } else {
        None
    };
    let model_id = runtime_config.provider.model_id.clone();
    let endpoint_url = (!runtime_config.provider.url.trim().is_empty())
        .then_some(runtime_config.provider.url.as_str());

    match provider_type {
        ProviderType::AnthropicMessages => {
            ensure_no_reasoning_effort(runtime_config, "Anthropic Messages")?;
            let oauth_active = oauth.is_some();
            let mut provider = if let Some(oauth) = oauth {
                apply_oauth_auth(AnthropicMessages::new(model_id.clone()), Some(oauth))
            } else {
                AnthropicMessages::new(model_id.clone())
            };
            if let Some(endpoint_url) = endpoint_url {
                provider = provider.with_url(endpoint_url);
            }
            provider = apply_config_headers(provider, &resolved_headers.headers, oauth_active);
            let model = ModelRef::new(ProviderId::Anthropic, model_id, ModelApi::AnthropicMessages);
            Ok(builder_with_model_cost(
                AgentRuntimeBuilder::new(model, Arc::new(provider)),
                model_config.cost.clone(),
            ))
        }
        ProviderType::OpenaiChatCompletions => {
            ensure_no_reasoning_effort(runtime_config, "OpenAI Chat Completions")?;
            let oauth_active = oauth.is_some();
            let provider = OpenAiChatCompletions::new(model_id.clone());
            let mut provider = apply_oauth_auth(provider, oauth);
            provider = apply_config_headers(provider, &resolved_headers.headers, oauth_active);
            if let Some(endpoint_url) = endpoint_url {
                provider = provider.with_url(endpoint_url);
            }
            let model = ModelRef::new(
                ProviderId::OpenAi,
                model_id,
                ModelApi::OpenAiChatCompletions,
            );
            Ok(builder_with_model_cost(
                AgentRuntimeBuilder::new(model, Arc::new(provider)),
                model_config.cost.clone(),
            ))
        }
        ProviderType::OpenaiResponses => {
            let oauth_active = oauth.is_some();
            let mut provider = apply_oauth_auth(OpenAiResponses::new(model_id.clone()), oauth);
            provider = apply_config_headers(provider, &resolved_headers.headers, oauth_active);
            if let Some(reasoning) = openai_reasoning_config(runtime_config)? {
                provider = provider.with_reasoning(reasoning);
            }
            if let Some(endpoint_url) = endpoint_url {
                provider = provider.with_url(endpoint_url);
            }
            let model = ModelRef::new(ProviderId::OpenAi, model_id, ModelApi::OpenAiResponses);
            Ok(builder_with_model_cost(
                AgentRuntimeBuilder::new(model, Arc::new(provider)),
                model_config.cost.clone(),
            ))
        }
        ProviderType::OpenaiResponsesWebsocket => {
            let oauth_active = oauth.is_some();
            let mut provider =
                apply_oauth_auth(OpenAiResponsesWebSocket::new(model_id.clone()), oauth);
            provider = apply_config_headers(provider, &resolved_headers.headers, oauth_active);
            if let Some(reasoning) = openai_reasoning_config(runtime_config)? {
                provider = provider.with_reasoning(reasoning);
            }
            if let Some(endpoint_url) = endpoint_url {
                provider = provider.with_url(endpoint_url);
            }
            let model = ModelRef::new(
                ProviderId::OpenAi,
                model_id,
                ModelApi::OpenAiResponsesWebSocket,
            );
            Ok(builder_with_model_cost(
                AgentRuntimeBuilder::new(model, Arc::new(provider)),
                model_config.cost.clone(),
            ))
        }
        ProviderType::Unspecified => {
            Err(Status::failed_precondition("provider type is unspecified"))
        }
    }
}

fn builder_with_model_cost(
    builder: AgentRuntimeBuilder,
    cost: Option<waltr::grpc::waltr::config::v1::ModelCost>,
) -> AgentRuntimeBuilder {
    let Some(cost) = cost else { return builder };
    builder.with_model_cost(waltr_agent::ModelCost {
        input_per_million_tokens: cost.input_per_million_tokens,
        output_per_million_tokens: cost.output_per_million_tokens,
        cache_read_per_million_tokens: cost.cache_read_per_million_tokens,
        cache_write_per_million_tokens: cost.cache_write_per_million_tokens,
        image_input_per_million_tokens: cost.image_input_per_million_tokens,
        image_input_per_image: cost.image_input_per_image,
        currency: cost.currency,
    })
}

trait RuntimeHeaderProvider: Sized {
    fn with_runtime_header(self, name: impl Into<String>, value: impl Into<String>) -> Self;
}

impl RuntimeHeaderProvider for AnthropicMessages {
    fn with_runtime_header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.with_header(name, value)
    }
}

impl RuntimeHeaderProvider for OpenAiChatCompletions {
    fn with_runtime_header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.with_header(name, value)
    }
}

impl RuntimeHeaderProvider for OpenAiResponses {
    fn with_runtime_header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.with_header(name, value)
    }
}

impl RuntimeHeaderProvider for OpenAiResponsesWebSocket {
    fn with_runtime_header(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.with_header(name, value)
    }
}

fn apply_config_headers<T>(
    mut provider: T,
    headers: &std::collections::HashMap<String, String>,
    oauth_active: bool,
) -> T
where
    T: RuntimeHeaderProvider,
{
    for (name, value) in headers {
        if oauth_active && is_credential_header(name) {
            continue;
        }
        provider = provider.with_runtime_header(name.clone(), value.clone());
    }
    provider
}

fn apply_oauth_auth<T>(mut provider: T, oauth: Option<ResolvedOAuth>) -> T
where
    T: RuntimeHeaderProvider,
{
    if let Some(oauth) = oauth {
        provider =
            provider.with_runtime_header("Authorization", format!("Bearer {}", oauth.access));
        if let Some(account_id) = oauth.metadata.get("account_id") {
            provider = provider
                .with_runtime_header("chatgpt-account-id", account_id.clone())
                .with_runtime_header("User-Agent", format!("waltr/{}", env!("CARGO_PKG_VERSION")));
        }
        provider
    } else {
        provider
    }
}

fn ensure_no_reasoning_effort(
    runtime_config: &StoredSessionRuntimeConfig,
    provider_name: &str,
) -> Result<(), Status> {
    if runtime_config.provider.reasoning_effort.is_some() {
        return Err(Status::failed_precondition(format!(
            "{provider_name} does not support session reasoning effort"
        )));
    }
    Ok(())
}

fn openai_reasoning_config(
    runtime_config: &StoredSessionRuntimeConfig,
) -> Result<Option<OpenAiReasoningConfig>, Status> {
    let Some(effort) = runtime_config.provider.reasoning_effort.as_deref() else {
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
            return Err(Status::failed_precondition(format!(
                "reasoning effort '{value}' is not supported by OpenAI Responses"
            )));
        }
    };

    Ok(Some(OpenAiReasoningConfig {
        effort,
        summary: Some(OpenAiReasoningSummary::Auto),
    }))
}

struct ResolvedOAuth {
    access: String,
    metadata: BTreeMap<String, String>,
}

async fn resolve_oauth(
    provider_key: &str,
    definition: Option<oauth::ProviderOAuthDefinition>,
    credential_store: &FileCredentialStore,
) -> Result<Option<ResolvedOAuth>, Status> {
    let Some(definition) = definition else {
        return Ok(None);
    };
    let Some(StoredCredential::OAuth(mut credential)) =
        credential_store.read(provider_key).await.map_err(|error| {
            Status::internal(format!("failed to read provider credentials: {error}"))
        })?
    else {
        return Ok(None);
    };

    if credential.expires_at_ms <= now_ms() + 5 * 60 * 1000 {
        credential = oauth::refresh(&definition.definition, &credential).await?;
        let stored = credential.clone();
        credential_store
            .modify(provider_key, |_| Some(StoredCredential::OAuth(stored)))
            .await
            .map_err(|error| {
                Status::internal(format!("failed to store refreshed credential: {error}"))
            })?;
    }

    Ok(Some(ResolvedOAuth {
        access: credential.access,
        metadata: credential.metadata,
    }))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

fn mcp_config(
    runtime_config: &StoredSessionRuntimeConfig,
    config: &Configuration,
) -> Result<McpConfig, Status> {
    let mut mcp = McpConfig::new();
    for stored in &runtime_config.mcp_servers {
        let current = config.mcp_servers.get(&stored.key).ok_or_else(|| {
            Status::failed_precondition(format!("mcp server '{}' does not exist", stored.key))
        })?;
        let server_type =
            McpServerType::try_from(stored.r#type).unwrap_or(McpServerType::Unspecified);
        let server = match server_type {
            McpServerType::Stdio => {
                let mut parts = current.command.split_whitespace();
                let command = parts.next().ok_or_else(|| {
                    Status::failed_precondition(format!(
                        "mcp server '{}' command must not be empty",
                        stored.key
                    ))
                })?;
                McpServerConfig::stdio(stored.key.clone(), command, parts)
            }
            McpServerType::Sse => {
                McpServerConfig::streamable_http(stored.key.clone(), current.url.clone())
            }
            McpServerType::Unspecified => {
                return Err(Status::failed_precondition(format!(
                    "mcp server '{}' type is unspecified",
                    stored.key
                )));
            }
        };
        mcp = mcp.with_server(server);
    }
    Ok(mcp)
}
