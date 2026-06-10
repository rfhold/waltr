//! Anthropic Messages streaming provider.

mod parser;
mod request;
mod stream;
mod thinking;

use serde_json::Value;

use crate::{CompletionRequest, LlmStream, stream::BoxStreamFuture};

pub use thinking::{AnthropicThinkingConfig, AnthropicThinkingDisplay};

const PROVIDER: &str = "anthropic";
const ANTHROPIC_MESSAGES_NAME: &str = "anthropic_messages";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MAX_TOKENS: u32 = 256;

/// Anthropic Messages API streaming provider.
#[derive(Debug, Clone)]
pub struct AnthropicMessages {
    api_key: String,
    model: String,
    messages_url: String,
    max_tokens: u32,
    thinking: Option<AnthropicThinkingConfig>,
    client: reqwest::Client,
}

impl AnthropicMessages {
    /// Create a Messages API provider for the given API key and model.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            messages_url: ANTHROPIC_MESSAGES_URL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: None,
            client: reqwest::Client::new(),
        }
    }

    /// Set the API base URL used by this provider.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        self.messages_url = format!("{}/v1/messages", base_url.as_ref().trim_end_matches('/'));
        self
    }

    /// Set the maximum number of tokens to sample.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Enable Anthropic thinking for requests from this provider.
    #[must_use]
    pub const fn with_thinking(mut self, config: AnthropicThinkingConfig) -> Self {
        self.thinking = Some(config);
        self
    }
}

impl LlmStream for AnthropicMessages {
    fn name(&self) -> &'static str {
        ANTHROPIC_MESSAGES_NAME
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let request =
            request.for_provider_identity(ANTHROPIC_MESSAGES_NAME, &self.messages_url, &self.model);
        Some(request::messages_request(
            &self.model,
            self.max_tokens,
            self.thinking.as_ref(),
            &request,
        ))
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(stream::send_request(self, request))
    }
}
