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
        OpenAiReasoningSummary, OpenAiResponses,
    },
    question::QuestionConfig,
};
#[cfg(feature = "shell")]
use waltr_agent::{ShellPromptConfig, shell::ShellConfig};
use tonic::Status;

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
        self.build_from_config(runtime_config, config).await
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

        builder = builder
            .with_mcp_servers(mcp_config(&runtime_config, config)?)
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
    let configured_api_key = configured_api_key(&provider_config.headers);
    let oauth = if configured_api_key.is_empty() {
        resolve_oauth(
            &runtime_config.provider.key,
            oauth::provider_definition(&runtime_config.provider.key, provider_config),
            credential_store,
        )
        .await?
    } else {
        None
    };
    let model_id = runtime_config.provider.model_id.clone();
    let base_url = (!runtime_config.provider.url.trim().is_empty())
        .then_some(runtime_config.provider.url.as_str());

    match provider_type {
        ProviderType::Anthropic => {
            ensure_no_reasoning_effort(runtime_config, "Anthropic Messages")?;
            let mut provider = if let Some(oauth) = oauth {
                anthropic_oauth_provider(oauth, model_id.clone())
            } else {
                let provider = AnthropicMessages::new(model_id.clone());
                if configured_api_key.is_empty() {
                    provider
                } else {
                    provider.with_header("x-api-key", configured_api_key)
                }
            };
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            let model = ModelRef::new(ProviderId::Anthropic, model_id, ModelApi::AnthropicMessages);
            Ok(AgentRuntimeBuilder::new(model, Arc::new(provider)))
        }
        ProviderType::Openai => {
            ensure_no_reasoning_effort(runtime_config, "OpenAI Chat Completions")?;
            let provider = OpenAiChatCompletions::new(model_id.clone());
            let mut provider = if let Some(oauth) = oauth {
                provider.with_header("Authorization", format!("Bearer {}", oauth.access))
            } else if configured_api_key.is_empty() {
                provider
            } else {
                provider.with_header("Authorization", format!("Bearer {configured_api_key}"))
            };
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            let model = ModelRef::new(
                ProviderId::OpenAi,
                model_id,
                ModelApi::OpenAiChatCompletions,
            );
            Ok(AgentRuntimeBuilder::new(model, Arc::new(provider)))
        }
        ProviderType::OpenaiCodex => {
            let oauth = oauth.ok_or_else(|| {
                Status::failed_precondition(
                    "OpenAI Codex provider requires stored OAuth credentials",
                )
            })?;
            let account_id = oauth.metadata.get("account_id").cloned().ok_or_else(|| {
                Status::failed_precondition("OpenAI Codex OAuth credential is missing account_id")
            })?;
            let mut provider = OpenAiResponses::new(model_id.clone())
                .with_header("Authorization", format!("Bearer {}", oauth.access))
                .with_header("chatgpt-account-id", account_id)
                .with_header("originator", "waltr")
                .with_header("User-Agent", format!("waltr/{}", env!("CARGO_PKG_VERSION")))
                .with_header("OpenAI-Beta", "responses=experimental")
                .with_header("accept", "text/event-stream");
            if let Some(reasoning) = openai_reasoning_config(runtime_config)? {
                provider = provider.with_reasoning(reasoning);
            }
            if let Some(base_url) = base_url {
                provider = provider.with_url(format!(
                    "{}/codex/responses",
                    base_url.trim_end_matches('/')
                ));
            } else {
                provider = provider.with_url("https://chatgpt.com/backend-api/codex/responses");
            }
            let model = ModelRef::new(ProviderId::OpenAi, model_id, ModelApi::OpenAiResponses);
            Ok(AgentRuntimeBuilder::new(model, Arc::new(provider)))
        }
        ProviderType::Unspecified => {
            Err(Status::failed_precondition("provider type is unspecified"))
        }
    }
}

fn anthropic_oauth_provider(oauth: ResolvedOAuth, model_id: String) -> AnthropicMessages {
    AnthropicMessages::new(model_id)
        .with_header("Authorization", format!("Bearer {}", oauth.access))
        .with_header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .with_header("user-agent", "claude-cli/2.1.75")
        .with_header("x-app", "cli")
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

fn configured_api_key(headers: &std::collections::HashMap<String, String>) -> String {
    for key in ["api_key", "x-api-key", "authorization"] {
        if let Some(value) = headers
            .get(key)
            .or_else(|| headers.get(&key.to_ascii_uppercase()))
        {
            let trimmed = value.trim();
            return trimmed
                .strip_prefix("Bearer ")
                .unwrap_or(trimmed)
                .to_owned();
        }
    }
    String::new()
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
