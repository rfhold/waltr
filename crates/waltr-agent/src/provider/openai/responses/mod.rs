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
    headers: Vec<(&'static str, String)>,
    model: String,
    responses_url: String,
    reasoning: Option<OpenAiReasoningConfig>,
}

impl OpenAiResponses {
    /// Create an `OpenAI` Responses provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            headers: Vec::new(),
            model: model.into(),
            responses_url: "https://api.openai.com/v1/responses".to_owned(),
            reasoning: None,
        }
    }

    /// Set the API base URL used by this provider.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        self.responses_url = format!("{}/v1/responses", base_url.as_ref().trim_end_matches('/'));
        self
    }

    /// Set the exact Responses endpoint URL used by this provider.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.responses_url = url.into();
        self
    }

    /// Add a static request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Enable `OpenAI` Responses reasoning for requests from this provider.
    #[must_use]
    pub const fn with_reasoning(mut self, config: OpenAiReasoningConfig) -> Self {
        self.reasoning = Some(config);
        self
    }
}
