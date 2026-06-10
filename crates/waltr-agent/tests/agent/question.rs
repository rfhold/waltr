//! Agent question tool integration tests.

use std::sync::Mutex;

use futures_util::stream;
use waltr_agent::{
    Agent, AgentTranscriptEvent, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
    CompletionRequest, LlmStream, ModelApi, ModelRef, ProviderId, ToolCall, ToolDefinition,
    middleware::ExternalRequestKind,
    tools::question::{QUESTION_TOOL_NAME, QuestionConfig},
};
use serde_json::{Value, json};

use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
#[tokio::test]
// TODO(agent-runtime): Add durable question suspension/resume coverage around external requests.
async fn question_agent_loop_surfaces_tool_and_suspends_for_external_answers()
-> waltr_agent::Result<()> {
    let provider = ToolCapturingProvider::new(ScriptedQuestionProvider::new(
        structured_question_arguments(),
    ));
    let agent = Agent::builder(test_model())
        .with_question_tool(QuestionConfig::new())
        .build();

    let session = AgentTestSession::run(&agent, &provider, "ask structured questions").await?;
    let context = test_context(&provider, &session);
    let question_tool = provider
        .tool_named(QUESTION_TOOL_NAME)
        .unwrap_or_else(|| panic!("question tool should be surfaced\n{context}"));

    assert_question_schema(&question_tool, &context);
    assert!(
        session.final_state.is_none(),
        "suspended run should not emit final state\n{context}"
    );
    assert!(
        session.text.is_empty(),
        "suspended run should not continue: {context}"
    );

    let [request] = session.external_requests.as_slice() else {
        panic!("expected one external question request: {context}");
    };
    assert_eq!(request.kind, ExternalRequestKind::Question);
    assert_eq!(request.tool_call_id.as_deref(), Some("ask_question"));
    assert!(request.id.starts_with("question:ephemeral:0:ask_question"));
    assert!(request.prompt.contains("Which implementation path"));
    assert_eq!(request.payload, structured_question_arguments());

    Ok(())
}

#[tokio::test]
async fn question_tool_rejects_unanswerable_prompt_without_options_or_custom()
-> waltr_agent::Result<()> {
    let provider = ToolCapturingProvider::new(ScriptedQuestionProvider::new(json!({
        "questions": [{
            "question": "How should this be answered?",
            "header": "Blocked",
            "options": [],
            "multiple": false,
            "custom": false
        }]
    })));
    let agent = Agent::builder(test_model())
        .with_question_tool(QuestionConfig::new())
        .build();

    let session = AgentTestSession::run(&agent, &provider, "ask an invalid question").await?;
    let context = test_context(&provider, &session);
    let results = tool_result_text(&session);

    assert!(results.contains("Tool error:"), "{context}");
    assert!(
        results.contains("has no options and custom answers are disabled"),
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

    fn tool_named(&self, name: &str) -> Option<ToolDefinition> {
        self.all_tools().into_iter().find(|tool| tool.name == name)
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
        // TODO(agent-runtime): Capture provider projections and request records, not only tools.
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .push(request.tools.clone());
        self.inner.stream_request(request)
    }
}

struct ScriptedQuestionProvider {
    arguments: Value,
}

impl ScriptedQuestionProvider {
    const fn new(arguments: Value) -> Self {
        Self { arguments }
    }
}

impl LlmStream for ScriptedQuestionProvider {
    fn name(&self) -> &'static str {
        "scripted_question_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        // TODO(agent-runtime): Resume scripted question flow from persisted pending request state.
        let events = if request.tool_results().next().is_none() {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "ask_question",
                QUESTION_TOOL_NAME,
                self.arguments.clone(),
            )))]
        } else {
            vec![Ok(CompletionEvent::Finished)]
        };
        Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
    }
}

fn structured_question_arguments() -> Value {
    json!({
        "questions": [
            {
                "question": "Which implementation path should we take?",
                "header": "Decision",
                "options": [
                    {
                        "label": "Plan",
                        "description": "Implement the planned library-only question tool."
                    },
                    {
                        "label": "Defer",
                        "description": "Do not implement the tool yet."
                    }
                ],
                "multiple": false
            },
            {
                "question": "Which tests should be included?",
                "header": "Tests",
                "options": [
                    {
                        "label": "Deterministic",
                        "description": "Add scripted agent-loop coverage."
                    },
                    {
                        "label": "Live",
                        "description": "Add ignored live-provider coverage."
                    }
                ],
                "multiple": true,
                "custom": false
            }
        ]
    })
}

fn assert_question_schema(tool: &ToolDefinition, context: &str) {
    let schema = &tool.input_schema;
    assert_schema_value(schema, "/type", &json!("object"), context);
    assert_schema_value(
        schema,
        "/properties/questions/type",
        &json!("array"),
        context,
    );
    assert_schema_value(schema, "/properties/questions/minItems", &json!(1), context);
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/question/type",
        &json!("string"),
        context,
    );
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/header/type",
        &json!("string"),
        context,
    );
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/options/type",
        &json!("array"),
        context,
    );
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/options/items/properties/label/type",
        &json!("string"),
        context,
    );
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/options/items/properties/description/type",
        &json!("string"),
        context,
    );
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/multiple/type",
        &json!("boolean"),
        context,
    );
    assert_schema_value(
        schema,
        "/properties/questions/items/properties/custom/type",
        &json!("boolean"),
        context,
    );
}

fn assert_schema_value(schema: &Value, pointer: &str, expected: &Value, context: &str) {
    assert_eq!(
        schema.pointer(pointer),
        Some(expected),
        "schema missing {pointer}\n{context}\n{schema}"
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

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}
