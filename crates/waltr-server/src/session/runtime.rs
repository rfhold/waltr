use std::{path::PathBuf, sync::Arc};

use waltr::grpc::waltr::config::v1::{Configuration, McpServerType, ProviderType};
use waltr_agent::{
    AgentRuntime, AgentRuntimeBuilder, AgentsPromptConfig, FilesystemPromptConfig, ModelApi,
    ModelRef, PermissionEffect, PermissionPolicy, ProviderId, SessionSummary, SkillsConfig,
    filesystem::FilesystemConfig,
    mcp::{McpConfig, McpServerConfig},
    provider::{AnthropicMessages, OpenAiChatCompletions},
    question::QuestionConfig,
};
#[cfg(feature = "shell")]
use waltr_agent::{ShellPromptConfig, shell::ShellConfig};
use tonic::Status;

use super::runtime_config::{StoredSessionRuntimeConfig, runtime_config_from_summary};

#[derive(Debug, Clone)]
pub(super) struct ServerRuntimeFactory {
    root: PathBuf,
}

impl ServerRuntimeFactory {
    pub(super) fn new(root: PathBuf) -> Self {
        Self { root }
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

        let provider_type = ProviderType::try_from(runtime_config.provider.r#type)
            .unwrap_or(ProviderType::Unspecified);
        let api_key = configured_api_key(&provider_config.headers);
        let model_id = runtime_config.provider.model_id.clone();
        let base_url = (!runtime_config.provider.url.trim().is_empty())
            .then_some(runtime_config.provider.url.as_str());

        let builder = match provider_type {
            ProviderType::Anthropic => {
                let mut provider = AnthropicMessages::new(api_key, model_id.clone());
                if let Some(base_url) = base_url {
                    provider = provider.with_base_url(base_url);
                }
                let model =
                    ModelRef::new(ProviderId::Anthropic, model_id, ModelApi::AnthropicMessages);
                AgentRuntimeBuilder::new(model, Arc::new(provider))
            }
            ProviderType::Openai => {
                let mut provider = OpenAiChatCompletions::new(api_key, model_id.clone());
                if let Some(base_url) = base_url {
                    provider = provider.with_base_url(base_url);
                }
                let model = ModelRef::new(
                    ProviderId::OpenAi,
                    model_id,
                    ModelApi::OpenAiChatCompletions,
                );
                AgentRuntimeBuilder::new(model, Arc::new(provider))
            }
            ProviderType::Unspecified => {
                return Err(Status::failed_precondition("provider type is unspecified"));
            }
        };

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
            .with_mcp_servers(mcp_config(runtime_config, config)?)
            .await
            .map_err(|error| {
                Status::failed_precondition(format!("failed to initialize MCP tools: {error}"))
            })?;

        Ok(builder.build())
    }
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
    runtime_config: StoredSessionRuntimeConfig,
    config: &Configuration,
) -> Result<McpConfig, Status> {
    let mut mcp = McpConfig::new();
    for stored in runtime_config.mcp_servers {
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
                McpServerConfig::stdio(stored.key, command, parts)
            }
            McpServerType::Sse => McpServerConfig::streamable_http(stored.key, current.url.clone()),
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
