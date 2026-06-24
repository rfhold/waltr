//! Live provider agent shell integration tests.

use std::sync::Arc;

use futures_util::StreamExt as _;
use waltr_agent::{
    AgentRunBuilder, AgentRuntime, AgentSessions, AgentTimelineEvent, CHANNEL_TOOL_CALL,
    CHANNEL_TOOL_RESULT, LlmStream, MemorySessionStore, ModelRef, PermissionCommandPattern,
    PermissionEffect, PermissionPolicy, PermissionToolSelector, SessionStatus, Suspension,
    TimelineContent, tools::shell::ShellConfig,
};
use tokio::{sync::mpsc, time};
use tokio_util::sync::CancellationToken;

use crate::{live_provider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::anthropic_messages())?
    else {
        return Ok(());
    };
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions())?
    else {
        return Ok(());
    };
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::openai_responses())?
    else {
        return Ok(());
    };
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_websocket())?
    else {
        return Ok(());
    };
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_handles_shell_tool_interrupts() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::anthropic_messages())?
    else {
        return Ok(());
    };
    assert_provider_handles_shell_tool_interrupts(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_handles_shell_tool_interrupts() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions())?
    else {
        return Ok(());
    };
    assert_provider_handles_shell_tool_interrupts(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_handles_shell_tool_interrupts() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::openai_responses())?
    else {
        return Ok(());
    };
    assert_provider_handles_shell_tool_interrupts(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_handles_shell_tool_interrupts() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_websocket())?
    else {
        return Ok(());
    };
    assert_provider_handles_shell_tool_interrupts(live.provider, live.model).await
}

async fn assert_provider_runs_shell_agent(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let provider = Arc::new(provider);
    let root = std::env::current_dir()?;
    let agent = AgentRuntime::builder(model, Arc::clone(&provider))
        .with_permission_policy(
            PermissionPolicy::filesystem_default(&root).with_command_rule(
                PermissionToolSelector::exact("shell"),
                PermissionCommandPattern::Name("printf".to_owned()),
                PermissionEffect::allow(),
            ),
        )
        .with_shell_tool(ShellConfig::new(root))
        .build();
    let session = AgentTestSession::run(
        &agent,
        &provider,
        "Use the shell tool exactly once to print waltr-shell-ok with printf. Then answer with waltr-shell-ok.",
    )
    .await?;

    session.require_tool_execution(provider.name())?;
    crate::session::report_tool_errors(&provider, &session, "shell live test");
    let context = crate::session::session_context(&provider, &session);

    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_CALL)
                && matches!(&event.content, TimelineContent::ToolCall { call } if call.name == "shell")
        }),
        "{context}"
    );
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                && matches!(&event.content, TimelineContent::ToolResult { result } if result.content.contains("waltr-shell-ok"))
        }),
        "{context}"
    );
    assert!(session.text.contains("waltr-shell-ok"), "{context}");

    Ok(())
}

async fn assert_provider_handles_shell_tool_interrupts(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let provider = Arc::new(provider);
    let root = std::env::current_dir()?;
    let agent = AgentRuntime::builder(model, Arc::clone(&provider))
        .with_permission_policy(shell_interrupt_policy())
        .with_shell_tool(ShellConfig::new(root))
        .build();
    let sessions = AgentSessions::new(MemorySessionStore::new());
    let session_id = sessions.create().await?.id().clone();

    let first_cancel = CancellationToken::new();
    let first_events = run_until_shell_call_then_cancel(
        sessions
            .prompt(
                &agent,
                session_id.clone(),
                "Use the shell tool exactly once with command `sleep 5 && echo foo` and description `sleep then echo foo`. Do not answer before the tool finishes.",
            )
            .with_cancellation_token(first_cancel.clone()),
        first_cancel,
        "sleep 5",
    )
    .await?;
    assert_shell_tool_called(&provider, &first_events, "sleep 5");

    let second_events = drain_agent_run(
        sessions.prompt(
            &agent,
            session_id.clone(),
            "I changed my mind. Use the shell tool exactly once with command `echo bar` and description `echo bar`, then answer with `bar`.",
        ),
    )
    .await?;
    assert_shell_tool_called(&provider, &second_events, "echo bar");
    assert_shell_tool_result_contains(&provider, &second_events, "bar");

    let final_cancel = CancellationToken::new();
    let final_events = run_until_shell_call_then_cancel(
        sessions
            .prompt(
                &agent,
                session_id.clone(),
                "Use the shell tool exactly once with command `sleep 5` and description `sleep before stop`. Do not answer before the tool finishes.",
            )
            .with_cancellation_token(final_cancel.clone()),
        final_cancel,
        "sleep 5",
    )
    .await?;
    assert_shell_tool_called(&provider, &final_events, "sleep 5");

    let record = sessions.load(&session_id).await?;
    assert!(
        matches!(
            record.record.snapshot().status,
            SessionStatus::Suspended(Suspension::Interrupted { .. })
        ),
        "{} did not stop as interrupted",
        provider.name()
    );

    Ok(())
}

fn shell_interrupt_policy() -> PermissionPolicy {
    let root = std::env::current_dir().expect("current dir");
    PermissionPolicy::filesystem_default(root)
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("sleep".to_owned()),
            PermissionEffect::allow(),
        )
        .with_command_rule(
            PermissionToolSelector::exact("shell"),
            PermissionCommandPattern::Name("echo".to_owned()),
            PermissionEffect::allow(),
        )
}

async fn run_until_shell_call_then_cancel<S>(
    run: AgentRunBuilder<S>,
    cancellation_token: CancellationToken,
    expected_command: &'static str,
) -> waltr_agent::Result<Vec<AgentTimelineEvent>>
where
    S: waltr_agent::SessionStore + Clone + Send + Sync + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move { drain_agent_run_with_events(run, tx).await });

    while let Some(event) = rx.recv().await {
        if shell_tool_call_contains(&event, expected_command) {
            time::sleep(time::Duration::from_millis(500)).await;
            cancellation_token.cancel();
            break;
        }
    }

    handle.await.expect("agent run task panicked")
}

async fn drain_agent_run<S>(run: AgentRunBuilder<S>) -> waltr_agent::Result<Vec<AgentTimelineEvent>>
where
    S: waltr_agent::SessionStore + Clone + Send + Sync + 'static,
{
    let (tx, _rx) = mpsc::unbounded_channel();
    drain_agent_run_with_events(run, tx).await
}

async fn drain_agent_run_with_events<S>(
    run: AgentRunBuilder<S>,
    tx: mpsc::UnboundedSender<AgentTimelineEvent>,
) -> waltr_agent::Result<Vec<AgentTimelineEvent>>
where
    S: waltr_agent::SessionStore + Clone + Send + Sync + 'static,
{
    let mut stream = run.stream();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        drop(tx.send(event.clone()));
        events.push(event);
    }
    Ok(events)
}

fn assert_shell_tool_called(
    provider: &Arc<impl LlmStream + 'static>,
    events: &[AgentTimelineEvent],
    command: &str,
) {
    assert!(
        events
            .iter()
            .any(|event| shell_tool_call_contains(event, command)),
        "{} did not call shell command {command:?}",
        provider.name()
    );
}

fn assert_shell_tool_result_contains(
    provider: &Arc<impl LlmStream + 'static>,
    events: &[AgentTimelineEvent],
    expected: &str,
) {
    assert!(
        events.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                && matches!(&event.content, TimelineContent::ToolResult { result } if result.content.contains(expected))
        }),
        "{} did not produce shell result containing {expected:?}",
        provider.name()
    );
}

fn shell_tool_call_contains(event: &AgentTimelineEvent, command: &str) -> bool {
    crate::session::channel_is(event, CHANNEL_TOOL_CALL)
        && matches!(
            &event.content,
            TimelineContent::ToolCall { call }
                if call.name == "shell" && call.arguments.to_string().contains(command)
        )
}
