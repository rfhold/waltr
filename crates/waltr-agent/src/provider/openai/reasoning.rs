//! `OpenAI` reasoning configuration.

use serde_json::{Value, json};

/// `OpenAI` Responses reasoning configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiReasoningConfig {
    /// Reasoning effort.
    pub effort: OpenAiReasoningEffort,
    /// Optional reasoning summary mode.
    pub summary: Option<OpenAiReasoningSummary>,
}

/// `OpenAI` Responses reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiReasoningEffort {
    /// Disable reasoning.
    None,
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    XHigh,
}

/// `OpenAI` Responses reasoning summary mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiReasoningSummary {
    /// Let `OpenAI` automatically summarize reasoning when supported.
    Auto,
}

pub(super) fn openai_reasoning(config: &OpenAiReasoningConfig) -> Value {
    let mut reasoning = json!({
        "effort": match config.effort {
            OpenAiReasoningEffort::None => "none",
            OpenAiReasoningEffort::Minimal => "minimal",
            OpenAiReasoningEffort::Low => "low",
            OpenAiReasoningEffort::Medium => "medium",
            OpenAiReasoningEffort::High => "high",
            OpenAiReasoningEffort::XHigh => "xhigh",
        }
    });
    if let Some(summary) = config.summary {
        reasoning["summary"] = json!(match summary {
            OpenAiReasoningSummary::Auto => "auto",
        });
    }
    reasoning
}
