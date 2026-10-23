//! Pinned, network-free speech provider settings for future adapter construction.

use waltr::grpc::waltr::config::v1::{self as proto, ModelType, ProviderType};

use super::{AgentDefinitionLoadError, materialize::invalid_value};

/// Validated speech options from one catalog revision.
#[derive(Debug, Clone)]
pub enum SpeechSettings {
    /// Google Speech V2 recognition options.
    Recognition(proto::SpeechRecognitionOptions),
    /// Google Text-to-Speech V1 synthesis options.
    Synthesis(proto::SpeechSynthesisOptions),
}

/// Owned speech settings for adapter construction from a pinned catalog snapshot.
#[derive(Debug, Clone)]
pub struct ResolvedSpeechModel {
    /// Explicit provider catalog key.
    pub provider_key: String,
    /// Explicit model key within the provider.
    pub model_key: String,
    /// Validated HTTP(S) upstream endpoint without credentials.
    pub endpoint: String,
    /// Operator-declared unary and streaming capabilities.
    pub capabilities: proto::SpeechCapabilities,
    /// Validated options for the selected speech operation.
    pub settings: SpeechSettings,
}

/// Resolve explicit keys from a pinned catalog snapshot; never performs I/O.
pub fn resolve_speech_model(
    configuration: &proto::Configuration,
    provider_key: &str,
    model_key: &str,
    expected: ModelType,
    streaming: bool,
) -> Result<ResolvedSpeechModel, String> {
    let provider = configuration
        .providers
        .get(provider_key)
        .ok_or("unknown speech provider")?;
    validate_speech_provider(provider, provider_key).map_err(|error| error.to_string())?;
    let model = provider
        .models
        .get(model_key)
        .ok_or("unknown speech model")?;
    if model.r#type != expected as i32 {
        return Err("speech model kind does not match task".into());
    }
    let capabilities = model
        .speech_capabilities
        .ok_or("missing speech capabilities")?;
    if (streaming && !capabilities.streaming) || (!streaming && !capabilities.unary) {
        return Err("speech model does not support requested operation".into());
    }
    let settings = match expected {
        ModelType::Stt => SpeechSettings::Recognition(
            model
                .recognition
                .clone()
                .ok_or("missing recognition options")?,
        ),
        ModelType::Tts => {
            SpeechSettings::Synthesis(model.synthesis.clone().ok_or("missing synthesis options")?)
        }
        _ => return Err("expected STT or TTS model".into()),
    };
    Ok(ResolvedSpeechModel {
        provider_key: provider_key.into(),
        model_key: model_key.into(),
        endpoint: provider.url.clone(),
        capabilities,
        settings,
    })
}

pub(super) fn validate_speech_source(
    source: &str,
    path: &str,
) -> Result<(), AgentDefinitionLoadError> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(source).map_err(|source| {
        AgentDefinitionLoadError::ResourceYaml {
            path: path.into(),
            source,
        }
    })?;
    if let Some(models) = value["spec"]["models"].as_mapping() {
        for model in models.values() {
            check_keys(
                model,
                &[
                    "name",
                    "type",
                    "recognition",
                    "synthesis",
                    "speech_capabilities",
                ],
                path,
            )?;
            if let Some(recognition) = model.get("recognition") {
                check_keys(
                    recognition,
                    &[
                        "recognizer",
                        "model",
                        "language_codes",
                        "sample_rate_hz",
                        "channels",
                        "interim_results",
                        "automatic_punctuation",
                    ],
                    path,
                )?;
            }
            if let Some(synthesis) = model.get("synthesis") {
                check_keys(
                    synthesis,
                    &[
                        "voice",
                        "language_code",
                        "sample_rate_hz",
                        "channels",
                        "speaking_rate",
                        "pitch",
                    ],
                    path,
                )?;
            }
            if let Some(capabilities) = model.get("speech_capabilities") {
                check_keys(capabilities, &["unary", "streaming"], path)?;
            }
        }
    }
    Ok(())
}

fn check_keys(
    value: &serde_yaml_ng::Value,
    allowed: &[&str],
    path: &str,
) -> Result<(), AgentDefinitionLoadError> {
    if let Some(fields) = value.as_mapping() {
        for field in fields.keys() {
            if !field.as_str().is_some_and(|name| allowed.contains(&name)) {
                return Err(invalid_value(
                    path,
                    format!("unknown speech model field `{field:?}`"),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn is_speech_transport(kind: i32) -> bool {
    matches!(
        ProviderType::try_from(kind),
        Ok(ProviderType::GoogleSpeechV2 | ProviderType::GoogleTextToSpeechV1)
    )
}

pub(super) fn validate_speech_provider(
    provider: &proto::Provider,
    path: &str,
) -> Result<(), AgentDefinitionLoadError> {
    let fail = |message: &str| invalid_value(path, message.into());
    if !is_speech_transport(provider.r#type) {
        return Err(fail("expected a speech provider transport"));
    }
    let url = url::Url::parse(&provider.url).map_err(|_| fail("invalid speech endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(fail(
            "speech endpoint must be HTTP(S) without userinfo, query, or fragment",
        ));
    }
    if !provider.headers.is_empty()
        || provider.oauth_settings.is_some()
        || provider.oauth_compatible
    {
        return Err(fail(
            "speech transports do not support authentication or headers in the initial version",
        ));
    }
    if provider.models.is_empty() {
        return Err(fail("speech providers require explicit models"));
    }
    for (key, model) in &provider.models {
        if key.trim().is_empty() || model.name.trim().is_empty() {
            return Err(fail("speech model keys and names must not be empty"));
        }
        if model.options.is_some()
            || model.limits.is_some()
            || model.capabilities.is_some()
            || model.catalog_match.is_some()
            || model.cost.is_some()
        {
            return Err(fail(
                "speech models cannot declare inference or models.dev fields",
            ));
        }
        let capabilities = model
            .speech_capabilities
            .as_ref()
            .ok_or_else(|| fail("speechCapabilities is required"))?;
        if !capabilities.unary && !capabilities.streaming {
            return Err(fail("at least one speech capability must be enabled"));
        }
        match ProviderType::try_from(provider.r#type) {
            Ok(ProviderType::GoogleSpeechV2) => {
                let options = model
                    .recognition
                    .as_ref()
                    .ok_or_else(|| fail("recognition options are required"))?;
                if model.r#type != ModelType::Stt as i32 || model.synthesis.is_some() {
                    return Err(fail(
                        "googleSpeechV2 requires STT models with recognition options only",
                    ));
                }
                if options.recognizer.trim().is_empty()
                    || options.model.trim().is_empty()
                    || options.language_codes.is_empty()
                    || options.language_codes.len() > 3
                    || options
                        .language_codes
                        .iter()
                        .any(|code| code.trim().is_empty())
                    || !(8000..=48000).contains(&options.sample_rate_hz)
                    || !(1..=8).contains(&options.channels)
                {
                    return Err(fail(
                        "invalid recognition recognizer, model, languages, or PCM format",
                    ));
                }
            }
            Ok(ProviderType::GoogleTextToSpeechV1) => {
                let options = model
                    .synthesis
                    .as_ref()
                    .ok_or_else(|| fail("synthesis options are required"))?;
                if model.r#type != ModelType::Tts as i32 || model.recognition.is_some() {
                    return Err(fail(
                        "googleTextToSpeechV1 requires TTS models with synthesis options only",
                    ));
                }
                if options.voice.trim().is_empty()
                    || options.language_code.trim().is_empty()
                    || !(8000..=48000).contains(&options.sample_rate_hz)
                    || options.channels != 1
                    || options
                        .speaking_rate
                        .is_some_and(|rate| !rate.is_finite() || !(0.25..=2.0).contains(&rate))
                    || options
                        .pitch
                        .is_some_and(|pitch| !pitch.is_finite() || !(-20.0..=20.0).contains(&pitch))
                {
                    return Err(fail(
                        "invalid synthesis voice, language, PCM format, rate, or pitch",
                    ));
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}
