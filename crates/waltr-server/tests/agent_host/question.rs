use crate::host_tests::scripted_provider::{CapturingProvider, ScriptedProvider};
use crate::{
    agent_host::{HostRuntimeBuilder, question::QuestionConfig},
    interaction::{Interaction, InteractionBroker, InteractionResponse},
    permission::ServerPermissionPolicy,
};
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use waltr_agent::*;

#[tokio::test]
async fn question_uses_original_live_turn_and_validates_schema_and_answers() {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "question",
        |request| {
            if request.tool_results().next().is_none() {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "ask",
                    "question",
                    json!({"questions":[
                        {"question":"Which implementation path?", "header":"Decision", "options":[{"label":"Plan", "description":"Implement"}], "multiple":false, "custom":false},
                        {"question":"Which tests?", "header":"Tests", "options":[{"label":"Deterministic", "description":"Scripted"},{"label":"Live", "description":"Provider"}], "multiple":true, "custom":false}
                    ]}),
                )))]
            } else {
                let result = request.tool_results().next().unwrap();
                let output: serde_json::Value =
                    serde_json::from_str(result.content.as_str()).unwrap();
                assert_eq!(output["answers"][0]["selections"], json!(["Plan"]));
                assert_eq!(
                    output["answers"][1]["selections"],
                    json!(["Deterministic", "Live"])
                );
                vec![
                    Ok(CompletionEvent::Delta("question-flow-complete".into())),
                    Ok(CompletionEvent::Finished),
                ]
            }
        },
    )));
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let broker = InteractionBroker::new();
    let runtime = HostRuntimeBuilder::new(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider.clone(),
    )
    .with_question_tool(QuestionConfig::new())
    .with_dispatch_context(
        Some(id.clone()),
        ServerPermissionPolicy::sessions(broker.clone()),
    )
    .build();
    let runs = sessions.clone();
    let run_id = id.clone();
    let owner = tokio::spawn(async move {
        runs.prompt(&runtime, run_id, "ask structured questions")
            .stream()
            .collect::<Vec<_>>()
            .await
    });
    let mut watch = broker.subscribe(id.clone());
    let snapshot = loop {
        let next = watch.next().await.unwrap();
        if !next.pending.is_empty() {
            break next;
        }
    };
    assert!(!owner.is_finished());
    let request = &snapshot.pending[0];
    let Interaction::Question { input } = &request.request else {
        panic!("question interaction")
    };
    assert_eq!(input.questions.len(), 2);
    let tool = provider.tool_named("question").unwrap();
    for (pointer, expected) in [
        ("/type", "object"),
        ("/properties/questions/type", "array"),
        (
            "/properties/questions/items/properties/question/type",
            "string",
        ),
        (
            "/properties/questions/items/properties/header/type",
            "string",
        ),
        (
            "/properties/questions/items/properties/options/type",
            "array",
        ),
        (
            "/properties/questions/items/properties/options/items/properties/label/type",
            "string",
        ),
        (
            "/properties/questions/items/properties/options/items/properties/description/type",
            "string",
        ),
        (
            "/properties/questions/items/properties/multiple/type",
            "boolean",
        ),
        (
            "/properties/questions/items/properties/custom/type",
            "boolean",
        ),
    ] {
        assert_eq!(tool.input_schema.pointer(pointer), Some(&json!(expected)));
    }
    let respond = |answers| {
        broker.respond(
            &id,
            &snapshot.generation,
            &request.id,
            &request.operation_id,
            InteractionResponse::Question(answers),
        )
    };
    assert!(respond(vec![vec!["unlisted".into()], vec!["Live".into()]]).is_err());
    assert_eq!(broker.snapshot(&id).pending.len(), 1);
    let answers = vec![
        vec!["Plan".into()],
        vec!["Deterministic".into(), "Live".into()],
    ];
    respond(answers.clone()).unwrap();
    assert!(respond(answers).is_err());
    let events = owner
        .await
        .unwrap()
        .into_iter()
        .collect::<waltr_agent::Result<Vec<_>>>()
        .unwrap();
    assert!(events.iter().any(|event| matches!(&event.content, TimelineContent::TextDelta { delta } if delta == "question-flow-complete")));
    assert!(broker.snapshot(&id).pending.is_empty());
    let record = sessions.store().load(&id).await.unwrap();
    assert_eq!(record.provider_requests().count(), 2);
    assert_eq!(
        record
            .snapshot()
            .tool_results()
            .filter(|result| result.call_id == "ask")
            .count(),
        1
    );
    sessions.shutdown().await;
}

#[tokio::test]
async fn unanswerable_question_fails_without_creating_an_interaction() {
    let provider = Arc::new(ScriptedProvider::from_turns(
        "invalid-question",
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "invalid",
                "question",
                json!({"questions":[{"question":"How?", "header":"Blocked", "options":[], "multiple":false, "custom":false}]}),
            ))],
            vec![CompletionEvent::Finished],
        ],
    ));
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let broker = InteractionBroker::new();
    let runtime = HostRuntimeBuilder::new(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider,
    )
    .with_question_tool(QuestionConfig::new())
    .with_dispatch_context(
        Some(id.clone()),
        ServerPermissionPolicy::sessions(broker.clone()),
    )
    .build();
    let events = sessions
        .prompt(&runtime, id.clone(), "invalid question")
        .stream()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|event| matches!(event, Ok(event) if matches!(&event.content, TimelineContent::ToolResult { result } if result.is_error && result.content.contains("has no options")))));
    assert!(broker.snapshot(&id).pending.is_empty());
    sessions.shutdown().await;
}
