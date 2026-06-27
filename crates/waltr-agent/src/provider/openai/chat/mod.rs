//! `OpenAI` Chat Completions streaming provider.

mod parser;
mod request;

use serde_json::Value;

use crate::stream::{CompletionRequest, LlmStream};

use self::parser::ChatParser;
use self::request::chat_body;
use super::shared::{CHAT_COMPLETIONS_NAME, stream_openai_http};

/// `OpenAI` Chat Completions HTTP streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiChatCompletions {
    headers: Vec<(&'static str, String)>,
    model: String,
    completions_url: String,
}

impl OpenAiChatCompletions {
    /// Create an `OpenAI` Chat Completions provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            headers: Vec::new(),
            model: model.into(),
            completions_url: "https://api.openai.com/v1/chat/completions".to_owned(),
        }
    }

    /// Set the API base URL used by this provider.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Self {
        self.completions_url = format!(
            "{}/v1/chat/completions",
            base_url.as_ref().trim_end_matches('/')
        );
        self
    }

    /// Add a static request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

impl LlmStream for OpenAiChatCompletions {
    fn name(&self) -> &'static str {
        CHAT_COMPLETIONS_NAME
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let request = request.for_provider_identity(
            CHAT_COMPLETIONS_NAME,
            &self.completions_url,
            &self.model,
        );
        Some(chat_body(&self.model, &request))
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let request = request.for_provider_identity(
                CHAT_COMPLETIONS_NAME,
                &self.completions_url,
                &self.model,
            );
            let body = chat_body(&self.model, &request);
            let mut parser =
                ChatParser::new(CHAT_COMPLETIONS_NAME, &self.completions_url, &self.model);

            stream_openai_http(
                CHAT_COMPLETIONS_NAME,
                &self.completions_url,
                body,
                self.headers.clone(),
                move |data| parser.parse_event(data),
            )
            .await
        })
    }
}
