//! Agent question tool integration tests.

use waltr_agent::{
    Agent, LlmStream, ModelRef,
    middleware::ExternalRequestKind,
    tools::question::{QUESTION_TOOL_NAME, QuestionConfig},
};

use crate::{live_provider, scripted_provider::CapturingProvider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_question_agent_loop_asks_and_uses_structured_answers() -> waltr_agent::Result<()> {
    let Some(live) = live_provider::skip_if_missing_config(live_provider::selected_from_env(
        "WALTR_AGENT_LIVE_PROVIDER",
        "openai_responses_websocket",
    ))?
    else {
        return Ok(());
    };
    run_live_question(live.provider, live.model).await
}

async fn run_live_question(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
    let provider = CapturingProvider::new(provider);
    let agent = Agent::builder(model)
        .with_question_tool(QuestionConfig::new())
        .build();
    let prompt = "Use the question tool exactly once to ask the user which implementation path to choose and which tests to include.";

    let session = AgentTestSession::run(&agent, &provider, prompt).await?;
    crate::session::report_tool_errors(&provider, &session, "question live test");
    let context = test_context(&provider, &session);

    assert_tool_was_surfaced(&provider, QUESTION_TOOL_NAME);
    assert!(session.final_snapshot.is_none(), "{context}");
    assert!(
        session
            .external_requests
            .iter()
            .any(|request| request.kind == ExternalRequestKind::Question),
        "{context}"
    );

    Ok(())
}

fn assert_tool_was_surfaced(provider: &CapturingProvider<impl LlmStream>, name: &str) {
    assert!(
        provider.all_tools().iter().any(|tool| tool.name == name),
        "{}",
        provider.tool_context()
    );
}

fn test_context(
    provider: &CapturingProvider<impl LlmStream>,
    session: &AgentTestSession,
) -> String {
    format!(
        "{}\n{}",
        crate::session::session_context(provider, session),
        provider.tool_context()
    )
}
