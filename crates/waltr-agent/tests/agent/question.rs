//! AgentRuntime question tool integration tests.

use std::sync::Arc;

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentSessions, CHANNEL_ASSISTANT_TEXT, CHANNEL_EXTERNAL_REQUEST, CompletionEvent,
    ExternalRequestId, ExternalRequestKind, ExternalRequestStatus, ExternalResponse, LlmStream,
    MemorySessionStore, ModelApi, ModelRef, ProviderId, ResumeInput, SessionStatus, SessionStore,
    TimelineContent, ToolCall, ToolDefinition,
    tools::question::{QUESTION_TOOL_NAME, QuestionConfig},
};
use serde_json::{Value, json};

use crate::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::session::AgentTestSession;

const OPENAI_TEST_MODEL: &str = "gpt-5-nano";
#[tokio::test]
async fn question_agent_loop_surfaces_tool_and_suspends_for_external_answers()
-> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(scripted_question_provider(
        structured_question_arguments(),
    )));
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_question_tool(QuestionConfig::new())
        .build();

    let session = AgentTestSession::run(&agent, &provider, "ask structured questions").await?;
    let context = test_context(&provider, &session);
    let question_tool = provider
        .tool_named(QUESTION_TOOL_NAME)
        .unwrap_or_else(|| panic!("question tool should be surfaced\n{context}"));

    assert_question_schema(&question_tool, &context);
    assert!(
        !session.finished,
        "suspended run should not emit finished status\n{context}"
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
    assert!(
        request
            .id
            .as_str()
            .starts_with("question:ephemeral:0:ask_question")
    );
    assert!(request.prompt.contains("Which implementation path"));
    assert_eq!(request.payload, structured_question_arguments());

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn question_external_request_resumes_from_durable_session() -> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(scripted_question_provider(
        structured_question_arguments(),
    )));
    let agent = AgentRuntime::builder(test_model(), provider.clone())
        .with_question_tool(QuestionConfig::new())
        .build();
    let store = MemorySessionStore::new();
    let sessions = AgentSessions::new(store.clone());
    let session_id = sessions.create().await?.id().clone();

    let mut stream = sessions
        .prompt(&agent, session_id.clone(), "ask structured questions")
        .stream();
    let mut request_id = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        if let TimelineContent::ExternalRequest { request } = &event.content {
            assert!(crate::session::channel_is(&event, CHANNEL_EXTERNAL_REQUEST));
            assert_eq!(request.kind, ExternalRequestKind::Question);
            assert_eq!(request.tool_call_id.as_deref(), Some("ask_question"));
            request_id = Some(request.id.as_str().to_owned());
        }
    }
    let request_id = request_id.expect("question request");

    let suspended = store.load(&session_id).await.expect("stored session");
    let suspended_snapshot = suspended.snapshot();
    let persisted_request = suspended_snapshot
        .external_requests
        .iter()
        .find(|request| request.id.as_str() == request_id)
        .expect("persisted question request");
    assert_eq!(persisted_request.kind, ExternalRequestKind::Question);
    assert_eq!(
        persisted_request.tool_call_id.as_deref(),
        Some("ask_question")
    );
    assert_eq!(persisted_request.status, ExternalRequestStatus::Pending);
    assert!(matches!(
        suspended_snapshot.status,
        SessionStatus::Suspended(_)
    ));
    assert!(
        suspended_snapshot
            .pending_work
            .iter()
            .any(|work| work.id == "ask_question")
    );
    let request_record = suspended
        .provider_requests()
        .next()
        .expect("request record");
    assert_eq!(
        request_record.projected_request,
        json!({ "provider": "scripted_question_provider", "prompt": "ask structured questions" })
    );

    let mut resumed = sessions
        .resume(
            &agent,
            session_id.clone(),
            ResumeInput::ExternalResponse {
                request_id: ExternalRequestId::new(request_id.clone()),
                response: ExternalResponse::new(json!(["Plan", "Deterministic"])),
            },
        )
        .stream();
    let mut text = String::new();
    while let Some(event) = resumed.next().await {
        let event = event?;
        if let TimelineContent::TextDelta { delta } = &event.content
            && crate::session::channel_is(&event, CHANNEL_ASSISTANT_TEXT)
        {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "question-flow-complete");

    let completed = store.load(&session_id).await.expect("stored session");
    let completed_snapshot = completed.snapshot();
    let responded_request = completed_snapshot
        .external_requests
        .iter()
        .find(|request| request.id.as_str() == request_id)
        .expect("responded question request");
    assert!(matches!(
        responded_request.status,
        ExternalRequestStatus::Responded(_)
    ));
    assert!(completed_snapshot.tool_results().any(|result| {
        result.name == QUESTION_TOOL_NAME
            && result.call_id == "ask_question"
            && result.content.contains("Deterministic")
    }));
    assert_eq!(
        provider.request_prompts(),
        vec![
            "ask structured questions".to_owned(),
            "ask structured questions".to_owned(),
        ]
    );

    Ok(())
}

#[tokio::test]
async fn question_tool_rejects_unanswerable_prompt_without_options_or_custom()
-> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(scripted_question_provider(json!({
        "questions": [{
            "question": "How should this be answered?",
            "header": "Blocked",
            "options": [],
            "multiple": false,
            "custom": false
        }]
    }))));
    let agent = AgentRuntime::builder(test_model(), provider.clone())
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

fn scripted_question_provider(arguments: Value) -> ScriptedProvider {
    ScriptedProvider::new("scripted_question_provider", move |request| {
        if request.tool_results().next().is_none() {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "ask_question",
                QUESTION_TOOL_NAME,
                arguments.clone(),
            )))]
        } else {
            let result = request
                .tool_results()
                .find(|result| result.call_id == "ask_question")
                .expect("resumed question result");
            if !result.content.starts_with("Tool error:") {
                assert!(result.content.contains("Deterministic"), "{result:?}");
            }
            vec![
                Ok(CompletionEvent::Delta("question-flow-complete".to_owned())),
                Ok(CompletionEvent::Finished),
            ]
        }
    })
    .with_projection(|request| {
        Some(json!({
            "provider": "scripted_question_provider",
            "prompt": request.prompt,
        }))
    })
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
    session
        .tool_results
        .iter()
        .map(|result| result.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
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

fn test_model() -> ModelRef {
    ModelRef::new(
        ProviderId::OpenAi,
        OPENAI_TEST_MODEL,
        ModelApi::OpenAiResponses,
    )
}
