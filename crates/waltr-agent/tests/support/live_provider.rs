//! Shared live provider construction for integration tests.

#![allow(dead_code)]

use std::env;

use waltr_agent::{
    BoxStreamFuture, CompletionRequest, LlmError, LlmStream, ModelApi, ModelRef, ProviderId,
    provider::{
        AnthropicMessages, OpenAiChatCompletions, OpenAiResponses, OpenAiResponsesWebSocket,
    },
};

pub const ANTHROPIC_TEST_MODEL: &str = "claude-haiku-4-5";
pub const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
pub const GATEWAY_ENV: &str = "WALTR_AGENT_LIVE_GATEWAY";
pub const GATEWAY_BASE_URL: &str = "https://agent-gateway.holdenitdown.net";
pub const GATEWAY_ANTHROPIC_TEST_MODEL: &str = "anthropic/claude-haiku-4-5";
pub const GATEWAY_OPENAI_TEST_MODEL: &str = "openai/gpt-5-nano";

/// Provider and matching model for a live test case.
pub struct LiveProviderCase {
    /// Provider implementation.
    pub provider: LiveProvider,
    /// Matching model reference.
    pub model: ModelRef,
}

/// Concrete live provider selected for a test.
pub enum LiveProvider {
    /// Anthropic Messages API.
    AnthropicMessages(AnthropicMessages),
    /// `OpenAI` Chat Completions API.
    OpenAiChatCompletions(OpenAiChatCompletions),
    /// `OpenAI` Responses API.
    OpenAiResponses(OpenAiResponses),
    /// `OpenAI` Responses WebSocket API.
    OpenAiResponsesWebSocket(OpenAiResponsesWebSocket),
}

impl LlmStream for LiveProvider {
    fn name(&self) -> &'static str {
        match self {
            Self::AnthropicMessages(provider) => provider.name(),
            Self::OpenAiChatCompletions(provider) => provider.name(),
            Self::OpenAiResponses(provider) => provider.name(),
            Self::OpenAiResponsesWebSocket(provider) => provider.name(),
        }
    }

    fn project_request(&self, request: &CompletionRequest<'_>) -> Option<serde_json::Value> {
        match self {
            Self::AnthropicMessages(provider) => provider.project_request(request),
            Self::OpenAiChatCompletions(provider) => provider.project_request(request),
            Self::OpenAiResponses(provider) => provider.project_request(request),
            Self::OpenAiResponsesWebSocket(provider) => provider.project_request(request),
        }
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        match self {
            Self::AnthropicMessages(provider) => provider.stream_request(request),
            Self::OpenAiChatCompletions(provider) => provider.stream_request(request),
            Self::OpenAiResponses(provider) => provider.stream_request(request),
            Self::OpenAiResponsesWebSocket(provider) => provider.stream_request(request),
        }
    }
}

/// Build the Anthropic Messages live provider case.
pub fn anthropic_messages() -> waltr_agent::Result<LiveProviderCase> {
    Ok(LiveProviderCase {
        provider: LiveProvider::AnthropicMessages(anthropic_messages_provider(1024)?),
        model: anthropic_model(),
    })
}

/// Build the Anthropic Messages provider for live tests.
pub fn anthropic_messages_provider(max_tokens: u32) -> waltr_agent::Result<AnthropicMessages> {
    let mut provider = AnthropicMessages::new(
        api_key("ANTHROPIC_API_KEY")?,
        model_name(ANTHROPIC_TEST_MODEL, GATEWAY_ANTHROPIC_TEST_MODEL),
    )
    .with_max_tokens(max_tokens);
    if use_gateway() {
        provider = provider.with_base_url(GATEWAY_BASE_URL);
    }
    Ok(provider)
}

/// Build the `OpenAI` Chat Completions live provider case.
pub fn openai_chat_completions() -> waltr_agent::Result<LiveProviderCase> {
    Ok(LiveProviderCase {
        provider: LiveProvider::OpenAiChatCompletions(openai_chat_completions_provider()?),
        model: openai_model(ModelApi::OpenAiChatCompletions),
    })
}

/// Build the `OpenAI` Chat Completions provider for live tests.
pub fn openai_chat_completions_provider() -> waltr_agent::Result<OpenAiChatCompletions> {
    let mut provider = OpenAiChatCompletions::new(api_key("OPENAI_API_KEY")?, openai_model_name());
    if use_gateway() {
        provider = provider.with_base_url(GATEWAY_BASE_URL);
    }
    Ok(provider)
}

/// Build the `OpenAI` Responses live provider case.
pub fn openai_responses() -> waltr_agent::Result<LiveProviderCase> {
    Ok(LiveProviderCase {
        provider: LiveProvider::OpenAiResponses(openai_responses_provider()?),
        model: openai_model(ModelApi::OpenAiResponses),
    })
}

/// Build the `OpenAI` Responses provider for live tests.
pub fn openai_responses_provider() -> waltr_agent::Result<OpenAiResponses> {
    let mut provider = OpenAiResponses::new(api_key("OPENAI_API_KEY")?, openai_model_name());
    if use_gateway() {
        provider = provider.with_base_url(GATEWAY_BASE_URL);
    }
    Ok(provider)
}

/// Build the `OpenAI` Responses WebSocket live provider case.
pub fn openai_responses_websocket() -> waltr_agent::Result<LiveProviderCase> {
    Ok(LiveProviderCase {
        provider: LiveProvider::OpenAiResponsesWebSocket(openai_responses_websocket_provider()?),
        model: openai_model(ModelApi::OpenAiResponsesWebSocket),
    })
}

/// Build the `OpenAI` Responses WebSocket provider for live tests.
pub fn openai_responses_websocket_provider() -> waltr_agent::Result<OpenAiResponsesWebSocket> {
    let mut provider =
        OpenAiResponsesWebSocket::new(api_key("OPENAI_API_KEY")?, openai_model_name());
    if use_gateway() {
        provider = provider.with_base_url(GATEWAY_BASE_URL);
    }
    Ok(provider)
}

/// Build a live provider case from an environment selector.
pub fn selected_from_env(
    env_name: &'static str,
    default: &str,
) -> waltr_agent::Result<LiveProviderCase> {
    match env::var(env_name)
        .unwrap_or_else(|_| default.to_owned())
        .as_str()
    {
        "openai_responses_websocket" => openai_responses_websocket(),
        "openai_responses" => openai_responses(),
        "openai_chat_completions" => openai_chat_completions(),
        "anthropic_messages" => anthropic_messages(),
        other => Err(LlmError::Protocol {
            provider: "live_provider",
            message: format!(
                "unsupported {env_name}={other}; expected one of openai_responses_websocket, openai_responses, openai_chat_completions, anthropic_messages"
            ),
        }),
    }
}

/// Read a required environment variable for live tests.
pub fn required_env(name: &'static str) -> waltr_agent::Result<String> {
    env::var(name).map_err(|_| LlmError::MissingConfig { name })
}

/// Whether live tests should use the agent gateway instead of direct providers.
pub fn use_gateway() -> bool {
    matches!(
        env::var(GATEWAY_ENV).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Matching Anthropic model reference for the active live test target.
pub fn anthropic_model() -> ModelRef {
    ModelRef::new(
        ProviderId::Anthropic,
        model_name(ANTHROPIC_TEST_MODEL, GATEWAY_ANTHROPIC_TEST_MODEL),
        ModelApi::AnthropicMessages,
    )
}

fn api_key(name: &'static str) -> waltr_agent::Result<String> {
    if use_gateway() {
        Ok(String::new())
    } else {
        required_env(name)
    }
}

fn model_name(direct: &'static str, gateway: &'static str) -> &'static str {
    if use_gateway() { gateway } else { direct }
}

/// Active `OpenAI`-compatible model name for live tests.
pub fn openai_model_name() -> &'static str {
    model_name(OPENAI_TEST_MODEL, GATEWAY_OPENAI_TEST_MODEL)
}

fn openai_model(api: ModelApi) -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, openai_model_name(), api)
}
