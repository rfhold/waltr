//! Live provider thinking/reasoning integration tests.

use futures_util::StreamExt;
use waltr_agent::{
    Agent, AgentEvent, LlmError, LlmStream, MemorySessionStore, ModelApi, ModelRef, SessionStore,
    provider::{
        AnthropicThinkingConfig, AnthropicThinkingDisplay, OpenAiReasoningConfig,
        OpenAiReasoningEffort, OpenAiReasoningSummary,
    },
};

use crate::live_provider;

const EXPECTED_TEXT: &str = "thinking-ok";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_surfaces_thinking() -> waltr_agent::Result<()> {
    let provider = live_provider::anthropic_messages_provider(2048)?.with_thinking(
        AnthropicThinkingConfig::Enabled {
            budget_tokens: 1024,
            display: Some(AnthropicThinkingDisplay::Summarized),
        },
    );
    let model = live_provider::anthropic_model();

    assert_provider_thinking(provider, model, true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_surfaces_reasoning() -> waltr_agent::Result<()> {
    let provider =
        live_provider::openai_responses_provider()?.with_reasoning(openai_reasoning_config());
    let model = openai_model(ModelApi::OpenAiResponses);

    assert_provider_thinking(provider, model, true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_surfaces_reasoning() -> waltr_agent::Result<()> {
    let provider = live_provider::openai_responses_websocket_provider()?
        .with_reasoning(openai_reasoning_config());
    let model = openai_model(ModelApi::OpenAiResponsesWebSocket);

    assert_provider_thinking(provider, model, true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_is_no_thinking_baseline() -> waltr_agent::Result<()> {
    let provider = live_provider::openai_chat_completions_provider()?;
    let model = openai_model(ModelApi::OpenAiChatCompletions);

    assert_provider_thinking(provider, model, false).await
}

async fn assert_provider_thinking(
    provider: impl LlmStream,
    model: ModelRef,
    expect_thinking: bool,
) -> waltr_agent::Result<()> {
    let agent = Agent::builder(model).build();
    let store = MemorySessionStore::new();
    let sessions = agent.sessions(store.clone());
    let session_id = sessions.create().await?.id().clone();
    let mut stream = sessions
        .prompt(
            &provider,
            session_id.clone(),
            format!("Think briefly if supported, then answer exactly: {EXPECTED_TEXT}"),
        )
        .with_system_prompt_fragment(
            "You are running inside an automated test harness. Follow the user instructions exactly.",
        )
        .stream();

    let mut thinking_delta_count = 0_u16;
    let mut final_state = None;
    while let Some(event) = stream.next().await {
        match event? {
            AgentEvent::ThinkingDelta { .. } => thinking_delta_count += 1,
            AgentEvent::Finished { state } => final_state = Some(state),
            AgentEvent::TextDelta { .. }
            | AgentEvent::ExternalRequest { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::TurnFinished { .. } => {}
        }
    }

    let state = final_state.ok_or_else(|| LlmError::Protocol {
        provider: provider.name(),
        message: "agent stream ended without final state".to_owned(),
    })?;
    let text = state.last_assistant_text();
    assert!(text.contains(EXPECTED_TEXT), "{}: {text}", provider.name());

    let stored = store
        .load(&session_id)
        .await
        .map_err(|error| LlmError::Protocol {
            provider: provider.name(),
            message: error.to_string(),
        })?;
    let saw_thinking =
        thinking_delta_count > 0 || stored.snapshot.thinking_blocks().next().is_some();
    assert_eq!(saw_thinking, expect_thinking, "{}", provider.name());

    Ok(())
}

const fn openai_reasoning_config() -> OpenAiReasoningConfig {
    OpenAiReasoningConfig {
        effort: OpenAiReasoningEffort::Low,
        summary: Some(OpenAiReasoningSummary::Auto),
    }
}

fn openai_model(api: ModelApi) -> ModelRef {
    ModelRef::new(
        waltr_agent::ProviderId::OpenAi,
        live_provider::openai_model_name(),
        api,
    )
}
