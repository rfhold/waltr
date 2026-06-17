//! Live provider agent-loop integration tests.

use waltr_agent::{
    Agent, AgentTool, CHANNEL_TOOL_CALL, CHANNEL_TOOL_RESULT, LlmStream, ModelRef, TimelineContent,
    ToolDefinition,
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

    assert_finished_status_session(&session, &context);
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
        |context| Ok(context.result(STATUS)),
    )
}

fn assert_finished_status_session(session: &AgentTestSession, context: &str) {
    assert!(session.finished, "{context}");
    assert_eq!(session.text.trim(), STATUS, "{context}");
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_CALL)
                && matches!(&event.content, TimelineContent::ToolCall { call } if call.name == "get_status")
        }),
        "{context}"
    );
    assert!(
        session.timeline.iter().any(|event| {
            crate::session::channel_is(event, CHANNEL_TOOL_RESULT)
                && matches!(&event.content, TimelineContent::ToolResult { result } if result.name == "get_status" && result.content == STATUS)
        }),
        "{context}"
    );
}
