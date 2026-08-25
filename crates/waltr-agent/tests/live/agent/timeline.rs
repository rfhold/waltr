//! Live provider agent timeline streaming tests.

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, CHANNEL_ASSISTANT_TEXT, CHANNEL_USER_TEXT, LlmError, LlmStream,
    MemorySessionStore, ModelRef, SessionCoordinator, SessionId, SessionTimelineEvent, TimelineContent,
    TimelineEventKind, TimelineQuery, TimelineStatus,
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

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn selected_provider_publishes_truncation_after_rollback_before_cursor()
-> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_chat_completions",
    ))?
    else {
        return Ok(());
    };
    assert_provider_publishes_truncation_after_rollback(live.provider, live.model).await
}

async fn assert_provider_streams_timeline_updates(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let provider_name = provider.name();
    let agent = AgentRuntime::builder(model, Arc::new(provider)).build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let session_id = sessions
        .create(waltr_agent::SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
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
        let waltr_agent::SessionTimelineEvent::Event(event) = update.event else {
            continue;
        };
        let finished = event.kind == TimelineEventKind::SessionStatusChanged
            && event.status == Some(TimelineStatus::Finished);
        events.push(event);
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

async fn assert_provider_publishes_truncation_after_rollback(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let provider_name = provider.name();
    let agent = AgentRuntime::builder(model, Arc::new(provider)).build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let session_id = sessions
        .create(waltr_agent::SessionCreateRequest::new(SessionId::generate()))
        .await?
        .id()
        .clone();
    let mut timeline = sessions.watch_timeline(session_id.clone());
    let first_prompt = "Answer with exactly: rollback live first";
    let second_prompt = "Answer with exactly: rollback live second";

    run_prompt_to_finished(
        &sessions,
        &agent,
        session_id.clone(),
        first_prompt,
        provider_name,
    )
    .await?;
    run_prompt_to_finished(
        &sessions,
        &agent,
        session_id.clone(),
        second_prompt,
        provider_name,
    )
    .await?;

    let events = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 128))
        .await?;
    let first_user_cursor = user_prompt_cursor(&events, first_prompt, provider_name)?;
    let second_user_cursor = user_prompt_cursor(&events, second_prompt, provider_name)?;

    sessions
        .rollback_before_cursor(session_id.clone(), second_user_cursor)
        .await?;
    let update = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let update = timeline.next().await.ok_or_else(|| LlmError::Protocol {
                provider: provider_name,
                message: "timeline watch ended before truncation".to_owned(),
            })?;
            if matches!(update.event, SessionTimelineEvent::Truncated { .. }) {
                return Ok::<_, LlmError>(update);
            }
        }
    })
    .await
    .map_err(|_| LlmError::Protocol {
        provider: provider_name,
        message: "timed out waiting for rollback truncation".to_owned(),
    })??;
    let remaining = sessions
        .timeline(&session_id, TimelineQuery::forward(None, 128))
        .await?;

    assert!(matches!(
        update.event,
        SessionTimelineEvent::Truncated {
            after_cursor: Some(cursor),
            ..
        } if cursor == first_user_cursor
    ));
    assert!(
        remaining
            .iter()
            .any(|event| event.cursor == first_user_cursor),
        "{provider_name}: first prompt should remain after rollback: {remaining:?}"
    );
    assert!(
        remaining
            .iter()
            .all(|event| event.cursor < second_user_cursor),
        "{provider_name}: second prompt and later events should be truncated: {remaining:?}"
    );

    Ok(())
}

async fn run_prompt_to_finished(
    sessions: &SessionCoordinator<MemorySessionStore>,
    agent: &AgentRuntime,
    session_id: waltr_agent::SessionId,
    prompt: &str,
    provider_name: &'static str,
) -> waltr_agent::Result<()> {
    let mut stream = sessions
        .prompt(agent, session_id, prompt)
        .with_system_prompt_fragment(
            "You are running inside an automated test harness. Follow the user instructions exactly.",
        )
        .stream();
    let mut finished = false;
    while let Some(event) = stream.next().await {
        let event = event?;
        if event.kind == TimelineEventKind::SessionStatusChanged
            && event.status == Some(TimelineStatus::Finished)
        {
            finished = true;
        }
    }

    if !finished {
        return Err(LlmError::Protocol {
            provider: provider_name,
            message: "prompt stream ended before finished status".to_owned(),
        });
    }
    Ok(())
}

fn user_prompt_cursor(
    events: &[waltr_agent::AgentTimelineEvent],
    prompt: &str,
    provider_name: &'static str,
) -> waltr_agent::Result<waltr_agent::TimelineCursor> {
    events
        .iter()
        .find(|event| {
            crate::session::channel_is(event, CHANNEL_USER_TEXT)
                && event.kind == TimelineEventKind::ItemFinished
                && matches!(&event.content, TimelineContent::TextSnapshot { text } if text == prompt)
        })
        .map(|event| event.cursor)
        .ok_or_else(|| LlmError::Protocol {
            provider: provider_name,
            message: format!("missing user prompt timeline cursor for {prompt:?}: {events:?}"),
        })
}
