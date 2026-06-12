//! Agent question tool integration tests.

use waltr_agent::{
    Agent, BoxStreamFuture, CompletionRequest, LlmStream, ModelRef, ToolDefinition,
    middleware::ExternalRequestKind,
    tools::question::{QUESTION_TOOL_NAME, QuestionConfig},
};
use std::sync::Mutex;

use crate::{live_provider, session::AgentTestSession};

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_question_agent_loop_asks_and_uses_structured_answers() -> waltr_agent::Result<()> {
    let live =
        live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")?;
    run_live_question(live.provider, live.model).await
}

async fn run_live_question(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
    let provider = ToolCapturingProvider::new(provider);
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

struct ToolCapturingProvider<P> {
    inner: P,
    tools_by_turn: Mutex<Vec<Vec<ToolDefinition>>>,
}

impl<P> ToolCapturingProvider<P> {
    const fn new(inner: P) -> Self {
        Self {
            inner,
            tools_by_turn: Mutex::new(Vec::new()),
        }
    }

    fn all_tools(&self) -> Vec<ToolDefinition> {
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .iter()
            .flatten()
            .cloned()
            .collect()
    }

    fn tool_context(&self) -> String {
        let turns = self.tools_by_turn.lock().expect("captured tools").clone();
        if turns.is_empty() {
            return "captured provider tools: no provider requests captured".to_owned();
        }
        let lines = turns
            .iter()
            .enumerate()
            .map(|(index, tools)| {
                let names = tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("turn {index}: {names}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("captured provider tools:\n{lines}")
    }
}

impl<P: LlmStream> LlmStream for ToolCapturingProvider<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .push(request.tools.clone());
        self.inner.stream_request(request)
    }
}

fn assert_tool_was_surfaced(provider: &ToolCapturingProvider<impl LlmStream>, name: &str) {
    assert!(
        provider.all_tools().iter().any(|tool| tool.name == name),
        "{}",
        provider.tool_context()
    );
}

fn test_context(
    provider: &ToolCapturingProvider<impl LlmStream>,
    session: &AgentTestSession,
) -> String {
    format!(
        "{}\n{}",
        crate::session::session_context(provider, session),
        provider.tool_context()
    )
}
