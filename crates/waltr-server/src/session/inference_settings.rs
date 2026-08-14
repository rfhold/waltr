use std::collections::BTreeMap;

use crate::auth::oauth;
use waltr::grpc::waltr::{
    config::v1::{Configuration, ProviderType},
    session::v1::{
        InferenceProviderSettings as ProtoInferenceProviderSettings,
        InferenceSettings as ProtoInferenceSettings, InferenceSettingsMatch,
        InferenceSettingsRequest as ProtoInferenceSettingsRequest,
    },
};
use waltr_agent::{InferenceProviderSettings, InferenceSettings, ModelApi, ModelCost};
use tonic::Status;

pub(super) fn inference_settings_to_proto(
    settings: InferenceSettings,
    current: &Configuration,
) -> ProtoInferenceSettings {
    ProtoInferenceSettings {
        provider: Some(provider_to_proto(settings.provider, current)),
    }
}

pub(super) fn resolve_inference_settings_for_create(
    requested: Option<ProtoInferenceSettingsRequest>,
    config: &Configuration,
) -> Result<Option<InferenceSettings>, Status> {
    requested.map_or_else(
        || inference_settings_from_defaults(config),
        |requested| resolve_inference_settings(requested, config).map(Some),
    )
}

pub(super) fn resolve_inference_settings(
    request: ProtoInferenceSettingsRequest,
    config: &Configuration,
) -> Result<InferenceSettings, Status> {
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
        .map(|definition| oauth::provider_refresh_settings(&definition.definition));

    Ok(InferenceSettings {
        provider: InferenceProviderSettings {
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
    })
}

fn inference_settings_from_defaults(
    config: &Configuration,
) -> Result<Option<InferenceSettings>, Status> {
    let Some(defaults) = config.inference_defaults.as_ref() else {
        return Ok(None);
    };
    if defaults.provider_key.trim().is_empty() && defaults.model_key.trim().is_empty() {
        return Ok(None);
    }

    resolve_inference_settings(
        ProtoInferenceSettingsRequest {
            provider_key: defaults.provider_key.clone(),
            model_id: defaults.model_key.clone(),
            reasoning_effort: defaults.reasoning_effort.clone(),
        },
        config,
    )
    .map(Some)
    .map_err(|status| Status::failed_precondition(status.message().to_owned()))
}

fn provider_to_proto(
    provider: InferenceProviderSettings,
    current: &Configuration,
) -> ProtoInferenceProviderSettings {
    let stored_type = provider_type(provider.api) as i32;
    let r#match =
        current
            .providers
            .get(&provider.key)
            .map_or(InferenceSettingsMatch::Missing, |current| {
                if !current.models.contains_key(&provider.model_id) {
                    InferenceSettingsMatch::Missing
                } else if current.r#type == stored_type && current.url == provider.url {
                    InferenceSettingsMatch::Configured
                } else {
                    InferenceSettingsMatch::Changed
                }
            });

    ProtoInferenceProviderSettings {
        key: provider.key,
        r#type: stored_type,
        url: provider.url,
        model_id: provider.model_id,
        reasoning_effort: provider.reasoning_effort,
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

const fn provider_type(api: ModelApi) -> ProviderType {
    match api {
        ModelApi::AnthropicMessages => ProviderType::AnthropicMessages,
        ModelApi::OpenAiChatCompletions => ProviderType::OpenaiChatCompletions,
        ModelApi::OpenAiResponses => ProviderType::OpenaiResponses,
        ModelApi::OpenAiResponsesWebSocket => ProviderType::OpenaiResponsesWebsocket,
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
