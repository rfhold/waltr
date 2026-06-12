//! Live provider agent shell integration tests.

use waltr_agent::{
    Agent, AgentTranscriptEvent, LlmStream, ModelRef, PermissionCommandPattern, PermissionEffect,
    PermissionPolicy, PermissionToolSelector, tools::shell::ShellConfig,
};

use crate::{live_provider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let live = live_provider::anthropic_messages()?;
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let live = live_provider::openai_chat_completions()?;
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let live = live_provider::openai_responses()?;
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_runs_shell_agent_loop() -> waltr_agent::Result<()> {
    let live = live_provider::openai_responses_websocket()?;
    assert_provider_runs_shell_agent(live.provider, live.model).await
}

async fn assert_provider_runs_shell_agent(
    provider: impl LlmStream,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let root = std::env::current_dir()?;
    let agent = Agent::builder(model)
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

    let events = session
        .final_snapshot
        .as_ref()
        .map(|snapshot| snapshot.state.events.as_slice())
        .unwrap_or_default();
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentTranscriptEvent::ToolCall { call, .. } if call.name == "shell"
        )),
        "{context}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentTranscriptEvent::ToolResult { result, .. } if result.content.contains("waltr-shell-ok")
    )), "{context}");
    assert!(session.text.contains("waltr-shell-ok"), "{context}");

    Ok(())
}
