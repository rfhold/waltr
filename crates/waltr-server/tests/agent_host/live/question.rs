//! Server-owned live question tool integration tests.

use crate::agent_host::HostRuntimeBuilder;
use crate::agent_host::tools::question::{QUESTION_TOOL_NAME, QuestionConfig};
use crate::interaction::{Interaction, InteractionResponse};

use std::sync::Arc;

use waltr_agent::*;

use crate::host_tests::{live_provider, scripted_provider::CapturingProvider};

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

async fn run_live_question(
    provider: impl LlmStream + 'static,
    model: ModelRef,
) -> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(provider));
    let agent = crate::host_tests::harness::HostSession::new(
        HostRuntimeBuilder::new(model, Arc::clone(&provider))
            .with_question_tool(QuestionConfig::new()),
    )
    .await;
    let prompt = "Use the question tool exactly once to ask the user which implementation path to choose and which tests to include.";

    let owner = agent.start(prompt);
    let pending = crate::host_tests::pending(&agent.broker, &agent.id).await;
    assert_tool_was_surfaced(&provider, QUESTION_TOOL_NAME);
    let Interaction::Question { input } = &pending.pending[0].request else {
        panic!("question expected")
    };
    assert!(!owner.is_finished());
    let answers = input
        .questions
        .iter()
        .map(|item| vec![item.options.first().expect("offered option").label.clone()])
        .collect();
    crate::host_tests::respond(
        &agent.broker,
        &pending,
        InteractionResponse::Question(answers),
    )
    .unwrap();
    let events = owner
        .await
        .unwrap()
        .into_iter()
        .collect::<waltr_agent::Result<Vec<_>>>()?;
    assert!(events.iter().any(|event| matches!(&event.content, TimelineContent::ToolResult { result } if result.name == QUESTION_TOOL_NAME)));
    assert!(agent.broker.snapshot(&agent.id).pending.is_empty());
    agent.close().await;
    Ok(())
}

fn assert_tool_was_surfaced(provider: &Arc<CapturingProvider<impl LlmStream>>, name: &str) {
    assert!(
        provider.all_tools().iter().any(|tool| tool.name == name),
        "{}",
        provider.tool_context()
    );
}
