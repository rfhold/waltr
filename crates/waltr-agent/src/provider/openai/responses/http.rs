//! `OpenAI` Responses HTTP transport.

use crate::stream::{CompletionRequest, LlmStream};

use super::OpenAiResponses;
use super::parser::ResponsesParser;
use super::request::responses_body;
use crate::provider::openai::shared::{RESPONSES_NAME, stream_openai_http};

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
