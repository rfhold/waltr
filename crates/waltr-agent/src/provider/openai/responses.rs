//! `OpenAI` Responses HTTP streaming provider.

mod http;
mod parser;
mod request;
mod websocket;

use crate::stream::LlmStream;

use crate::stream::CompletionRequest;

use super::{
    OpenAiReasoningConfig, RESPONSES_NAME, ResponsesParser, responses_body, stream_openai_http,
};

/// `OpenAI` Responses HTTP streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiResponses {
    api_key: String,
    model: String,
    responses_url: String,
    reasoning: Option<OpenAiReasoningConfig>,
}

impl OpenAiResponses {
    /// Create an `OpenAI` Responses provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
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

    /// Enable `OpenAI` Responses reasoning for requests from this provider.
    #[must_use]
    pub const fn with_reasoning(mut self, config: OpenAiReasoningConfig) -> Self {
        self.reasoning = Some(config);
        self
    }
}

impl LlmStream for OpenAiResponses {
    fn name(&self) -> &'static str {
        RESPONSES_NAME
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let mut parser = ResponsesParser::default();
            stream_openai_http(
                RESPONSES_NAME,
                &self.responses_url,
                &self.api_key,
                responses_body(&self.model, self.reasoning.as_ref(), &request),
                move |data| parser.parse_event(data),
            )
            .await
        })
    }
}
