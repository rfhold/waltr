use crate::{resource_catalog::AgentDefinitionSnapshot, routine::ports::PortError};
use serde::Deserialize;
use waltr_agent::InferenceSettings;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "from", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InferenceSource {
    Defaults {},
    SourceSession {
        #[serde(default, rename = "reasoningEffort", deserialize_with = "effort")]
        reasoning_effort: Option<Option<String>>,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExactInference {
    provider: String,
    model: String,
    #[serde(default, deserialize_with = "effort")]
    reasoning_effort: Option<Option<String>>,
}

fn effort<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum InferenceSelector {
    Source(InferenceSource),
    Exact(ExactInference),
}
impl Default for InferenceSelector {
    fn default() -> Self {
        Self::Source(InferenceSource::Defaults {})
    }
}
impl InferenceSelector {
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Source(InferenceSource::Defaults {}))
    }

    pub fn resolve(
        &self,
        snapshot: &AgentDefinitionSnapshot,
        source: Option<&InferenceSettings>,
    ) -> Result<InferenceSettings, PortError> {
        let configuration = snapshot.configuration();
        let (mut settings, override_effort) = match self {
            Self::Source(InferenceSource::SourceSession { reasoning_effort }) => (
                source.cloned().ok_or(PortError::Invalid)?,
                reasoning_effort.clone(),
            ),
            _ => {
                let (provider, model, reasoning) = match self {
                    Self::Exact(exact) => (
                        &exact.provider,
                        &exact.model,
                        exact.reasoning_effort.clone(),
                    ),
                    _ => {
                        let defaults = configuration
                            .inference_defaults
                            .as_ref()
                            .ok_or(PortError::Invalid)?;
                        (
                            &defaults.provider_key,
                            &defaults.model_key,
                            defaults.reasoning_effort.clone().map(Some),
                        )
                    }
                };
                if [provider, model].into_iter().any(|value| {
                    value.is_empty()
                        || value.len() > 256
                        || value.trim() != value
                        || value.chars().any(char::is_control)
                }) {
                    return Err(PortError::Invalid);
                }
                let selected = configuration
                    .providers
                    .get(provider)
                    .and_then(|p| p.models.get(model))
                    .ok_or(PortError::Invalid)?;
                if selected.r#type != waltr::grpc::waltr::config::v1::ModelType::Llm as i32 {
                    return Err(PortError::Invalid);
                }
                let mut settings = crate::session::pinned_inference_settings(
                    provider.clone(),
                    model.clone(),
                    configuration,
                )
                .map_err(|_| PortError::Invalid)?;
                settings.provider.reasoning_effort = None;
                (settings, reasoning)
            }
        };
        if let Some(reasoning) = override_effort {
            if let Some(value) = reasoning {
                let model = configuration
                    .providers
                    .get(&settings.provider.key)
                    .and_then(|p| p.models.get(&settings.provider.model_id))
                    .ok_or(PortError::Invalid)?;
                if !model
                    .options
                    .as_ref()
                    .is_some_and(|options| options.reasoning_efforts.contains(&value))
                {
                    return Err(PortError::Invalid);
                }
                settings.provider.reasoning_effort = Some(value);
            } else {
                settings = settings.with_reasoning_disabled();
            }
        }
        Ok(settings)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HelperInput {
    pub text: bool,
    pub max_bytes: usize,
}
impl Default for HelperInput {
    fn default() -> Self {
        Self {
            text: true,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}
impl HelperInput {
    pub fn validate(&self) -> Result<(), PortError> {
        if !(1..=8 * 1024 * 1024).contains(&self.max_bytes) {
            return Err(PortError::Invalid);
        }
        Ok(())
    }
}
