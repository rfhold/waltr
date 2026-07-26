//! Stable shell-facing model catalog views.

use serde::Serialize;
use ts_rs::TS;

use crate::{
    CatalogMatch, CatalogMatchConfidence, CatalogModelCapabilitySummary, CatalogModelCostSummary,
    CatalogModelLimitsSummary, CatalogModelReasoningSummary, CatalogModelRef, CatalogModelSummary,
};

/// Stable model catalog identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelRefView {
    /// Catalog provider identity.
    pub provider_id: String,
    /// Catalog model identity.
    pub model_id: String,
}

/// Stable model token-limit summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelLimitsSummaryView {
    /// Maximum context tokens.
    pub context: Option<u32>,
    /// Maximum output tokens.
    pub output: Option<u32>,
}

/// Stable model cost summary.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelCostSummaryView {
    /// Input cost per million tokens.
    pub input_per_million_tokens: Option<f64>,
    /// Output cost per million tokens.
    pub output_per_million_tokens: Option<f64>,
    /// Cache-read cost per million tokens.
    pub cache_read_per_million_tokens: Option<f64>,
    /// Cache-write cost per million tokens.
    pub cache_write_per_million_tokens: Option<f64>,
    /// Image-input cost per million tokens.
    pub image_input_per_million_tokens: Option<f64>,
    /// Image-input cost per image.
    pub image_input_per_image: Option<f64>,
    /// Cost currency.
    pub currency: String,
}

/// Stable model capability summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelCapabilitySummaryView {
    /// Whether tools are supported.
    pub tools: bool,
    /// Whether image input is supported.
    pub image: bool,
    /// Whether audio input is supported.
    pub audio: bool,
}

/// Stable model reasoning summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelReasoningSummaryView {
    /// Whether reasoning is supported.
    pub supported: bool,
    /// Default reasoning effort.
    pub effort: Option<String>,
    /// Available reasoning efforts.
    pub efforts: Vec<String>,
}

/// Stable model catalog search result.
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModelSummaryView {
    /// Catalog model identity.
    pub model: Option<CatalogModelRefView>,
    /// Provider display name.
    pub provider_name: String,
    /// Model display name.
    pub model_name: String,
    /// Model family.
    pub family: Option<String>,
    /// Catalog lifecycle status.
    pub status: Option<String>,
    /// Token limits.
    pub limits: Option<CatalogModelLimitsSummaryView>,
    /// Token and image costs.
    pub cost: Option<CatalogModelCostSummaryView>,
    /// Input and tool capabilities.
    pub capabilities: Option<CatalogModelCapabilitySummaryView>,
    /// Reasoning support.
    pub reasoning: Option<CatalogModelReasoningSummaryView>,
}

/// Stable catalog match confidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum CatalogMatchConfidenceView {
    /// Confidence was not specified.
    Unspecified,
    /// Exact identity match.
    Exact,
    /// High-confidence inferred match.
    High,
    /// Lower-confidence suggestion.
    Suggested,
    /// Unknown protocol value.
    Unknown,
}

/// Stable suggested catalog match.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, Debug, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CatalogMatchView {
    /// Matched catalog model.
    pub model: Option<CatalogModelRefView>,
    /// Provider display name.
    pub provider_name: String,
    /// Model display name.
    pub model_name: String,
    /// Match confidence.
    pub confidence: CatalogMatchConfidenceView,
    /// Human-readable match reasons.
    pub reasons: Vec<String>,
    /// Upstream model identity when a gateway model was matched.
    pub upstream_model: Option<CatalogModelRefView>,
}

impl From<CatalogModelRef> for CatalogModelRefView {
    fn from(model: CatalogModelRef) -> Self {
        Self {
            provider_id: model.provider_id,
            model_id: model.model_id,
        }
    }
}

impl From<CatalogModelLimitsSummary> for CatalogModelLimitsSummaryView {
    fn from(limits: CatalogModelLimitsSummary) -> Self {
        Self {
            context: limits.context,
            output: limits.output,
        }
    }
}

impl From<CatalogModelCostSummary> for CatalogModelCostSummaryView {
    fn from(cost: CatalogModelCostSummary) -> Self {
        Self {
            input_per_million_tokens: cost.input_per_million_tokens,
            output_per_million_tokens: cost.output_per_million_tokens,
            cache_read_per_million_tokens: cost.cache_read_per_million_tokens,
            cache_write_per_million_tokens: cost.cache_write_per_million_tokens,
            image_input_per_million_tokens: cost.image_input_per_million_tokens,
            image_input_per_image: cost.image_input_per_image,
            currency: cost.currency,
        }
    }
}

impl From<CatalogModelCapabilitySummary> for CatalogModelCapabilitySummaryView {
    fn from(capabilities: CatalogModelCapabilitySummary) -> Self {
        Self {
            tools: capabilities.tools,
            image: capabilities.image,
            audio: capabilities.audio,
        }
    }
}

impl From<CatalogModelReasoningSummary> for CatalogModelReasoningSummaryView {
    fn from(reasoning: CatalogModelReasoningSummary) -> Self {
        Self {
            supported: reasoning.supported,
            effort: reasoning.effort,
            efforts: reasoning.efforts,
        }
    }
}

impl From<CatalogModelSummary> for CatalogModelSummaryView {
    fn from(summary: CatalogModelSummary) -> Self {
        Self {
            model: summary.model.map(Into::into),
            provider_name: summary.provider_name,
            model_name: summary.model_name,
            family: summary.family,
            status: summary.status,
            limits: summary.limits.map(Into::into),
            cost: summary.cost.map(Into::into),
            capabilities: summary.capabilities.map(Into::into),
            reasoning: summary.reasoning.map(Into::into),
        }
    }
}

impl From<CatalogMatch> for CatalogMatchView {
    fn from(catalog_match: CatalogMatch) -> Self {
        Self {
            model: catalog_match.model.map(Into::into),
            provider_name: catalog_match.provider_name,
            model_name: catalog_match.model_name,
            confidence: CatalogMatchConfidence::try_from(catalog_match.confidence)
                .map_or(CatalogMatchConfidenceView::Unknown, Into::into),
            reasons: catalog_match.reasons,
            upstream_model: catalog_match.upstream_model.map(Into::into),
        }
    }
}

impl From<CatalogMatchConfidence> for CatalogMatchConfidenceView {
    fn from(confidence: CatalogMatchConfidence) -> Self {
        match confidence {
            CatalogMatchConfidence::Unspecified => Self::Unspecified,
            CatalogMatchConfidence::Exact => Self::Exact,
            CatalogMatchConfidence::High => Self::High,
            CatalogMatchConfidence::Suggested => Self::Suggested,
        }
    }
}
