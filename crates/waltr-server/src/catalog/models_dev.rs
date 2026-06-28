//! Models.dev catalog types and helpers.

use std::{cmp::Ordering, collections::BTreeMap};

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;

/// Parsed models.dev catalog keyed by provider id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalog {
    /// Providers included in the catalog.
    pub providers: BTreeMap<String, CatalogProvider>,
}

impl ModelCatalog {
    /// Parse a models.dev `api.json` payload.
    pub fn from_json_str(input: &str) -> Result<Self, serde_json::Error> {
        let providers = serde_json::from_str::<BTreeMap<String, RawCatalogProvider>>(input)?
            .into_iter()
            .map(|(provider_key, provider)| {
                let provider = CatalogProvider::from_raw(provider_key, provider);
                (provider.id.clone(), provider)
            })
            .collect();

        Ok(Self { providers })
    }

    /// Return a provider by exact provider id.
    #[must_use]
    pub fn provider(&self, provider_id: &str) -> Option<&CatalogProvider> {
        self.providers.get(provider_id)
    }

    /// Return a model by exact provider id and model id.
    #[must_use]
    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<&CatalogModel> {
        self.provider(provider_id)
            .and_then(|provider| provider.models.get(model_id))
    }

    /// Search providers and models with deterministic ranking.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<CatalogMatch> {
        let query_key = normalize_key(query);
        if query_key.is_empty() || limit == 0 {
            return Vec::new();
        }

        let query_tokens = tokenize(query);
        let mut matches = Vec::new();
        for provider in self.providers.values() {
            for model in provider.models.values() {
                let score = search_score(provider, model, &query_key, &query_tokens);
                if score == 0 {
                    continue;
                }

                matches.push(CatalogMatch {
                    model: CatalogModelRef::new(&provider.id, &model.id),
                    confidence: CatalogMatchConfidence::Suggested,
                    score,
                    reason: "search".to_owned(),
                });
            }
        }

        sort_matches(&mut matches);
        matches.truncate(limit);
        matches
    }

    /// Suggest catalog matches for a configured provider/model identity.
    #[must_use]
    pub fn suggest_matches(
        &self,
        provider_key: Option<&str>,
        provider_url: Option<&str>,
        model_id: &str,
        limit: usize,
    ) -> Vec<CatalogMatch> {
        let model_key = normalize_key(model_id);
        if model_key.is_empty() || limit == 0 {
            return Vec::new();
        }

        let provider_key_raw = provider_key
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let provider_key = provider_key
            .map(normalize_key)
            .filter(|value| !value.is_empty());
        let provider_url = provider_url
            .map(normalize_url)
            .filter(|value| !value.is_empty());
        let model_tokens = tokenize(model_id);
        let mut matches = Vec::new();

        for provider in self.providers.values() {
            for model in provider.models.values() {
                let provider_id_exact = provider_key_raw.is_some_and(|key| key == provider.id);
                let provider_id_match = provider_key
                    .as_ref()
                    .is_some_and(|key| *key == normalize_key(&provider.id));
                let provider_name_match = provider_key
                    .as_ref()
                    .is_some_and(|key| *key == normalize_key(&provider.name));
                let api_match = provider_url
                    .as_ref()
                    .is_some_and(|url| provider_url_matches(provider, model, url));
                let model_id_exact = model.id == model_id.trim();

                if provider_id_exact && model_id_exact {
                    matches.push(CatalogMatch {
                        model: CatalogModelRef::new(&provider.id, &model.id),
                        confidence: CatalogMatchConfidence::Exact,
                        score: 10_000,
                        reason: "provider_id_model_id".to_owned(),
                    });
                    continue;
                }

                if api_match && model_id_exact {
                    matches.push(CatalogMatch {
                        model: CatalogModelRef::new(&provider.id, &model.id),
                        confidence: CatalogMatchConfidence::High,
                        score: 9_000,
                        reason: "provider_api_model_id".to_owned(),
                    });
                    continue;
                }

                let score = suggestion_score(
                    provider,
                    model,
                    &model_key,
                    &model_tokens,
                    provider_id_match || provider_name_match,
                    api_match,
                );
                if score == 0 {
                    continue;
                }

                matches.push(CatalogMatch {
                    model: CatalogModelRef::new(&provider.id, &model.id),
                    confidence: CatalogMatchConfidence::Suggested,
                    score,
                    reason: "similar_identity".to_owned(),
                });
            }
        }

        sort_matches(&mut matches);
        matches.truncate(limit);
        matches
    }
}

/// Provider facts from the catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    /// Stable provider id.
    pub id: String,
    /// Human-readable provider name.
    pub name: String,
    /// Provider API base URL when present.
    pub api: Option<String>,
    /// AI SDK package name when present.
    pub npm: Option<String>,
    /// Documentation URL when present.
    pub doc: Option<String>,
    /// Environment variable names used by the provider.
    pub env: Vec<String>,
    /// Models exposed by this provider, keyed by model id.
    pub models: BTreeMap<String, CatalogModel>,
}

impl CatalogProvider {
    fn from_raw(provider_key: String, raw: RawCatalogProvider) -> Self {
        let id = raw.id.unwrap_or(provider_key);
        let name = raw.name.unwrap_or_else(|| id.clone());
        let models = raw
            .models
            .into_iter()
            .map(|(model_key, model)| {
                let model = CatalogModel::from_raw(model_key, model);
                (model.id.clone(), model)
            })
            .collect();

        Self {
            id,
            name,
            api: raw.api,
            npm: raw.npm,
            doc: raw.doc,
            env: raw.env,
            models,
        }
    }
}

/// Model facts from the catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// Stable provider-specific model id.
    pub id: String,
    /// Human-readable model name.
    pub name: String,
    /// Model family when present.
    pub family: Option<String>,
    /// Catalog status when present.
    pub status: Option<String>,
    /// Release date string when present.
    pub release_date: Option<String>,
    /// Last-updated date string when present.
    pub last_updated: Option<String>,
    /// Knowledge cutoff date string when present.
    pub knowledge: Option<String>,
    /// Token limits.
    #[serde(default, alias = "limit")]
    pub limits: CatalogLimits,
    /// Model pricing facts.
    #[serde(default)]
    pub cost: CatalogCost,
    /// Supported capabilities.
    #[serde(default)]
    pub capabilities: CatalogCapabilities,
    /// Input and output modalities.
    #[serde(default)]
    pub modalities: CatalogModalities,
    /// Reasoning controls supported by the model.
    #[serde(default)]
    pub reasoning_options: Vec<CatalogReasoningOption>,
    /// Interleaved thinking/tool content support.
    #[serde(default)]
    pub interleaved: CatalogInterleaved,
    /// Model-level provider override when present.
    pub provider: Option<CatalogModelProvider>,
}

impl CatalogModel {
    fn from_raw(model_key: String, raw: RawCatalogModel) -> Self {
        let id = raw.id.unwrap_or(model_key);
        let name = raw.name.unwrap_or_else(|| id.clone());

        Self {
            id,
            name,
            family: raw.family,
            status: raw.status,
            release_date: raw.release_date,
            last_updated: raw.last_updated,
            knowledge: raw.knowledge,
            limits: raw.limits.unwrap_or_default(),
            cost: raw.cost.unwrap_or_default(),
            capabilities: CatalogCapabilities {
                attachment: raw.attachment,
                reasoning: raw.reasoning,
                tool_call: raw.tool_call,
                structured_output: raw.structured_output,
                temperature: raw.temperature,
            },
            modalities: raw.modalities.unwrap_or_default(),
            reasoning_options: raw.reasoning_options.unwrap_or_default(),
            interleaved: raw.interleaved,
            provider: raw.provider,
        }
    }

    /// Compare configured facts to this catalog model.
    #[must_use]
    pub fn compare_configured_facts(&self, configured: &ConfiguredModelFacts) -> Vec<CatalogDrift> {
        let mut drifts = Vec::new();

        compare_present(
            &mut drifts,
            CatalogDriftField::Status,
            configured.status.as_ref(),
            self.status.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::LimitContext,
            configured.limits.context.as_ref(),
            self.limits.context.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::LimitInput,
            configured.limits.input.as_ref(),
            self.limits.input.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::LimitOutput,
            configured.limits.output.as_ref(),
            self.limits.output.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::CapabilityAttachment,
            configured.capabilities.attachment.as_ref(),
            self.capabilities.attachment.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::CapabilityReasoning,
            configured.capabilities.reasoning.as_ref(),
            self.capabilities.reasoning.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::CapabilityToolCall,
            configured.capabilities.tool_call.as_ref(),
            self.capabilities.tool_call.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::CapabilityStructuredOutput,
            configured.capabilities.structured_output.as_ref(),
            self.capabilities.structured_output.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::CapabilityTemperature,
            configured.capabilities.temperature.as_ref(),
            self.capabilities.temperature.as_ref(),
        );
        compare_present(
            &mut drifts,
            CatalogDriftField::ProviderShape,
            configured.provider_shape.as_ref(),
            self.provider
                .as_ref()
                .and_then(|provider| provider.shape.as_ref()),
        );
        if let Some(cost) = &configured.cost {
            compare_present(
                &mut drifts,
                CatalogDriftField::CostInput,
                cost.input.as_ref(),
                self.cost.input.as_ref(),
            );
            compare_present(
                &mut drifts,
                CatalogDriftField::CostOutput,
                cost.output.as_ref(),
                self.cost.output.as_ref(),
            );
            compare_present(
                &mut drifts,
                CatalogDriftField::CostCacheRead,
                cost.cache_read.as_ref(),
                self.cost.cache_read.as_ref(),
            );
            compare_present(
                &mut drifts,
                CatalogDriftField::CostCacheWrite,
                cost.cache_write.as_ref(),
                self.cost.cache_write.as_ref(),
            );
        }

        compare_modalities(
            &mut drifts,
            CatalogDriftField::ModalityInput,
            configured.modalities.input.as_ref(),
            &self.modalities.input,
        );
        compare_modalities(
            &mut drifts,
            CatalogDriftField::ModalityOutput,
            configured.modalities.output.as_ref(),
            &self.modalities.output,
        );

        if let Some(configured_options) = &configured.reasoning_options {
            if !self.reasoning_options.is_empty() && configured_options != &self.reasoning_options {
                drifts.push(CatalogDrift::new(
                    CatalogDriftField::ReasoningOptions,
                    configured_options,
                    &self.reasoning_options,
                ));
            }
        }

        drifts
    }
}

/// Token limits for a model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogLimits {
    /// Total context window tokens.
    pub context: Option<u64>,
    /// Input token limit when separate from context.
    pub input: Option<u64>,
    /// Output token limit.
    pub output: Option<u64>,
}

/// Pricing facts for a model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogCost {
    /// Input token price.
    pub input: Option<f64>,
    /// Output token price.
    pub output: Option<f64>,
    /// Cache-read token price.
    pub cache_read: Option<f64>,
    /// Cache-write token price.
    pub cache_write: Option<f64>,
    /// Reasoning token price.
    pub reasoning: Option<f64>,
    /// Audio input price.
    pub input_audio: Option<f64>,
    /// Audio output price.
    pub output_audio: Option<f64>,
    /// Tiered pricing entries.
    #[serde(default)]
    pub tiers: Vec<CatalogCostTier>,
    /// Pricing that applies when context exceeds 200k tokens.
    pub context_over_200k: Option<CatalogCostRate>,
}

/// Pricing facts for one catalog tier.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogCostTier {
    /// Input token price.
    pub input: Option<f64>,
    /// Output token price.
    pub output: Option<f64>,
    /// Cache-read token price.
    pub cache_read: Option<f64>,
    /// Cache-write token price.
    pub cache_write: Option<f64>,
    /// Reasoning token price.
    pub reasoning: Option<f64>,
    /// Audio input price.
    pub input_audio: Option<f64>,
    /// Audio output price.
    pub output_audio: Option<f64>,
    /// Tier selector facts.
    pub tier: Option<CatalogCostTierInfo>,
}

/// Pricing facts reused for named cost tiers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogCostRate {
    /// Input token price.
    pub input: Option<f64>,
    /// Output token price.
    pub output: Option<f64>,
    /// Cache-read token price.
    pub cache_read: Option<f64>,
    /// Cache-write token price.
    pub cache_write: Option<f64>,
    /// Reasoning token price.
    pub reasoning: Option<f64>,
    /// Audio input price.
    pub input_audio: Option<f64>,
    /// Audio output price.
    pub output_audio: Option<f64>,
}

/// Selector attached to a cost tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCostTierInfo {
    /// Tier selector type.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Selector size value.
    pub size: Option<u64>,
}

/// Capability flags exposed by models.dev.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCapabilities {
    /// Attachment support.
    pub attachment: Option<bool>,
    /// Reasoning support.
    pub reasoning: Option<bool>,
    /// Tool-call support.
    pub tool_call: Option<bool>,
    /// Structured-output support.
    pub structured_output: Option<bool>,
    /// Temperature parameter support.
    pub temperature: Option<bool>,
}

/// Input and output modalities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModalities {
    /// Input modality names.
    #[serde(default)]
    pub input: Vec<String>,
    /// Output modality names.
    #[serde(default)]
    pub output: Vec<String>,
}

/// Model-level provider override facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModelProvider {
    /// Override API URL.
    pub api: Option<String>,
    /// Override AI SDK package name.
    pub npm: Option<String>,
    /// Override request shape.
    pub shape: Option<String>,
}

/// Interleaved thinking/tool content support details.
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogInterleaved {
    /// Missing or explicit null interleaved facts.
    Null,
    /// Boolean interleaved support flag.
    Boolean(bool),
    /// Object interleaved facts.
    Object(CatalogInterleavedObject),
    /// Unknown interleaved shape preserved from source JSON.
    Unknown(Value),
}

impl Default for CatalogInterleaved {
    fn default() -> Self {
        Self::Null
    }
}

impl Serialize for CatalogInterleaved {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Boolean(value) => value.serialize(serializer),
            Self::Object(value) => value.serialize(serializer),
            Self::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CatalogInterleaved {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Boolean(value),
            Value::Object(map) => {
                let field = match map.get("field") {
                    Some(Value::String(field)) => Some(field.clone()),
                    Some(Value::Null) | None => None,
                    _ => return Ok(Self::Unknown(Value::Object(map))),
                };
                Self::Object(CatalogInterleavedObject { field })
            }
            value => Self::Unknown(value),
        })
    }
}

/// Object form of interleaved support details.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogInterleavedObject {
    /// Field name carrying interleaved reasoning content.
    pub field: Option<String>,
}

/// Reasoning option exposed by the catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogReasoningOption {
    /// Reasoning effort values.
    Effort {
        /// Supported effort values.
        values: Vec<CatalogReasoningEffortValue>,
    },
    /// Boolean reasoning toggle.
    Toggle,
    /// Reasoning budget token range.
    BudgetTokens {
        /// Minimum budget tokens.
        min: Option<i64>,
        /// Maximum budget tokens.
        max: Option<i64>,
    },
    /// Unknown option preserved as raw JSON.
    Unknown(Value),
}

impl Serialize for CatalogReasoningOption {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Effort { values } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "effort")?;
                map.serialize_entry("values", values)?;
                map.end()
            }
            Self::Toggle => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "toggle")?;
                map.end()
            }
            Self::BudgetTokens { min, max } => {
                let mut len = 1;
                if min.is_some() {
                    len += 1;
                }
                if max.is_some() {
                    len += 1;
                }

                let mut map = serializer.serialize_map(Some(len))?;
                map.serialize_entry("type", "budget_tokens")?;
                if let Some(min) = min {
                    map.serialize_entry("min", min)?;
                }
                if let Some(max) = max {
                    map.serialize_entry("max", max)?;
                }
                map.end()
            }
            Self::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CatalogReasoningOption {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return Ok(Self::Unknown(value));
        };

        match kind {
            "effort" => Ok(Self::Effort {
                values: value
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .cloned()
                            .map(CatalogReasoningEffortValue::from)
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            "toggle" => Ok(Self::Toggle),
            "budget_tokens" => Ok(Self::BudgetTokens {
                min: value.get("min").and_then(Value::as_i64),
                max: value.get("max").and_then(Value::as_i64),
            }),
            _ => Ok(Self::Unknown(value)),
        }
    }
}

/// Reasoning effort value, including non-string values preserved from source JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CatalogReasoningEffortValue {
    /// Named effort value.
    String(String),
    /// Non-string value preserved from source JSON.
    Other(Value),
}

impl From<Value> for CatalogReasoningEffortValue {
    fn from(value: Value) -> Self {
        match value {
            Value::String(value) => Self::String(value),
            value => Self::Other(value),
        }
    }
}

/// Provider/model identity returned by catalog helpers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CatalogModelRef {
    /// Provider id.
    pub provider_id: String,
    /// Model id.
    pub model_id: String,
}

impl CatalogModelRef {
    /// Create a catalog model reference.
    #[must_use]
    pub fn new(provider_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }
    }
}

/// Conservative confidence for catalog matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogMatchConfidence {
    /// Exact provider id and model id match.
    Exact,
    /// High-confidence provider API URL and model id match.
    High,
    /// Suggested match from fuzzy/name similarity.
    Suggested,
}

/// Ranked catalog match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogMatch {
    /// Matched provider/model reference.
    pub model: CatalogModelRef,
    /// Conservative match confidence.
    pub confidence: CatalogMatchConfidence,
    /// Deterministic ranking score.
    pub score: u32,
    /// Short reason for the match.
    pub reason: String,
}

/// Lightweight configured-model facts used for drift comparison.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfiguredModelFacts {
    /// Configured status when known.
    pub status: Option<String>,
    /// Configured token limits.
    #[serde(default)]
    pub limits: CatalogLimits,
    /// Configured capabilities.
    #[serde(default)]
    pub capabilities: CatalogCapabilities,
    /// Configured modalities.
    #[serde(default)]
    pub modalities: ConfiguredModelModalities,
    /// Configured reasoning options.
    pub reasoning_options: Option<Vec<CatalogReasoningOption>>,
    /// Configured provider request shape.
    pub provider_shape: Option<String>,
    /// Configured model pricing.
    pub cost: Option<CatalogCost>,
}

/// Configured-model modality facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredModelModalities {
    /// Configured input modalities.
    pub input: Option<Vec<String>>,
    /// Configured output modalities.
    pub output: Option<Vec<String>>,
}

/// Field that drifted from catalog facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogDriftField {
    /// Status drift.
    Status,
    /// Context limit drift.
    LimitContext,
    /// Input limit drift.
    LimitInput,
    /// Output limit drift.
    LimitOutput,
    /// Attachment capability drift.
    CapabilityAttachment,
    /// Reasoning capability drift.
    CapabilityReasoning,
    /// Tool-call capability drift.
    CapabilityToolCall,
    /// Structured-output capability drift.
    CapabilityStructuredOutput,
    /// Temperature support drift.
    CapabilityTemperature,
    /// Input modality drift.
    ModalityInput,
    /// Output modality drift.
    ModalityOutput,
    /// Reasoning option drift.
    ReasoningOptions,
    /// Provider shape drift.
    ProviderShape,
    /// Input cost drift.
    CostInput,
    /// Output cost drift.
    CostOutput,
    /// Cache-read cost drift.
    CostCacheRead,
    /// Cache-write cost drift.
    CostCacheWrite,
}

/// One configured fact that differs from the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogDrift {
    /// Drift field.
    pub field: CatalogDriftField,
    /// Configured value.
    pub configured: Value,
    /// Catalog value.
    pub catalog: Value,
}

impl CatalogDrift {
    fn new<T, U>(field: CatalogDriftField, configured: &T, catalog: &U) -> Self
    where
        T: Serialize,
        U: Serialize,
    {
        Self {
            field,
            configured: json_value(configured),
            catalog: json_value(catalog),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCatalogProvider {
    id: Option<String>,
    name: Option<String>,
    api: Option<String>,
    npm: Option<String>,
    doc: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_vec")]
    env: Vec<String>,
    #[serde(default)]
    models: BTreeMap<String, RawCatalogModel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCatalogModel {
    id: Option<String>,
    name: Option<String>,
    family: Option<String>,
    status: Option<String>,
    release_date: Option<String>,
    last_updated: Option<String>,
    knowledge: Option<String>,
    #[serde(default, rename = "limit", alias = "limits")]
    limits: Option<CatalogLimits>,
    #[serde(default)]
    cost: Option<CatalogCost>,
    #[serde(default)]
    modalities: Option<CatalogModalities>,
    attachment: Option<bool>,
    reasoning: Option<bool>,
    tool_call: Option<bool>,
    structured_output: Option<bool>,
    temperature: Option<bool>,
    #[serde(default)]
    reasoning_options: Option<Vec<CatalogReasoningOption>>,
    #[serde(default)]
    interleaved: CatalogInterleaved,
    provider: Option<CatalogModelProvider>,
}

fn deserialize_string_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(Value::Array(values)) => values
            .into_iter()
            .filter_map(|value| match value {
                Value::String(value) => Some(value),
                _ => None,
            })
            .collect(),
        Some(Value::String(value)) => vec![value],
        _ => Vec::new(),
    })
}

fn sort_matches(matches: &mut [CatalogMatch]) {
    matches.sort_by(|left, right| {
        confidence_rank(right.confidence)
            .cmp(&confidence_rank(left.confidence))
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| left.model.cmp(&right.model))
    });
}

fn confidence_rank(confidence: CatalogMatchConfidence) -> u8 {
    match confidence {
        CatalogMatchConfidence::Exact => 3,
        CatalogMatchConfidence::High => 2,
        CatalogMatchConfidence::Suggested => 1,
    }
}

fn search_score(
    provider: &CatalogProvider,
    model: &CatalogModel,
    query: &str,
    query_tokens: &[String],
) -> u32 {
    let provider_id = normalize_key(&provider.id);
    let provider_name = normalize_key(&provider.name);
    let model_id = normalize_key(&model.id);
    let model_name = normalize_key(&model.name);
    let family = model.family.as_deref().map(normalize_key);
    let mut score = 0;

    if query == format!("{provider_id}/{model_id}") {
        score += 10_000;
    }
    if query == model_id {
        score += 7_000;
    }
    if query == model_name {
        score += 6_500;
    }
    if family.as_deref().is_some_and(|family| query == family) {
        score += 4_000;
    }
    if provider_id.contains(query) || provider_name.contains(query) {
        score += 600;
    }
    if model_id.contains(query) {
        score += 3_000;
    }
    if model_name.contains(query) {
        score += 2_500;
    }
    if family
        .as_deref()
        .is_some_and(|family| family.contains(query))
    {
        score += 1_500;
    }

    score += token_score(provider, model, query_tokens);
    score
}

fn token_score(provider: &CatalogProvider, model: &CatalogModel, query_tokens: &[String]) -> u32 {
    if query_tokens.is_empty() {
        return 0;
    }

    let provider_id = tokenize(&provider.id);
    let provider_name = tokenize(&provider.name);
    let model_id = tokenize(&model.id);
    let model_name = tokenize(&model.name);
    let family = model.family.as_deref().map(tokenize).unwrap_or_default();
    let mut score = 0;

    for token in query_tokens {
        let token_score = if provider_id.iter().any(|value| value == token) {
            220
        } else if provider_name.iter().any(|value| value == token) {
            180
        } else if model_id.iter().any(|value| value == token) {
            140
        } else if model_name.iter().any(|value| value == token) {
            120
        } else if family.iter().any(|value| value == token) {
            90
        } else {
            return 0;
        };
        score += token_score;
    }

    score
}

fn suggestion_score(
    provider: &CatalogProvider,
    model: &CatalogModel,
    model_key: &str,
    model_tokens: &[String],
    provider_match: bool,
    api_match: bool,
) -> u32 {
    let model_id = normalize_key(&model.id);
    let model_name = normalize_key(&model.name);
    let family = model.family.as_deref().map(normalize_key);
    let mut score = 0;

    if model_key == model_id {
        score += 5_000;
    }
    if model_key == model_name {
        score += 4_500;
    }
    if model_id.contains(model_key) {
        score += 2_500;
    }
    if model_name.contains(model_key) {
        score += 2_200;
    }
    if family
        .as_deref()
        .is_some_and(|family| family.contains(model_key))
    {
        score += 1_200;
    }
    if token_score(provider, model, model_tokens) > 0 {
        score += 1_000;
    }
    if provider_match {
        score += 900;
    }
    if api_match {
        score += 800;
    }

    score
}

fn provider_url_matches(provider: &CatalogProvider, model: &CatalogModel, url: &str) -> bool {
    provider
        .api
        .as_deref()
        .is_some_and(|api| normalize_url(api) == url)
        || model
            .provider
            .as_ref()
            .and_then(|provider| provider.api.as_deref())
            .is_some_and(|api| normalize_url(api) == url)
}

fn compare_present<T>(
    drifts: &mut Vec<CatalogDrift>,
    field: CatalogDriftField,
    configured: Option<&T>,
    catalog: Option<&T>,
) where
    T: PartialEq + Serialize,
{
    if let (Some(configured), Some(catalog)) = (configured, catalog) {
        if configured != catalog {
            drifts.push(CatalogDrift::new(field, configured, catalog));
        }
    }
}

fn compare_modalities(
    drifts: &mut Vec<CatalogDrift>,
    field: CatalogDriftField,
    configured: Option<&Vec<String>>,
    catalog: &[String],
) {
    let Some(configured) = configured else {
        return;
    };
    if catalog.is_empty() {
        return;
    }

    let configured = normalized_values(configured);
    let catalog = normalized_values(catalog);
    if configured.cmp(&catalog) != Ordering::Equal {
        drifts.push(CatalogDrift::new(field, &configured, &catalog));
    }
}

fn normalized_values(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| normalize_key(value))
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn json_value<T>(value: &T) -> Value
where
    T: Serialize,
{
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn normalize_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_url(value: &str) -> String {
    normalize_key(value).trim_end_matches('/').to_owned()
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
    {
      "anthropic": {
        "id": "anthropic",
        "name": "Anthropic",
        "api": "https://api.anthropic.com/v1",
        "npm": "@ai-sdk/anthropic",
        "doc": "https://docs.anthropic.com/",
        "env": ["ANTHROPIC_API_KEY"],
        "unknown_provider_field": true,
        "models": {
          "claude-sonnet-4-5": {
            "id": "claude-sonnet-4-5",
            "name": "Claude Sonnet 4.5",
            "family": "claude-sonnet",
            "status": "stable",
            "attachment": true,
            "reasoning": true,
            "tool_call": true,
            "structured_output": true,
            "temperature": true,
            "knowledge": "2025-07-31",
            "release_date": "2025-09-29",
            "last_updated": "2025-09-29",
            "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]},
            "limit": {"context": 200000, "output": 64000},
            "cost": {
              "input": 3,
              "output": 15,
              "cache_read": 0.3,
              "cache_write": 3.75,
              "tiers": [{"input": 6, "output": 22.5, "tier": {"type": "context", "size": 200000}}],
              "context_over_200k": {"input": 6, "output": 22.5, "cache_read": 0.6}
            },
            "reasoning_options": [{"type": "budget_tokens", "min": 1024, "max": 64000}],
            "interleaved": true
          }
        }
      },
      "openai": {
        "id": "openai",
        "name": "OpenAI",
        "api": "https://api.openai.com/v1",
        "npm": "@ai-sdk/openai",
        "env": ["OPENAI_API_KEY"],
        "models": {
          "gpt-4o": {
            "id": "gpt-4o",
            "name": "GPT-4o",
            "family": "gpt",
            "status": "stable",
            "attachment": true,
            "reasoning": false,
            "tool_call": true,
            "structured_output": true,
            "temperature": true,
            "modalities": {"input": ["text", "image", "audio"], "output": ["text", "audio"]},
            "limit": {"context": 128000, "input": 120000, "output": 16384},
            "cost": {"input": 2.5, "output": 10, "input_audio": 40, "output_audio": 80},
            "top_p": 1,
            "top_k": 40,
            "temperature_default": 1,
            "presence_penalty": 0
          },
          "gpt-5": {
            "id": "gpt-5",
            "name": "GPT-5",
            "family": "gpt",
            "status": "preview",
            "attachment": true,
            "reasoning": true,
            "tool_call": true,
            "structured_output": true,
            "temperature": false,
            "modalities": {"input": ["text", "image"], "output": ["text"]},
            "limit": {"context": 400000, "output": 128000},
            "reasoning_options": [
              {"type": "effort", "values": ["minimal", "low", "medium", "high", null]},
              {"type": "toggle"},
              {"type": "vendor_extension", "value": true}
            ]
          }
        }
      },
      "gateway": {
        "id": "gateway",
        "name": "Example Gateway",
        "api": "https://gateway.example/v1/",
        "npm": "@ai-sdk/openai-compatible",
        "doc": "https://gateway.example/docs",
        "env": ["GATEWAY_API_KEY"],
        "models": {
          "openai/gpt-4o": {
            "id": "openai/gpt-4o",
            "name": "GPT-4o via Gateway",
            "attachment": true,
            "reasoning": false,
            "tool_call": true,
            "temperature": true,
            "modalities": {"input": ["text", "image"], "output": ["text"]},
            "limit": {"context": 128000, "output": 16384},
            "provider": {"npm": "@ai-sdk/openai-compatible", "api": "https://gateway.example/openai/v1", "shape": "responses"},
            "interleaved": {"field": "reasoning_content"},
            "reasoning_options": []
          }
        }
      }
    }
    "#;

    fn fixture_catalog() -> ModelCatalog {
        ModelCatalog::from_json_str(FIXTURE).expect("fixture parses")
    }

    #[test]
    fn catalog_parses_models_dev_shape() {
        let catalog = fixture_catalog();
        let anthropic = catalog.provider("anthropic").expect("provider exists");
        assert_eq!(anthropic.name, "Anthropic");
        assert_eq!(anthropic.env, vec!["ANTHROPIC_API_KEY"]);

        let model = catalog
            .model("anthropic", "claude-sonnet-4-5")
            .expect("model exists");
        assert_eq!(model.family.as_deref(), Some("claude-sonnet"));
        assert_eq!(model.status.as_deref(), Some("stable"));
        assert_eq!(model.release_date.as_deref(), Some("2025-09-29"));
        assert_eq!(model.limits.context, Some(200_000));
        assert_eq!(model.cost.input, Some(3.0));
        assert_eq!(model.cost.cache_write, Some(3.75));
        assert_eq!(
            model.cost.tiers[0].tier.as_ref().and_then(|tier| tier.size),
            Some(200_000)
        );
        assert_eq!(
            model
                .cost
                .context_over_200k
                .as_ref()
                .and_then(|cost| cost.output),
            Some(22.5)
        );
        assert_eq!(model.capabilities.structured_output, Some(true));
        assert_eq!(model.modalities.input, vec!["text", "image", "pdf"]);
        assert_eq!(model.interleaved, CatalogInterleaved::Boolean(true));

        let gateway_model = catalog
            .model("gateway", "openai/gpt-4o")
            .expect("gateway model exists");
        assert_eq!(
            gateway_model.interleaved,
            CatalogInterleaved::Object(CatalogInterleavedObject {
                field: Some("reasoning_content".to_owned())
            })
        );
    }

    #[test]
    fn catalog_exact_lookup_uses_provider_and_model_ids() {
        let catalog = fixture_catalog();
        assert!(catalog.model("openai", "gpt-4o").is_some());
        assert!(catalog.model("openai", "openai/gpt-4o").is_none());
        assert!(catalog.model("missing", "gpt-4o").is_none());
    }

    #[test]
    fn catalog_interleaved_parses_null_and_unknown_safely() {
        let catalog = ModelCatalog::from_json_str(
            r#"
            {
              "provider": {
                "models": {
                  "missing": {"name": "Missing Interleaved"},
                  "null": {"name": "Null Interleaved", "interleaved": null},
                  "unknown": {"name": "Unknown Interleaved", "interleaved": ["unexpected"]}
                }
              }
            }
            "#,
        )
        .expect("catalog parses tolerant interleaved shapes");

        assert_eq!(
            catalog
                .model("provider", "missing")
                .expect("model exists")
                .interleaved,
            CatalogInterleaved::Null
        );
        assert_eq!(
            catalog
                .model("provider", "null")
                .expect("model exists")
                .interleaved,
            CatalogInterleaved::Null
        );
        assert_eq!(
            catalog
                .model("provider", "unknown")
                .expect("model exists")
                .interleaved,
            CatalogInterleaved::Unknown(Value::Array(vec![Value::String("unexpected".to_owned())]))
        );
    }

    #[test]
    fn catalog_search_is_ranked_and_deterministic() {
        let catalog = fixture_catalog();
        let matches = catalog.search("openai gpt-4o", 3);
        assert_eq!(matches[0].model, CatalogModelRef::new("openai", "gpt-4o"));
        assert_eq!(matches[0].confidence, CatalogMatchConfidence::Suggested);

        let exact = catalog.search("openai/gpt-4o", 1);
        assert_eq!(exact[0].model, CatalogModelRef::new("openai", "gpt-4o"));
        assert_eq!(exact[0].confidence, CatalogMatchConfidence::Suggested);

        let model_only = catalog.search("gpt-4o", 3);
        assert!(
            model_only
                .iter()
                .all(|item| item.confidence == CatalogMatchConfidence::Suggested)
        );
    }

    #[test]
    fn catalog_suggestions_are_conservative() {
        let catalog = fixture_catalog();

        let exact = catalog.suggest_matches(Some("openai"), None, "gpt-4o", 3);
        assert_eq!(exact[0].model, CatalogModelRef::new("openai", "gpt-4o"));
        assert_eq!(exact[0].confidence, CatalogMatchConfidence::Exact);

        let by_url =
            catalog.suggest_matches(None, Some("https://gateway.example/v1"), "openai/gpt-4o", 3);
        assert_eq!(
            by_url[0].model,
            CatalogModelRef::new("gateway", "openai/gpt-4o")
        );
        assert_eq!(by_url[0].confidence, CatalogMatchConfidence::High);

        let fuzzy = catalog.suggest_matches(Some("openai"), None, "GPT-4o", 3);
        assert!(
            fuzzy
                .iter()
                .any(|item| item.model == CatalogModelRef::new("openai", "gpt-4o"))
        );
        assert!(
            fuzzy
                .iter()
                .all(|item| item.confidence == CatalogMatchConfidence::Suggested)
        );
    }

    #[test]
    fn catalog_reasoning_options_parse_known_and_unknown_variants() {
        let catalog = fixture_catalog();
        let model = catalog.model("openai", "gpt-5").expect("model exists");

        assert_eq!(
            model.reasoning_options[0],
            CatalogReasoningOption::Effort {
                values: vec![
                    CatalogReasoningEffortValue::String("minimal".to_owned()),
                    CatalogReasoningEffortValue::String("low".to_owned()),
                    CatalogReasoningEffortValue::String("medium".to_owned()),
                    CatalogReasoningEffortValue::String("high".to_owned()),
                    CatalogReasoningEffortValue::Other(Value::Null),
                ]
            }
        );
        assert_eq!(model.reasoning_options[1], CatalogReasoningOption::Toggle);
        assert!(matches!(
            model.reasoning_options[2],
            CatalogReasoningOption::Unknown(_)
        ));

        let anthropic = catalog
            .model("anthropic", "claude-sonnet-4-5")
            .expect("model exists");
        assert_eq!(
            anthropic.reasoning_options[0],
            CatalogReasoningOption::BudgetTokens {
                min: Some(1024),
                max: Some(64_000)
            }
        );
    }

    #[test]
    fn catalog_drift_reports_supported_facts_and_ignores_sampling_defaults() {
        let catalog = fixture_catalog();
        let model = catalog.model("openai", "gpt-4o").expect("model exists");

        let matching = ConfiguredModelFacts {
            status: Some("stable".to_owned()),
            limits: CatalogLimits {
                context: Some(128_000),
                input: Some(120_000),
                output: Some(16_384),
            },
            capabilities: CatalogCapabilities {
                attachment: Some(true),
                reasoning: Some(false),
                tool_call: Some(true),
                structured_output: Some(true),
                temperature: Some(true),
            },
            modalities: ConfiguredModelModalities {
                input: Some(vec![
                    "audio".to_owned(),
                    "image".to_owned(),
                    "text".to_owned(),
                ]),
                output: Some(vec!["audio".to_owned(), "text".to_owned()]),
            },
            cost: None,
            reasoning_options: None,
            provider_shape: None,
        };
        assert!(model.compare_configured_facts(&matching).is_empty());

        let drifting = ConfiguredModelFacts {
            limits: CatalogLimits {
                context: Some(64_000),
                ..CatalogLimits::default()
            },
            capabilities: CatalogCapabilities {
                tool_call: Some(false),
                ..CatalogCapabilities::default()
            },
            modalities: ConfiguredModelModalities {
                input: Some(vec!["text".to_owned()]),
                output: None,
            },
            ..ConfiguredModelFacts::default()
        };
        let drifts = model.compare_configured_facts(&drifting);
        assert_eq!(
            drifts.iter().map(|drift| drift.field).collect::<Vec<_>>(),
            vec![
                CatalogDriftField::LimitContext,
                CatalogDriftField::CapabilityToolCall,
                CatalogDriftField::ModalityInput,
            ]
        );
    }

    #[test]
    fn catalog_drift_includes_reasoning_provider_shape_and_status() {
        let catalog = fixture_catalog();
        let model = catalog
            .model("gateway", "openai/gpt-4o")
            .expect("model exists");

        let configured = ConfiguredModelFacts {
            status: Some("deprecated".to_owned()),
            reasoning_options: Some(vec![CatalogReasoningOption::Toggle]),
            provider_shape: Some("chat".to_owned()),
            ..ConfiguredModelFacts::default()
        };
        let drifts = model.compare_configured_facts(&configured);
        assert_eq!(
            drifts.iter().map(|drift| drift.field).collect::<Vec<_>>(),
            vec![CatalogDriftField::ProviderShape]
        );

        let model = catalog.model("openai", "gpt-5").expect("model exists");
        let configured = ConfiguredModelFacts {
            status: Some("stable".to_owned()),
            reasoning_options: Some(vec![CatalogReasoningOption::Toggle]),
            ..ConfiguredModelFacts::default()
        };
        let drifts = model.compare_configured_facts(&configured);
        assert_eq!(
            drifts.iter().map(|drift| drift.field).collect::<Vec<_>>(),
            vec![
                CatalogDriftField::Status,
                CatalogDriftField::ReasoningOptions,
            ]
        );
    }
}
