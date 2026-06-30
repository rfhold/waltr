//! `OpenAI` Responses streaming providers.

mod http;
pub(in crate::provider::openai) mod parser;
pub(in crate::provider::openai) mod request;
mod websocket;

pub use websocket::OpenAiResponsesWebSocket;

use super::reasoning::OpenAiReasoningConfig;

/// `OpenAI` Responses HTTP streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiResponses {
    headers: Vec<(String, String)>,
    model: String,
    responses_url: String,
    reasoning: Option<OpenAiReasoningConfig>,
    prompt_cache: bool,
    prompt_cache_retention: Option<String>,
}

impl OpenAiResponses {
    /// Create an `OpenAI` Responses provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            headers: Vec::new(),
            model: model.into(),
            responses_url: "https://api.openai.com/v1/responses".to_owned(),
            reasoning: None,
            prompt_cache: true,
            prompt_cache_retention: None,
        }
    }

    /// Set the exact Responses endpoint URL used by this provider.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.responses_url = url.into();
        self
    }

    /// Add a request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Enable `OpenAI` Responses reasoning for requests from this provider.
    #[must_use]
    pub const fn with_reasoning(mut self, config: OpenAiReasoningConfig) -> Self {
        self.reasoning = Some(config);
        self
    }

    /// Set the OpenAI prompt cache retention hint for outgoing requests.
    #[must_use]
    pub fn with_prompt_cache_retention(mut self, retention: impl Into<String>) -> Self {
        self.prompt_cache_retention = Some(retention.into());
        self
    }

    /// Disable OpenAI prompt cache routing hints on outgoing requests.
    #[must_use]
    pub const fn without_prompt_cache(mut self) -> Self {
        self.prompt_cache = false;
        self
    }
}
