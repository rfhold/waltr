//! Agent question tool integration tests.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures_util::stream;
use waltr_agent::{
    Agent, AgentTranscriptEvent, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
    CompletionRequest, LlmStream, ModelApi, ModelRef, ProviderId, ToolCall, ToolDefinition,
    tools::question::{QUESTION_TOOL_NAME, QuestionAnswer, QuestionConfig, QuestionOutput},
};
use serde_json::{Value, json};

use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
const QUESTION_FLOW_COMPLETE: &str = "question-flow-complete";
#[tokio::test]
async fn question_agent_loop_surfaces_tool_and_returns_structured_answers() -> waltr_agent::Result<()>
{
    let callback_inputs = Arc::new(Mutex::new(Vec::new()));
    let handler_inputs = Arc::clone(&callback_inputs);
    let provider = ToolCapturingProvider::new(ScriptedQuestionProvider::new(
        structured_question_arguments(),
        QUESTION_FLOW_COMPLETE,
    ));
    let agent = Agent::builder(test_model())
        .with_question_tool(QuestionConfig::new(move |input| {
            handler_inputs.lock().expect("callback inputs").push(input);
            Ok(deterministic_answers())
        }))
        .build();

    let session = AgentTestSession::run(&agent, &provider, "ask structured questions").await?;
    session.require_tool_execution(provider.name())?;
    let context = test_context(&provider, &session);
    let question_tool = provider
        .tool_named(QUESTION_TOOL_NAME)
        .unwrap_or_else(|| panic!("question tool should be surfaced\n{context}"));

    assert_question_schema(&question_tool, &context);
    assert_question_tool_call(&session, &context);
    assert!(session.text.contains(QUESTION_FLOW_COMPLETE), "{context}");

    let results = tool_result_text(&session);
    assert!(results.contains("selected-plan-101"), "{context}");
    assert!(results.contains("selected-test-202"), "{context}");
    assert!(results.contains("custom-note-303"), "{context}");

    let input = {
        let inputs = callback_inputs.lock().expect("callback inputs");
        assert_eq!(inputs.len(), 1, "{context}");
        inputs[0].clone()
    };
    let questions = &input.questions;
    assert_eq!(questions.len(), 2, "{questions:#?}");
    assert_eq!(questions[0].header, "Decision");
    assert_eq!(questions[0].options[0].label, "Plan");
    assert!(!questions[0].multiple);
    assert!(questions[0].custom, "custom should default to true");
    assert_eq!(questions[1].header, "Tests");
    assert_eq!(questions[1].options[0].label, "Deterministic");
    assert!(questions[1].multiple);
    assert!(!questions[1].custom);

    Ok(())
}

#[tokio::test]
async fn question_tool_rejects_unanswerable_prompt_without_options_or_custom()
-> waltr_agent::Result<()> {
    let callback_calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&callback_calls);
    let provider = ToolCapturingProvider::new(ScriptedQuestionProvider::new(
        json!({
            "questions": [{
                "question": "How should this be answered?",
                "header": "Blocked",
                "options": [],
                "multiple": false,
                "custom": false
            }]
        }),
        "question-error-flow-complete",
    ));
    let agent = Agent::builder(test_model())
        .with_question_tool(QuestionConfig::new(move |_input| {
            handler_calls.fetch_add(1, Ordering::SeqCst);
            Ok(QuestionOutput {
                answers: Vec::new(),
            })
        }))
        .build();

    let session = AgentTestSession::run(&agent, &provider, "ask an invalid question").await?;
    let context = test_context(&provider, &session);
    let results = tool_result_text(&session);

    assert_eq!(callback_calls.load(Ordering::SeqCst), 0, "{context}");
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
        self.tools_by_turn
            .lock()
            .expect("captured tools")
            .push(request.tools.clone());
        self.inner.stream_request(request)
    }
}

struct ScriptedQuestionProvider {
    arguments: Value,
    final_text: &'static str,
}

impl ScriptedQuestionProvider {
    const fn new(arguments: Value, final_text: &'static str) -> Self {
        Self {
            arguments,
            final_text,
        }
    }
}

impl LlmStream for ScriptedQuestionProvider {
    fn name(&self) -> &'static str {
        "scripted_question_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        let events = if request.tool_results.is_empty() {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "ask_question",
                QUESTION_TOOL_NAME,
                self.arguments.clone(),
            )))]
        } else {
            vec![
                Ok(CompletionEvent::Delta(self.final_text.to_owned())),
                Ok(CompletionEvent::Finished),
            ]
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

fn deterministic_answers() -> QuestionOutput {
    QuestionOutput {
        answers: vec![
            QuestionAnswer {
                header: "Decision".to_owned(),
                selections: vec!["selected-plan-101".to_owned()],
                custom: Some("custom-note-303".to_owned()),
            },
            QuestionAnswer {
                header: "Tests".to_owned(),
                selections: vec!["selected-test-202".to_owned()],
                custom: None,
            },
        ],
    }
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

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}
