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
    headers: Vec<(String, String)>,
    model: String,
    messages_url: String,
    max_tokens: u32,
    thinking: Option<AnthropicThinkingConfig>,
    prompt_cache: bool,
    client: reqwest::Client,
}

impl AnthropicMessages {
    /// Create a Messages API provider for the given model.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            headers: Vec::new(),
            model: model.into(),
            messages_url: ANTHROPIC_MESSAGES_URL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: None,
            prompt_cache: true,
            client: reqwest::Client::new(),
        }
    }

    /// Set the exact Messages endpoint URL used by this provider.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.messages_url = url.into();
        self
    }

    /// Set the maximum number of tokens to sample.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Add a request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Enable Anthropic thinking for requests from this provider.
    #[must_use]
    pub const fn with_thinking(mut self, config: AnthropicThinkingConfig) -> Self {
        self.thinking = Some(config);
        self
    }

    /// Disable Anthropic prompt cache control on outgoing requests.
    #[must_use]
    pub const fn without_prompt_cache(mut self) -> Self {
        self.prompt_cache = false;
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
            self.prompt_cache,
            &request,
        ))
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(stream::send_request(self, request))
    }
}
