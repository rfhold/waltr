//! `OpenAI` Chat Completions streaming provider.

mod parser;
mod request;

use serde_json::json;

use crate::stream::{CompletionRequest, LlmStream};

use self::parser::ChatParser;
use self::request::{chat_messages, chat_tools};
use super::shared::{CHAT_COMPLETIONS_NAME, stream_openai_http};

/// `OpenAI` Chat Completions HTTP streaming provider.
#[derive(Debug, Clone)]
pub struct OpenAiChatCompletions {
    api_key: String,
    model: String,
    completions_url: String,
}

impl OpenAiChatCompletions {
    /// Create an `OpenAI` Chat Completions provider.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
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
}

impl LlmStream for OpenAiChatCompletions {
    fn name(&self) -> &'static str {
        CHAT_COMPLETIONS_NAME
    }

    fn stream_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
    ) -> crate::stream::BoxStreamFuture<'a> {
        Box::pin(async move {
            let body = json!({
                "model": self.model,
                "stream": true,
                "store": false,
                "messages": chat_messages(&request),
                "tools": chat_tools(&request.tools),
            });
            let mut parser = ChatParser::default();

            stream_openai_http(
                CHAT_COMPLETIONS_NAME,
                &self.completions_url,
                &self.api_key,
                body,
                move |data| parser.parse_event(data),
            )
            .await
        })
    }
}
