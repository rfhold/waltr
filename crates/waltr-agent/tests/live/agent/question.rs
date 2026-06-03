//! Agent question tool integration tests.

use waltr_agent::{
    Agent, AgentTranscriptEvent, BoxStreamFuture, CompletionRequest, LlmStream, ModelRef,
    ToolDefinition,
    tools::question::{
        QUESTION_TOOL_NAME, QuestionAnswer, QuestionConfig, QuestionInput, QuestionOutput,
    },
};
use std::sync::{Arc, Mutex};

use crate::{live_provider, session::AgentTestSession};

const QUESTION_LIVE_FLOW_COMPLETE: &str = "question-live-flow-complete";

#[tokio::test]
#[ignore = "requires live provider API keys or WALTR_AGENT_LIVE_GATEWAY=1"]
async fn live_question_agent_loop_asks_and_uses_structured_answers() -> waltr_agent::Result<()> {
    let live =
        live_provider::selected_from_env("WALTR_AGENT_LIVE_PROVIDER", "openai_responses_websocket")?;
    run_live_question(live.provider, live.model).await
}

async fn run_live_question(provider: impl LlmStream, model: ModelRef) -> waltr_agent::Result<()> {
    let callback_inputs = Arc::new(Mutex::new(Vec::new()));
    let handler_inputs = Arc::clone(&callback_inputs);
    let provider = ToolCapturingProvider::new(provider);
    let agent = Agent::builder(model)
        .with_question_tool(QuestionConfig::new(move |input| {
            let output = live_answers(&input);
            handler_inputs.lock().expect("callback inputs").push(input);
            Ok(output)
        }))
        .build();
    let prompt = "Use the question tool exactly once to ask the user which implementation path to choose and which tests to include. After the question tool returns, report a concise final status containing every exact marker returned by the question tool: question-live-choice-101, question-live-test-202, question-live-custom-303, and question-live-flow-complete.";

    let session = AgentTestSession::run(&agent, &provider, prompt).await?;
    session.require_tool_execution(provider.name())?;
    crate::session::report_tool_errors(&provider, &session, "question live test");
    let context = test_context(&provider, &session);

    assert_tool_was_surfaced(&provider, QUESTION_TOOL_NAME);
    assert_question_tool_call(&session, &context);
    assert!(
        tool_result_text(&session).contains("question-live-choice-101"),
        "{context}"
    );
    assert!(
        tool_result_text(&session).contains("question-live-test-202"),
        "{context}"
    );
    assert!(
        tool_result_text(&session).contains("question-live-custom-303"),
        "{context}"
    );
    assert!(
        session.text.contains("question-live-choice-101"),
        "{context}"
    );
    assert!(session.text.contains("question-live-test-202"), "{context}");
    assert!(
        session.text.contains("question-live-custom-303"),
        "{context}"
    );
    assert!(
        session.text.contains(QUESTION_LIVE_FLOW_COMPLETE),
        "{context}"
    );

    let inputs = callback_inputs.lock().expect("callback inputs").clone();
    assert!(!inputs.is_empty(), "{context}");
    assert!(
        inputs.iter().any(|input| !input.questions.is_empty()),
        "{inputs:#?}"
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

fn live_answers(input: &QuestionInput) -> QuestionOutput {
    QuestionOutput {
        answers: input
            .questions
            .iter()
            .map(|prompt| QuestionAnswer {
                header: prompt.header.clone(),
                selections: if prompt.multiple {
                    vec![
                        "question-live-choice-101".to_owned(),
                        "question-live-test-202".to_owned(),
                    ]
                } else {
                    vec!["question-live-choice-101".to_owned()]
                },
                custom: Some("question-live-test-202 question-live-custom-303".to_owned()),
            })
            .collect(),
    }
}

fn assert_tool_was_surfaced(provider: &ToolCapturingProvider<impl LlmStream>, name: &str) {
    assert!(
        provider.all_tools().iter().any(|tool| tool.name == name),
        "{}",
        provider.tool_context()
    );
}

fn assert_question_tool_call(session: &AgentTestSession, context: &str) {
    let Some(state) = &session.final_state else {
        panic!("missing final state\n{context}");
    };
    assert!(
        state.events.iter().any(|event| matches!(
            event,
            AgentTranscriptEvent::ToolCall { call, .. } if call.name == QUESTION_TOOL_NAME
        )),
        "{context}"
    );
    assert!(
        state.events.iter().any(|event| matches!(
            event,
            AgentTranscriptEvent::ToolResult { result, .. } if result.name == QUESTION_TOOL_NAME
        )),
        "{context}"
    );
}

fn tool_result_text(session: &AgentTestSession) -> String {
    let Some(state) = &session.final_state else {
        return String::new();
    };
    state
        .events
        .iter()
        .filter_map(|event| match event {
            AgentTranscriptEvent::ToolResult { result, .. } => Some(result.content.as_str()),
            AgentTranscriptEvent::Thinking { .. }
            | AgentTranscriptEvent::Text { .. }
            | AgentTranscriptEvent::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
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
