//! Live provider agent shell integration tests.

use std::sync::Arc;

use waltr_agent::{
    AgentRuntime, CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT, LlmStream, ModelRef,
    PermissionCommandPattern, PermissionEffect, PermissionPolicy, PermissionToolSelector,
    TimelineContent, tools::shell::ShellConfig,
};

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
