use std::collections::BTreeMap;

use waltr::grpc::waltr::{
    config::v1::{Configuration, McpServerType, ProviderType},
    session::v1::{
        RuntimeConfigMatch, SessionMcpServerRuntime, SessionProviderRuntime,
        SessionRuntimeConfig as ProtoSessionRuntimeConfig,
        SessionRuntimeConfigRequest as ProtoSessionRuntimeConfigRequest,
    },
};
use waltr_agent::SessionSummary;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tonic::Status;

pub(super) const SESSION_RUNTIME_CONFIG_METADATA_KEY: &str = "waltr.runtime_config";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StoredSessionRuntimeConfig {
    pub provider: StoredSessionProviderRuntime,
    pub mcp_servers: Vec<StoredSessionMcpServerRuntime>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StoredSessionProviderRuntime {
    pub key: String,
    pub r#type: i32,
    pub url: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StoredSessionMcpServerRuntime {
    pub key: String,
    pub r#type: i32,
    pub command: String,
    pub url: String,
}

pub(super) fn runtime_config_from_summary(
    summary: &SessionSummary,
) -> Option<StoredSessionRuntimeConfig> {
    summary
        .metadata_values
        .get(SESSION_RUNTIME_CONFIG_METADATA_KEY)
        .and_then(|value| match serde_json::from_value(value.clone()) {
            Ok(config) => Some(config),
            Err(error) => {
                tracing::warn!(%error, session_id = %summary.id.as_str(), "invalid session runtime metadata");
                None
            }
        })
}

pub(super) fn runtime_config_to_metadata_value(
    config: &StoredSessionRuntimeConfig,
) -> Result<Value, Status> {
    serde_json::to_value(config).map_err(|error| Status::internal(error.to_string()))
}

pub(super) fn runtime_metadata_values(
    config: &StoredSessionRuntimeConfig,
) -> Result<BTreeMap<String, Value>, Status> {
    Ok(BTreeMap::from([(
        SESSION_RUNTIME_CONFIG_METADATA_KEY.to_owned(),
        runtime_config_to_metadata_value(config)?,
    )]))
}

pub(super) fn runtime_config_to_proto(
    config: StoredSessionRuntimeConfig,
    current: &Configuration,
) -> ProtoSessionRuntimeConfig {
    ProtoSessionRuntimeConfig {
        provider: Some(provider_to_proto(config.provider, current)),
        mcp_servers: config
            .mcp_servers
            .into_iter()
            .map(|server| mcp_server_to_proto(server, current))
            .collect(),
    }
}

pub(super) fn resolve_runtime_config_for_create(
    requested: Option<ProtoSessionRuntimeConfigRequest>,
    config: &Configuration,
) -> Result<Option<StoredSessionRuntimeConfig>, Status> {
    requested.map_or_else(
        || runtime_config_from_defaults(config),
        |requested| resolve_runtime_config(requested, config).map(Some),
    )
}

pub(super) fn resolve_runtime_config(
    request: ProtoSessionRuntimeConfigRequest,
    config: &Configuration,
) -> Result<StoredSessionRuntimeConfig, Status> {
    if request.provider_key.trim().is_empty() {
        return Err(Status::invalid_argument("provider key must not be empty"));
    }
    if request.model_id.trim().is_empty() {
        return Err(Status::invalid_argument("model id must not be empty"));
    }

    let provider = config.providers.get(&request.provider_key).ok_or_else(|| {
        Status::invalid_argument(format!(
            "provider '{}' does not exist",
            request.provider_key
        ))
    })?;
    let model = provider.models.get(&request.model_id).ok_or_else(|| {
        Status::invalid_argument(format!(
            "model '{}' does not exist in provider '{}'",
            request.model_id, request.provider_key
        ))
    })?;

    let reasoning_effort = request
        .reasoning_effort
        .map(|effort| effort.trim().to_owned())
        .filter(|effort| !effort.is_empty());
    if let Some(effort) = &reasoning_effort {
        let supported = model.options.as_ref().is_some_and(|options| {
            options
                .reasoning_efforts
                .iter()
                .any(|value| value == effort)
        });
        if !supported {
            return Err(Status::invalid_argument(format!(
                "reasoning effort '{effort}' is not configured for model '{}' in provider '{}'",
                request.model_id, request.provider_key
            )));
        }
    }

    if !provider.models.contains_key(&request.model_id) {
        return Err(Status::invalid_argument(format!(
            "model '{}' does not exist in provider '{}'",
            request.model_id, request.provider_key
        )));
    }

    let mut mcp_servers = Vec::with_capacity(request.enabled_mcp_server_keys.len());
    for key in request.enabled_mcp_server_keys {
        let server = config.mcp_servers.get(&key).ok_or_else(|| {
            Status::invalid_argument(format!("mcp server '{key}' does not exist"))
        })?;
        mcp_servers.push(StoredSessionMcpServerRuntime {
            key,
            r#type: server.r#type,
            command: server.command.clone(),
            url: server.url.clone(),
        });
    }

    Ok(StoredSessionRuntimeConfig {
        provider: StoredSessionProviderRuntime {
            key: request.provider_key,
            r#type: provider.r#type,
            url: provider.url.clone(),
            model_id: request.model_id,
            reasoning_effort,
        },
        mcp_servers,
    })
}

fn runtime_config_from_defaults(
    config: &Configuration,
) -> Result<Option<StoredSessionRuntimeConfig>, Status> {
    let Some(defaults) = config.runtime_defaults.as_ref() else {
        return Ok(None);
    };
    if defaults.provider_key.trim().is_empty() && defaults.model_key.trim().is_empty() {
        return Ok(None);
    }

    let enabled_mcp_server_keys = if defaults.enabled_mcp_server_keys.is_empty() {
        if defaults.mcp_default_enabled.unwrap_or(true) {
            let mut keys = config.mcp_servers.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        } else {
            Vec::new()
        }
    } else {
        defaults.enabled_mcp_server_keys.clone()
    };

    resolve_runtime_config(
        ProtoSessionRuntimeConfigRequest {
            provider_key: defaults.provider_key.clone(),
            model_id: defaults.model_key.clone(),
            enabled_mcp_server_keys,
            reasoning_effort: defaults.reasoning_effort.clone(),
        },
        config,
    )
    .map(Some)
    .map_err(|status| Status::failed_precondition(status.message().to_owned()))
}

fn provider_to_proto(
    provider: StoredSessionProviderRuntime,
    current: &Configuration,
) -> SessionProviderRuntime {
    let r#match =
        current
            .providers
            .get(&provider.key)
            .map_or(RuntimeConfigMatch::Missing, |current| {
                if !current.models.contains_key(&provider.model_id) {
                    RuntimeConfigMatch::Missing
                } else if current.r#type == provider.r#type && current.url == provider.url {
                    RuntimeConfigMatch::Configured
                } else {
                    RuntimeConfigMatch::Changed
                }
            });

    SessionProviderRuntime {
        key: provider.key,
        r#type: provider_type(provider.r#type) as i32,
        url: provider.url,
        model_id: provider.model_id,
        reasoning_effort: provider.reasoning_effort,
        r#match: r#match as i32,
    }
}

fn mcp_server_to_proto(
    server: StoredSessionMcpServerRuntime,
    current: &Configuration,
) -> SessionMcpServerRuntime {
    let r#match =
        current
            .mcp_servers
            .get(&server.key)
            .map_or(RuntimeConfigMatch::Missing, |current| {
                if current.r#type == server.r#type
                    && current.command == server.command
                    && current.url == server.url
                {
                    RuntimeConfigMatch::Configured
                } else {
                    RuntimeConfigMatch::Changed
                }
            });

    SessionMcpServerRuntime {
        key: server.key,
        r#type: mcp_server_type(server.r#type) as i32,
        command: server.command,
        url: server.url,
        r#match: r#match as i32,
    }
}

fn provider_type(value: i32) -> ProviderType {
    ProviderType::try_from(value).unwrap_or(ProviderType::Unspecified)
}

fn mcp_server_type(value: i32) -> McpServerType {
    McpServerType::try_from(value).unwrap_or(McpServerType::Unspecified)
}
