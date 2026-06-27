//! `OpenAI` Responses HTTP transport.

use serde_json::Value;

use crate::stream::{CompletionRequest, LlmStream};

use super::OpenAiResponses;
use super::parser::ResponsesParser;
use super::request::responses_body;
use crate::provider::openai::shared::{RESPONSES_NAME, stream_openai_http};

impl LlmStream for OpenAiResponses {
    fn name(&self) -> &'static str {
        RESPONSES_NAME
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<Value> {
        let request =
            request.for_provider_identity(RESPONSES_NAME, &self.responses_url, &self.model);
        Some(responses_body(
            &self.model,
            self.reasoning.as_ref(),
            &request,
        ))
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let request =
                request.for_provider_identity(RESPONSES_NAME, &self.responses_url, &self.model);
            let mut parser = ResponsesParser::new(RESPONSES_NAME, &self.responses_url, &self.model);
            stream_openai_http(
                RESPONSES_NAME,
                &self.responses_url,
                responses_body(&self.model, self.reasoning.as_ref(), &request),
                self.headers.clone(),
                move |data| parser.parse_event(data),
            )
            .await
        })
    }
}
