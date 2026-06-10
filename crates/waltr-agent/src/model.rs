//! Provider and model identity types.

use serde::{Deserialize, Serialize};

/// Provider identifier used for provider-aware behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderId {
    /// Anthropic provider.
    Anthropic,
    /// `OpenAI` provider.
    OpenAi,
}

/// Model API surface used by the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelApi {
    /// Anthropic Messages API.
    AnthropicMessages,
    /// `OpenAI` Chat Completions API.
    OpenAiChatCompletions,
    /// `OpenAI` Responses API over HTTP.
    OpenAiResponses,
    /// `OpenAI` Responses API over WebSocket.
    OpenAiResponsesWebSocket,
}

/// Model identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelId(String);

impl ModelId {
    /// Create a model identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the model identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Provider, model, and API identity used while configuring tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider that owns the model.
    pub provider: ProviderId,
    /// Provider model identifier.
    pub model: ModelId,
    /// API surface used for calls to this model.
    pub api: ModelApi,
}

impl ModelRef {
    /// Create model identity metadata.
    pub fn new(provider: ProviderId, model: impl Into<ModelId>, api: ModelApi) -> Self {
        Self {
            provider,
            model: model.into(),
            api,
        }
    }
}
