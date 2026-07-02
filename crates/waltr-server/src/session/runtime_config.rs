use std::collections::BTreeMap;

use crate::auth::oauth;
use waltr::grpc::waltr::{
    config::v1::{Configuration, McpServerType, OauthTokenRequestStyle, ProviderType},
    session::v1::{
        RuntimeConfigMatch, SessionMcpServerRuntime, SessionProviderRuntime,
        SessionRuntimeConfig as ProtoSessionRuntimeConfig,
        SessionRuntimeConfigRequest as ProtoSessionRuntimeConfigRequest,
    },
};
use waltr_agent::{
    ModelApi, ModelCost, SessionMcpServerRuntimeKind, SessionMcpServerRuntimeSettings,
    SessionOAuthProviderSettings, SessionOAuthTokenRequestStyle, SessionProviderRuntimeSettings,
    SessionRuntimeSettings,
};
use tonic::Status;

pub(super) fn runtime_config_to_proto(
    config: SessionRuntimeSettings,
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
) -> Result<Option<SessionRuntimeSettings>, Status> {
    requested.map_or_else(
        || runtime_config_from_defaults(config),
        |requested| resolve_runtime_config(requested, config).map(Some),
    )
}

pub(super) fn resolve_runtime_config(
    request: ProtoSessionRuntimeConfigRequest,
    config: &Configuration,
) -> Result<SessionRuntimeSettings, Status> {
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

    let provider_api = provider_api(provider.r#type)?;
    let oauth = oauth::provider_definition(&request.provider_key, provider)
        .map(|definition| oauth_settings(definition.definition));

    let mut mcp_servers = Vec::with_capacity(request.enabled_mcp_server_keys.len());
    for key in request.enabled_mcp_server_keys {
        let server = config.mcp_servers.get(&key).ok_or_else(|| {
            Status::invalid_argument(format!("mcp server '{key}' does not exist"))
        })?;
        mcp_servers.push(SessionMcpServerRuntimeSettings {
            key,
            kind: mcp_server_kind(server.r#type)?,
            command: server.command.clone(),
            url: server.url.clone(),
        });
    }

    Ok(SessionRuntimeSettings {
        provider: SessionProviderRuntimeSettings {
            key: request.provider_key,
            api: provider_api,
            url: provider.url.clone(),
            model_id: request.model_id,
            model_cost: model.cost.clone().map(model_cost),
            headers: provider
                .headers
                .clone()
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            oauth,
            reasoning_effort,
        },
        mcp_servers,
    })
}

fn runtime_config_from_defaults(
    config: &Configuration,
) -> Result<Option<SessionRuntimeSettings>, Status> {
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
    provider: SessionProviderRuntimeSettings,
    current: &Configuration,
) -> SessionProviderRuntime {
    let stored_type = provider_type(provider.api) as i32;
    let r#match =
        current
            .providers
            .get(&provider.key)
            .map_or(RuntimeConfigMatch::Missing, |current| {
                if !current.models.contains_key(&provider.model_id) {
                    RuntimeConfigMatch::Missing
                } else if current.r#type == stored_type && current.url == provider.url {
                    RuntimeConfigMatch::Configured
                } else {
                    RuntimeConfigMatch::Changed
                }
            });

    SessionProviderRuntime {
        key: provider.key,
        r#type: stored_type,
        url: provider.url,
        model_id: provider.model_id,
        reasoning_effort: provider.reasoning_effort,
        r#match: r#match as i32,
    }
}

fn mcp_server_to_proto(
    server: SessionMcpServerRuntimeSettings,
    current: &Configuration,
) -> SessionMcpServerRuntime {
    let stored_type = mcp_server_type(server.kind) as i32;
    let r#match =
        current
            .mcp_servers
            .get(&server.key)
            .map_or(RuntimeConfigMatch::Missing, |current| {
                if current.r#type == stored_type
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
        r#type: stored_type,
        command: server.command,
        url: server.url,
        r#match: r#match as i32,
    }
}

fn provider_api(value: i32) -> Result<ModelApi, Status> {
    match ProviderType::try_from(value).unwrap_or(ProviderType::Unspecified) {
        ProviderType::AnthropicMessages => Ok(ModelApi::AnthropicMessages),
        ProviderType::OpenaiChatCompletions => Ok(ModelApi::OpenAiChatCompletions),
        ProviderType::OpenaiResponses => Ok(ModelApi::OpenAiResponses),
        ProviderType::OpenaiResponsesWebsocket => Ok(ModelApi::OpenAiResponsesWebSocket),
        ProviderType::Unspecified => Err(Status::invalid_argument("provider type is unspecified")),
    }
}

fn provider_type(api: ModelApi) -> ProviderType {
    match api {
        ModelApi::AnthropicMessages => ProviderType::AnthropicMessages,
        ModelApi::OpenAiChatCompletions => ProviderType::OpenaiChatCompletions,
        ModelApi::OpenAiResponses => ProviderType::OpenaiResponses,
        ModelApi::OpenAiResponsesWebSocket => ProviderType::OpenaiResponsesWebsocket,
    }
}

fn mcp_server_kind(value: i32) -> Result<SessionMcpServerRuntimeKind, Status> {
    match McpServerType::try_from(value).unwrap_or(McpServerType::Unspecified) {
        McpServerType::Stdio => Ok(SessionMcpServerRuntimeKind::Stdio),
        McpServerType::Sse => Ok(SessionMcpServerRuntimeKind::StreamableHttp),
        McpServerType::Unspecified => {
            Err(Status::invalid_argument("mcp server type is unspecified"))
        }
    }
}

fn mcp_server_type(kind: SessionMcpServerRuntimeKind) -> McpServerType {
    match kind {
        SessionMcpServerRuntimeKind::Stdio => McpServerType::Stdio,
        SessionMcpServerRuntimeKind::StreamableHttp => McpServerType::Sse,
    }
}

fn oauth_settings(
    definition: waltr::grpc::waltr::config::v1::OAuthProvider,
) -> SessionOAuthProviderSettings {
    let token_request_style = match OauthTokenRequestStyle::try_from(definition.token_request_style)
        .unwrap_or(OauthTokenRequestStyle::Unspecified)
    {
        OauthTokenRequestStyle::FormUrlencoded => SessionOAuthTokenRequestStyle::FormUrlencoded,
        OauthTokenRequestStyle::Json | OauthTokenRequestStyle::Unspecified => {
            SessionOAuthTokenRequestStyle::Json
        }
    };

    SessionOAuthProviderSettings {
        token_url: definition.token_url,
        client_id: definition.client_id,
        redirect_uri: definition.redirect_uri,
        token_request_style,
        token_fields: Vec::new(),
    }
}

fn model_cost(cost: waltr::grpc::waltr::config::v1::ModelCost) -> ModelCost {
    ModelCost {
        input_per_million_tokens: cost.input_per_million_tokens,
        output_per_million_tokens: cost.output_per_million_tokens,
        cache_read_per_million_tokens: cost.cache_read_per_million_tokens,
        cache_write_per_million_tokens: cost.cache_write_per_million_tokens,
        image_input_per_million_tokens: cost.image_input_per_million_tokens,
        image_input_per_image: cost.image_input_per_image,
        currency: cost.currency,
    }
}
