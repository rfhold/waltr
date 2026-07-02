//! Live provider agent timeline streaming tests.

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, CHANNEL_ASSISTANT_TEXT, CHANNEL_USER_TEXT, LlmError, LlmStream,
    MemorySessionStore, ModelRef, SessionCoordinator, TimelineContent, TimelineEventKind,
    TimelineStatus,
};

use crate::live_provider;

const EXPECTED_TEXT: &str =
    "timeline streaming keeps the client responsive while the assistant writes a longer answer";
const TEST_PROMPT: &str = "Answer with exactly this sentence and no quotation marks: timeline streaming keeps the client responsive while the assistant writes a longer answer";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_streams_timeline_updates() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::anthropic_messages())?
    else {
        return Ok(());
    };
    assert_provider_streams_timeline_updates(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_streams_timeline_updates() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions())?
    else {
        return Ok(());
    };
    assert_provider_streams_timeline_updates(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_streams_timeline_updates() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::openai_responses())?
    else {
        return Ok(());
    };
    assert_provider_streams_timeline_updates(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_streams_timeline_updates() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_websocket())?
    else {
        return Ok(());
    };
    assert_provider_streams_timeline_updates(live.provider, live.model).await
}

async fn assert_provider_streams_timeline_updates(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let provider_name = provider.name();
    let agent = AgentRuntime::builder(model, Arc::new(provider)).build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let session_id = sessions.create().await?.id().clone();
    let mut timeline = sessions.watch_timeline(session_id.clone());

    let run_sessions = sessions.clone();
    let run_agent = agent.clone();
    let run_session_id = session_id.clone();
    let run = tokio::spawn(async move {
        let mut stream = run_sessions
            .prompt(&run_agent, run_session_id, TEST_PROMPT)
            .with_system_prompt_fragment(
                "You are running inside an automated test harness. Follow the user instructions exactly.",
            )
            .stream();
        while let Some(event) = stream.next().await {
            event?;
        }
        waltr_agent::Result::Ok(())
    });

    let mut events = Vec::new();
    loop {
        let update = tokio::time::timeout(Duration::from_secs(30), timeline.next())
            .await
            .map_err(|_| LlmError::Protocol {
                provider: provider_name,
                message: "timed out waiting for timeline update".to_owned(),
            })?
            .ok_or_else(|| LlmError::Protocol {
                provider: provider_name,
                message: "timeline watch ended before finished status".to_owned(),
            })?;
        let finished = update.event.kind == TimelineEventKind::SessionStatusChanged
            && update.event.status == Some(TimelineStatus::Finished);
        events.push(update.event);
        if finished {
            break;
        }
    }

    run.await.map_err(|error| LlmError::Protocol {
        provider: provider_name,
        message: format!("agent run task failed: {error}"),
    })??;

    assert!(
        events.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_USER_TEXT)
                && event.kind == TimelineEventKind::ItemFinished
                && matches!(&event.content, TimelineContent::TextSnapshot { text } if text == TEST_PROMPT)
        }),
        "{provider_name}: missing user prompt timeline event: {events:?}"
    );
    let assistant_delta_count = events
        .iter()
        .filter(|event| {
            crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT)
                && matches!(&event.content, TimelineContent::TextDelta { .. })
        })
        .count();
    assert!(
        assistant_delta_count > 1,
        "{provider_name}: expected multiple assistant delta timeline events, saw {assistant_delta_count}: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_ASSISTANT_TEXT)
                && event.kind == TimelineEventKind::ItemFinished
                && matches!(&event.content, TimelineContent::TextSnapshot { text } if text.contains(EXPECTED_TEXT))
        }),
        "{provider_name}: missing final assistant snapshot: {events:?}"
    );

    Ok(())
}
