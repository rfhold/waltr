//! Ignored live durable replay tests across all provider wire formats.

use futures_util::StreamExt;
use waltr_agent::{
    Agent, AgentEvent, ExternalRequestId, ExternalResponse, FilesystemSessionStore, LlmStream,
    ModelApi, ModelRef, ProviderId, ResumeInput, SessionStore,
    provider::{
        AnthropicThinkingConfig, AnthropicThinkingDisplay, OpenAiReasoningConfig,
        OpenAiReasoningEffort, OpenAiReasoningSummary,
    },
    tools::question::{QUESTION_TOOL_NAME, QuestionConfig},
};
use serde_json::json;
use tempfile::TempDir;

use crate::live_provider;

const COMPLETION_MARKER: &str = "durable replay complete";
const FIRST_ANSWER: &str = "first-answer-ack";
const SECOND_ANSWER: &str = "second-answer-ack";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_replays_durable_session() -> waltr_agent::Result<()> {
    let provider = live_provider::anthropic_messages_provider(2048)?.with_thinking(
        AnthropicThinkingConfig::Enabled {
            budget_tokens: 1024,
            display: Some(AnthropicThinkingDisplay::Summarized),
        },
    );
    assert_provider_durable_replay(provider, live_provider::anthropic_model(), true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_replays_durable_session() -> waltr_agent::Result<()> {
    let provider = live_provider::openai_chat_completions_provider()?;
    assert_provider_durable_replay(
        provider,
        openai_model(ModelApi::OpenAiChatCompletions),
        false,
    )
    .await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_replays_durable_session() -> waltr_agent::Result<()> {
    let provider =
        live_provider::openai_responses_provider()?.with_reasoning(openai_reasoning_config());
    assert_provider_durable_replay(provider, openai_model(ModelApi::OpenAiResponses), true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_replays_durable_session() -> waltr_agent::Result<()> {
    let provider = live_provider::openai_responses_websocket_provider()?
        .with_reasoning(openai_reasoning_config());
    assert_provider_durable_replay(
        provider,
        openai_model(ModelApi::OpenAiResponsesWebSocket),
        true,
    )
    .await
}

async fn assert_provider_durable_replay(
    provider: impl LlmStream,
    model: ModelRef,
    expect_thinking: bool,
) -> waltr_agent::Result<()> {
    let label = provider.name();
    let temp = TempDir::new()?;
    let store = FilesystemSessionStore::new(temp.path().join("sessions"));
    let agent = Agent::builder(model)
        .with_system_prompt(
            "You are validating durable session replay. Follow the user instructions exactly.",
        )
        .with_question_tool(QuestionConfig::new())
        .build();
    let sessions = agent.sessions(store.clone());
    let session_id = sessions.create().await?.id().clone();

    let first = collect(
        sessions
            .prompt(
                &provider,
                session_id.clone(),
                format!(
                    "Call the question tool exactly twice, one call at a time. First call it to \
                     ask: 'First durable replay check?'. After you receive that answer, call it \
                     again to ask: 'Second durable replay check?'. After you receive the second \
                     answer, reply with exactly: {COMPLETION_MARKER}"
                ),
            )
            .stream(),
    )
    .await?;
    assert!(
        !first.finished,
        "{label}: first prompt should suspend on the question tool"
    );
    let first_request = expect_external_request(&first, label, "first")?;

    let second = collect(
        sessions
            .resume(
                &provider,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new(first_request),
                    response: ExternalResponse::new(json!(FIRST_ANSWER)),
                },
            )
            .stream(),
    )
    .await?;
    assert!(
        !second.finished,
        "{label}: first resume should suspend on the second question"
    );
    let second_request = expect_external_request(&second, label, "second")?;

    let resumed = collect(
        sessions
            .resume(
                &provider,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new(second_request),
                    response: ExternalResponse::new(json!(SECOND_ANSWER)),
                },
            )
            .stream(),
    )
    .await?;
    assert!(resumed.finished, "{label}: final resume did not finish");
    assert!(
        resumed.text.contains(COMPLETION_MARKER),
        "{label}: unexpected final text: {}",
        resumed.text
    );

    let stored = store.load(&session_id).await.map_err(store_error)?;
    for answer in [FIRST_ANSWER, SECOND_ANSWER] {
        assert!(
            stored
                .snapshot
                .tool_results()
                .any(|result| result.name == QUESTION_TOOL_NAME && result.content.contains(answer)),
            "{label}: stored session is missing the persisted question answer {answer}: {stored:?}"
        );
    }
    if expect_thinking {
        // Providers decide per-turn whether to think (Anthropic without
        // interleaved thinking typically only thinks on the first turn), so
        // require at least one turn-tagged thinking block rather than one per
        // turn. The later turns still prove replay validity: the provider
        // accepts requests that replay the earlier thinking payloads.
        let thinking_turns: Vec<u64> = stored
            .snapshot
            .turns
            .iter()
            .filter(|turn| turn.thinking_blocks().next().is_some())
            .map(|turn| turn.turn)
            .collect();
        assert!(
            !thinking_turns.is_empty(),
            "{label}: expected turn-tagged thinking blocks in the stored session: {stored:?}"
        );
    }
    Ok(())
}

#[derive(Default)]
struct CollectedSession {
    text: String,
    finished: bool,
    external_request_ids: Vec<String>,
}

async fn collect(
    mut events: waltr_agent::BoxAgentEventStream<'_>,
) -> waltr_agent::Result<CollectedSession> {
    let mut collected = CollectedSession::default();
    while let Some(event) = events.next().await {
        match event? {
            AgentEvent::TextDelta { delta, .. } => collected.text.push_str(&delta),
            AgentEvent::ExternalRequest { request, .. } => {
                collected.external_request_ids.push(request.id)
            }
            AgentEvent::Finished { .. } => collected.finished = true,
            AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::TurnFinished { .. } => {}
        }
    }
    Ok(collected)
}

fn expect_external_request(
    collected: &CollectedSession,
    label: &str,
    stage: &str,
) -> waltr_agent::Result<String> {
    collected
        .external_request_ids
        .first()
        .cloned()
        .ok_or_else(|| {
            store_error(format!(
                "{label}: expected {stage} external question request"
            ))
        })
}

const fn openai_reasoning_config() -> OpenAiReasoningConfig {
    OpenAiReasoningConfig {
        effort: OpenAiReasoningEffort::Low,
        summary: Some(OpenAiReasoningSummary::Auto),
    }
}

fn openai_model(api: ModelApi) -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, live_provider::openai_model_name(), api)
}

fn store_error(error: impl std::fmt::Display) -> waltr_agent::LlmError {
    waltr_agent::LlmError::Protocol {
        provider: "session-providers-test",
        message: error.to_string(),
    }
}
