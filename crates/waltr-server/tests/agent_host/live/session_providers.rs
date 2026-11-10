//! Ignored server-hosted live turns across all provider wire formats.

use crate::agent_host::HostRuntimeBuilder;
use crate::agent_host::tools::question::{QUESTION_TOOL_NAME, QuestionConfig};
use crate::interaction::{Interaction, InteractionBroker, InteractionResponse};
use crate::permission::*;
use waltr_agent::provider::{
    AnthropicThinkingConfig, AnthropicThinkingDisplay, OpenAiReasoningConfig,
    OpenAiReasoningEffort, OpenAiReasoningSummary,
};

use futures_util::StreamExt;
use tempfile::TempDir;
use waltr_agent::*;

use crate::host_tests::live_provider;

const COMPLETION_MARKER: &str = "durable replay complete";
const FIRST_ANSWER: &str = "first-answer-ack";
const SECOND_ANSWER: &str = "second-answer-ack";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_replays_durable_session() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::anthropic_messages_provider(2048))?
    else {
        return Ok(());
    };
    let provider = provider.with_thinking(AnthropicThinkingConfig::Enabled {
        budget_tokens: 1024,
        display: Some(AnthropicThinkingDisplay::Summarized),
    });
    assert_provider_durable_replay(provider, live_provider::anthropic_model(), true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_replays_durable_session() -> waltr_agent::Result<()> {
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions_provider())?
    else {
        return Ok(());
    };
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
    let Some(provider) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_provider())?
    else {
        return Ok(());
    };
    let provider = provider.with_reasoning(openai_reasoning_config());
    assert_provider_durable_replay(provider, openai_model(ModelApi::OpenAiResponses), true).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_replays_durable_session() -> waltr_agent::Result<()> {
    let Some(provider) = live_provider::skip_if_missing_config(
        live_provider::openai_responses_websocket_provider(),
    )?
    else {
        return Ok(());
    };
    let provider = provider.with_reasoning(openai_reasoning_config());
    assert_provider_durable_replay(
        provider,
        openai_model(ModelApi::OpenAiResponsesWebSocket),
        true,
    )
    .await
}

async fn assert_provider_durable_replay(
    provider: impl LlmStream + 'static,
    model: ModelRef,
    expect_thinking: bool,
) -> waltr_agent::Result<()> {
    let label = provider.name();
    let provider = std::sync::Arc::new(provider);
    let temp = TempDir::new()?;
    let store = FilesystemSessionStore::new(temp.path().join("sessions"));
    let sessions = SessionCoordinator::new(store.clone());
    let session_id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    let broker = InteractionBroker::new();
    let agent = HostRuntimeBuilder::new(model, std::sync::Arc::clone(&provider))
        .with_system_prompt(
            "You are validating durable session replay. Follow the user instructions exactly.",
        )
        .with_question_tool(QuestionConfig::new())
        .with_dispatch_context(
            Some(session_id.clone()),
            ServerPermissionPolicy::sessions(broker.clone()),
        )
        .build();
    let owner_sessions = sessions.clone();
    let owner_id = session_id.clone();
    let owner = tokio::spawn(async move {
        collect(
        owner_sessions
            .prompt(
                &agent,
                owner_id,
                format!(
                    "Call the question tool exactly twice, one call at a time. First call it to \
                     ask: 'First durable replay check?'. After you receive that answer, call it \
                     again to ask: 'Second durable replay check?'. After you receive the second \
                     answer, reply with exactly: {COMPLETION_MARKER}"
                ),
            )
            .stream(),
    )
    .await
    });
    let mut previous = None;
    for answer in [FIRST_ANSWER, SECOND_ANSWER] {
        let snapshot = crate::host_tests::pending(&broker, &session_id).await;
        assert!(
            !owner.is_finished(),
            "{label}: original live turn must wait"
        );
        assert_ne!(previous.as_ref(), Some(&snapshot.pending[0].id));
        previous = Some(snapshot.pending[0].id.clone());
        let Interaction::Question { input } = &snapshot.pending[0].request else {
            panic!("question")
        };
        assert_eq!(input.questions.len(), 1);
        crate::host_tests::respond(
            &broker,
            &snapshot,
            InteractionResponse::Question(vec![vec![answer.into()]]),
        )
        .unwrap();
    }
    let resumed = owner.await.unwrap()?;
    assert!(resumed.finished, "{label}: final resume did not finish");
    assert!(
        resumed.text.contains(COMPLETION_MARKER),
        "{label}: unexpected final text: {}",
        resumed.text
    );

    let stored = store.load(&session_id).await.map_err(store_error)?;
    assert_stored_replay_facts(&stored, label, expect_thinking);
    assert!(broker.snapshot(&session_id).pending.is_empty());
    sessions.shutdown().await;
    Ok(())
}

fn assert_stored_replay_facts(
    stored: &waltr_agent::SessionRecord,
    label: &str,
    expect_thinking: bool,
) {
    let snapshot = stored.snapshot();
    for answer in [FIRST_ANSWER, SECOND_ANSWER] {
        assert!(
            snapshot
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
        assert!(
            snapshot
                .turns
                .iter()
                .any(|turn| turn.thinking_blocks().next().is_some()),
            "{label}: expected turn-tagged thinking blocks in the stored session: {stored:?}"
        );
    }
}

#[derive(Default)]
struct CollectedSession {
    text: String,
    finished: bool,
}

async fn collect(
    mut events: waltr_agent::BoxAgentTimelineStream<'_>,
) -> waltr_agent::Result<CollectedSession> {
    let mut collected = CollectedSession::default();
    while let Some(event) = events.next().await {
        let event = event?;
        match &event.content {
            TimelineContent::TextDelta { delta }
                if crate::host_tests::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
            {
                collected.text.push_str(delta);
            }
            TimelineContent::Empty
                if event.kind == TimelineEventKind::SessionStatusChanged
                    && event.status == Some(TimelineStatus::Finished) =>
            {
                collected.finished = true;
            }
            TimelineContent::Empty
            | TimelineContent::TextDelta { .. }
            | TimelineContent::TextSnapshot { .. }
            | TimelineContent::ToolCall { .. }
            | TimelineContent::ToolResult { .. }
            | TimelineContent::ContentRef { .. }
            | TimelineContent::Json { .. } => {}
        }
    }
    Ok(collected)
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
