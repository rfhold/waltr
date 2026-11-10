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
    headers: Vec<(String, String)>,
    model: String,
    completions_url: String,
    prompt_cache: bool,
    prompt_cache_retention: Option<String>,
}

impl OpenAiChatCompletions {
    /// Create an `OpenAI` Chat Completions provider.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            headers: Vec::new(),
            model: model.into(),
            completions_url: "https://api.openai.com/v1/chat/completions".to_owned(),
            prompt_cache: true,
            prompt_cache_retention: None,
        }
    }

    /// Set the exact Chat Completions endpoint URL used by this provider.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.completions_url = url.into();
        self
    }

    /// Add a request header to this provider.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the `OpenAI` prompt cache retention hint for outgoing requests.
    #[must_use]
    pub fn with_prompt_cache_retention(mut self, retention: impl Into<String>) -> Self {
        self.prompt_cache_retention = Some(retention.into());
        self
    }

    /// Disable `OpenAI` prompt cache routing hints on outgoing requests.
    #[must_use]
    pub const fn without_prompt_cache(mut self) -> Self {
        self.prompt_cache = false;
        self
    }
}

impl LlmStream for OpenAiChatCompletions {
    fn transient_api(&self) -> Option<crate::TransientInputApi> {
        Some(crate::TransientInputApi::OpenAiChat)
    }
    fn preflight_transient_request(
        &self,
        request: &CompletionRequest<'_>,
        input: &crate::AdmittedTransientInput,
    ) -> crate::Result<()> {
        let request = request.for_provider_identity(
            CHAT_COMPLETIONS_NAME,
            &self.completions_url,
            &self.model,
        );
        input.input().check_body(&request::chat_body_with_input(
            &self.model,
            self.prompt_cache,
            self.prompt_cache_retention.as_deref(),
            &request,
            Some(input.input()),
        ))
    }
    fn stream_transient_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        input: &'a crate::AdmittedTransientInput,
    ) -> crate::BoxStreamFuture<'a> {
        self.stream_with_input(request, Some(input.input()))
    }
    fn name(&self) -> &'static str {
        CHAT_COMPLETIONS_NAME
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let request = request.for_provider_identity(
            CHAT_COMPLETIONS_NAME,
            &self.completions_url,
            &self.model,
        );
        Some(chat_body(
            &self.model,
            self.prompt_cache,
            self.prompt_cache_retention.as_deref(),
            &request,
        ))
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        self.stream_with_input(request, None)
    }
}

impl OpenAiChatCompletions {
    fn stream_with_input<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        input: Option<&'a crate::TransientInput>,
    ) -> crate::BoxStreamFuture<'a> {
        Box::pin(async move {
            let transient = input.is_some();
            let request = request.for_provider_identity(
                CHAT_COMPLETIONS_NAME,
                &self.completions_url,
                &self.model,
            );
            let body = request::chat_body_with_input(
                &self.model,
                self.prompt_cache,
                self.prompt_cache_retention.as_deref(),
                &request,
                input,
            );
            if let Some(input) = input {
                input.check_body(&body)?;
            }
            let mut parser =
                ChatParser::new(CHAT_COMPLETIONS_NAME, &self.completions_url, &self.model);

            stream_openai_http(
                CHAT_COMPLETIONS_NAME,
                &self.completions_url,
                body,
                transient,
                self.headers.clone(),
                move |data| {
                    parser.parse_event(data).map_err(|error| {
                        if transient {
                            crate::transient_input::sanitize_error(error)
                        } else {
                            error
                        }
                    })
                },
            )
            .await
        })
    }
}
