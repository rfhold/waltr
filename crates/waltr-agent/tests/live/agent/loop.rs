//! Live provider agent-loop integration tests.

use waltr_agent::{
    Agent, AgentTool, LlmError, LlmStream, ModelRef, SessionSnapshot, SessionStatus, ToolDefinition,
};
use serde_json::json;

use crate::{live_provider, session::AgentTestSession};

const STATUS: &str = "operational-waltr-status-4821";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn anthropic_messages_streams_agent_tool_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::anthropic_messages())?
    else {
        return Ok(());
    };
    assert_provider_runs_status_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_chat_completions_streams_agent_tool_loop() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_chat_completions())?
    else {
        return Ok(());
    };
    assert_provider_runs_status_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_streams_agent_tool_loop() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::openai_responses())?
    else {
        return Ok(());
    };
    assert_provider_runs_status_agent(live.provider, live.model).await
}

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn openai_responses_websocket_streams_agent_tool_loop() -> waltr_agent::Result<()> {
    let Some(live) =
        live_provider::skip_if_missing_config(live_provider::openai_responses_websocket())?
    else {
        return Ok(());
    };
    assert_provider_runs_status_agent(live.provider, live.model).await
}

async fn assert_provider_runs_status_agent(
    provider: impl LlmStream,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let agent = Agent::builder(model).with_tool(get_status_tool()).build();
    let session = AgentTestSession::run(
        &agent,
        &provider,
        "Use get_status to check the current status. After the tool returns, report only the exact status value and no other text.",
    )
    .await?;
    crate::session::report_tool_errors(&provider, &session, "status loop");
    let context = crate::session::session_context(&provider, &session);

    let final_snapshot = session
        .final_snapshot
        .as_ref()
        .ok_or_else(|| LlmError::Protocol {
            provider: provider.name(),
            message: "agent stream ended without final snapshot".to_owned(),
        })?;

    assert_final_snapshot(final_snapshot, &context);
    assert!(session.saw_text_after_tool_result, "{context}");

    Ok(())
}

fn get_status_tool() -> AgentTool {
    AgentTool::new(
        ToolDefinition::new(
            "get_status",
            "Returns the current status.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        |_| Ok(STATUS.to_owned()),
    )
}

fn assert_final_snapshot(snapshot: &SessionSnapshot, context: &str) {
    assert_eq!(snapshot.status, SessionStatus::Finished, "{context}");
    assert_eq!(
        snapshot.state.last_assistant_text().trim(),
        STATUS,
        "{context}"
    );
    assert!(
        snapshot.state.events.iter().any(
            |event| matches!(event, waltr_agent::AgentTranscriptEvent::ToolCall { call, .. } if call.name == "get_status")
        ),
        "{context}"
    );
    assert!(
        snapshot.state.events.iter().any(
            |event| matches!(event, waltr_agent::AgentTranscriptEvent::ToolResult { result, .. } if result.name == "get_status" && result.content == STATUS)
        ),
        "{context}"
    );
}
