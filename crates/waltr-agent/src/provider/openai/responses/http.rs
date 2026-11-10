//! `OpenAI` Responses HTTP transport.

use serde_json::Value;

use crate::stream::{CompletionRequest, LlmStream};

use super::OpenAiResponses;
use super::parser::ResponsesParser;
use super::request::responses_body;
use crate::provider::openai::shared::{RESPONSES_NAME, stream_openai_http};

impl LlmStream for OpenAiResponses {
    fn transient_api(&self) -> Option<crate::TransientInputApi> {
        Some(crate::TransientInputApi::OpenAiResponses)
    }
    fn preflight_transient_request(
        &self,
        request: &CompletionRequest<'_>,
        input: &crate::AdmittedTransientInput,
    ) -> crate::Result<()> {
        let request =
            request.for_provider_identity(RESPONSES_NAME, &self.responses_url, &self.model);
        input
            .input()
            .check_body(&super::request::responses_body_with_input(
                &self.model,
                self.reasoning.as_ref(),
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
        RESPONSES_NAME
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let request =
            request.for_provider_identity(RESPONSES_NAME, &self.responses_url, &self.model);
        Some(responses_body(
            &self.model,
            self.reasoning.as_ref(),
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

impl OpenAiResponses {
    fn stream_with_input<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        input: Option<&'a crate::TransientInput>,
    ) -> crate::BoxStreamFuture<'a> {
        Box::pin(async move {
            let transient = input.is_some();
            let request =
                request.for_provider_identity(RESPONSES_NAME, &self.responses_url, &self.model);
            let mut parser = ResponsesParser::new(RESPONSES_NAME, &self.responses_url, &self.model);
            let body = super::request::responses_body_with_input(
                &self.model,
                self.reasoning.as_ref(),
                self.prompt_cache,
                self.prompt_cache_retention.as_deref(),
                &request,
                input,
            );
            if let Some(input) = input {
                input.check_body(&body)?;
            }
            stream_openai_http(
                RESPONSES_NAME,
                &self.responses_url,
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
