//! Live provider thinking/reasoning integration tests.

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, CHANNEL_ASSISTANT_TEXT, CHANNEL_THINKING, LlmError, LlmStream,
    MemorySessionStore, ModelApi, ModelRef, SessionCoordinator, SessionId, SessionStore,
    TimelineContent, TimelineEventKind, TimelineStatus,
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
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::anthropic_messages_provider(2048))?
    else {
        return Ok(());
    };
    let provider = provider.with_thinking(AnthropicThinkingConfig::Enabled {
        budget_tokens: 1024,
        display: Some(AnthropicThinkingDisplay::Summarized),
    });
    let model = live_provider::anthropic_model();

    assert_provider_thinking(provider, model, true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_surfaces_reasoning() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_provider())?
    else {
        return Ok(());
    };
    let provider = provider.with_reasoning(openai_reasoning_config());
    let model = openai_model(ModelApi::OpenAiResponses);

    assert_provider_thinking(provider, model, true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_surfaces_reasoning() -> waltr_agent::Result<()> {
    let Some(provider) = live_provider::skip_if_missing_config(
        live_provider::openai_responses_websocket_provider(),
    )?
    else {
        return Ok(());
    };
    let provider = provider.with_reasoning(openai_reasoning_config());
    let model = openai_model(ModelApi::OpenAiResponsesWebSocket);

    assert_provider_thinking(provider, model, true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_is_no_thinking_baseline() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions_provider())?
    else {
        return Ok(());
    };
    let model = openai_model(ModelApi::OpenAiChatCompletions);

    assert_provider_thinking(provider, model, false).await
}

async fn assert_provider_thinking(
    provider: impl LlmStream + 'static,
    model: ModelRef,
    expect_thinking: bool,
) -> waltr_agent::Result<()> {
    let provider_name = provider.name();
    let agent = AgentRuntime::builder(model, std::sync::Arc::new(provider)).build();
    let store = MemorySessionStore::new();
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions
        .create(waltr_agent::SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    let mut stream = sessions
        .prompt(&agent, session_id.clone(),
            format!("Think briefly if supported, then answer exactly: {EXPECTED_TEXT}"),
        )
        .with_system_prompt_fragment(
            "You are running inside an automated test harness. Follow the user instructions exactly.",
        )
        .stream();

    let mut thinking_delta_count = 0_u16;
    let mut text = String::new();
    let mut finished = false;
    while let Some(event) = stream.next().await {
        let event = event?;
        match &event.content {
            TimelineContent::TextDelta { .. }
                if crate::session::channel_is(&event, CHANNEL_THINKING) =>
            {
                thinking_delta_count += 1;
            }
            TimelineContent::TextDelta { delta }
                if crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                text.push_str(delta);
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                finished = true;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ExternalRequest { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }

    if !finished {
        return Err(LlmError::Protocol {
            provider: provider_name,
            message: "agent stream ended without finished status".to_owned(),
        });
    }
    assert!(text.contains(EXPECTED_TEXT), "{provider_name}: {text}");

    let stored = store
        .load(&session_id)
        .await
        .map_err(|error| LlmError::Protocol {
            provider: provider_name,
            message: error.to_string(),
        })?;
    let saw_stored_thinking = stored.timeline_events().any(|event| {
        crate::session::channel_is(event, CHANNEL_THINKING)
            && matches!(&event.content, TimelineContent::TextDelta { .. })
    }) || stored.snapshot().thinking_blocks().next().is_some();
    let saw_thinking = thinking_delta_count > 0 || saw_stored_thinking;
    assert_eq!(saw_thinking, expect_thinking, "{provider_name}");

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
