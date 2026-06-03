use serde_json::{Value, json};

use crate::{ThinkingBlock, ThinkingBlockKind};

/// Anthropic Messages thinking configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicThinkingConfig {
    /// Manual thinking budget mode.
    Enabled {
        /// Maximum thinking tokens. Must be less than `max_tokens`.
        budget_tokens: u32,
        /// Optional display mode.
        display: Option<AnthropicThinkingDisplay>,
    },
    /// Adaptive thinking mode used by newer Claude models.
    Adaptive {
        /// Optional display mode.
        display: Option<AnthropicThinkingDisplay>,
    },
}

/// Anthropic thinking display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingDisplay {
    /// Stream summarized thinking when available.
    Summarized,
    /// Omit thinking text while preserving signatures for continuation.
    Omitted,
}

pub(super) fn anthropic_thinking(config: &AnthropicThinkingConfig) -> Value {
    let mut thinking = match config {
        AnthropicThinkingConfig::Enabled {
            budget_tokens,
            display: _,
        } => json!({ "type": "enabled", "budget_tokens": budget_tokens }),
        AnthropicThinkingConfig::Adaptive { display: _ } => json!({ "type": "adaptive" }),
    };
    let display = match config {
        AnthropicThinkingConfig::Enabled { display, .. }
        | AnthropicThinkingConfig::Adaptive { display } => *display,
    };
    if let Some(display) = display {
        thinking["display"] = json!(match display {
            AnthropicThinkingDisplay::Summarized => "summarized",
            AnthropicThinkingDisplay::Omitted => "omitted",
        });
    }
    thinking
}

#[derive(Debug, Default)]
pub(super) struct AnthropicThinkingBlock {
    pub(super) text: String,
    pub(super) signature: Option<String>,
}

impl AnthropicThinkingBlock {
    pub(super) fn into_event(self) -> ThinkingBlock {
        let mut payload = json!({
            "type": "thinking",
            "thinking": self.text,
        });
        if let Some(signature) = self.signature {
            payload["signature"] = json!(signature);
        }
        ThinkingBlock {
            kind: ThinkingBlockKind::Anthropic,
            text: payload["thinking"].as_str().unwrap_or_default().to_owned(),
            payload,
        }
    }
}
