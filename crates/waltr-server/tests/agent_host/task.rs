//! Server-owned task tool integration tests.

use crate::agent_host::AgentDefinitionBundle;
use crate::agent_host::runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory;
use crate::auth::FileCredentialStore;
use crate::interaction::InteractionBroker;
use crate::permission::*;

use std::sync::Arc;

use crate::agent_host::tools::task::TASK_TOOL_NAME;
use futures_util::{StreamExt as _, stream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, timeout};
use waltr_agent::*;

use crate::host_tests::sandbox::TestSandbox;
use crate::host_tests::scripted_provider::{CapturingProvider, ScriptedProvider};

use crate::agent_host::session_roles::{SessionRole, SessionRoles, TaskOrigin};

#[derive(Clone)]
struct TaskFact {
    session_id: SessionId,
    revision: Revision,
    origin: TaskOrigin,
}
#[derive(Default)]
struct SemanticSink(std::sync::Mutex<Vec<TaskFact>>, std::sync::Mutex<Option<SessionRoles>>);
impl waltr_agent::SessionEventSink for SemanticSink {
    fn send(&self, _: waltr_agent::SessionEvent) -> waltr_agent::SessionEventSinkFuture {
        Box::pin(async { Ok(()) })
    }
    fn semantic(&self, event: waltr_agent::SessionSemanticEvent) {
        if let Some(origin) = self.1.lock().unwrap().as_ref().and_then(|roles| roles.complete_task(&event)) {
            self.0.lock().unwrap().push(TaskFact { session_id: event.session_id, revision: event.revision, origin });
        }
    }
}

#[tokio::test]
async fn task_completion_is_an_actual_durable_child_fact_not_mutable_metadata() {
    let provider = Arc::new(ScriptedProvider::new("task-facts", |request| {
        if request.prompt == "child" {
            return vec![
                Ok(CompletionEvent::Delta("child result".into())),
                Ok(CompletionEvent::Finished),
            ];
        }
        if request.tool_results().next().is_none() {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "call",
                TASK_TOOL_NAME,
                json!({"prompt":"child","mode":"sync"}),
            )))]
        } else {
            vec![Ok(CompletionEvent::Finished)]
        }
    }));
    let sink = Arc::new(SemanticSink::default());
    let (sessions, _sandbox, _root) =
        task_sessions_with_sink(provider, 1, Some(sink.clone())).await;
    let mut request = waltr_agent::SessionCreateRequest::new("parent");
    request.inference_settings = Some(test_inference_settings());
    sessions.create(request).await.unwrap();
    let submission = sessions
        .submit_prompt(
            SessionId::new("parent"),
            "spawn",
            RunConflictPolicy::RejectIfActive,
        )
        .await
        .unwrap();
    submission.handle.wait().await.unwrap();
    let child_id = SessionId::new("task_parent_0_call");
    let child = sessions.load(&child_id).await.unwrap().record;
    assert!(
        child
            .journal
            .iter()
            .any(|event| matches!(event, SessionJournalEvent::Finished))
    );
    let facts = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].session_id, child_id);
    assert_eq!(facts[0].origin.parent.as_str(), "parent");
    assert_eq!(facts[0].origin.call, "call");
    assert_eq!(facts[0].origin.turn, 0);
    assert!(facts[0].revision <= child.revision);
    // A new run on this same child must not count the same task creation twice.
    sessions
        .submit_prompt(child_id, "child", RunConflictPolicy::RejectIfActive)
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    assert_eq!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .count(),
        1
    );
    sessions.shutdown().await;
}

#[tokio::test]
async fn deterministic_task_reuse_rejects_legacy_forged_and_wrong_creation_linkage() {
    for mismatch in ["legacy", "parent", "call", "incarnation"] {
        let provider = Arc::new(ScriptedProvider::new("task-reuse", |request| {
            assert_ne!(request.prompt, "child", "conflicting child must not run");
            if let Some(result) = request.tool_results().next() {
                assert!(result.is_error);
                assert!(result.content.contains("provenance mismatch"));
                vec![Ok(CompletionEvent::Finished)]
            } else {
                vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "call",
                    TASK_TOOL_NAME,
                    json!({"prompt":"child"}),
                )))]
            }
        }));
        let sink = Arc::new(SemanticSink::default());
        let (sessions, _sandbox, _root) = task_sessions_with_sink(provider, 1, Some(sink.clone())).await;
        let mut parent = waltr_agent::SessionCreateRequest::new("parent");
        parent.inference_settings = Some(test_inference_settings());
        let parent = sessions.create(parent).await.unwrap().record;
        let mut child = waltr_agent::SessionCreateRequest::new("task_parent_0_call");
        child.inference_settings = Some(test_inference_settings());
        child.metadata_values = std::collections::BTreeMap::from([
            ("task.parent_session_id".into(), json!("parent")),
            ("task.parent_turn".into(), json!(0)),
            ("task.tool_call_id".into(), json!("call")),
        ]);
        let before = sessions.create(child).await.unwrap().record;
        if mismatch != "legacy" {
            sink.1.lock().unwrap().as_ref().unwrap().register(&before, SessionRole::Task(TaskOrigin {
                parent: SessionId::new(if mismatch == "parent" { "wrong" } else { "parent" }),
                incarnation: if mismatch == "incarnation" { "wrong".into() } else { parent.metadata.incarnation.clone().unwrap() },
                turn: 0, call: if mismatch == "call" { "wrong".into() } else { "call".into() }, depth: 1,
            })).unwrap();
        }
        sessions
            .submit_prompt(parent.id, "spawn", RunConflictPolicy::RejectIfActive)
            .await
            .unwrap()
            .handle
            .wait()
            .await
            .unwrap();
        assert_eq!(sessions.load(&before.id).await.unwrap().record, before);
        assert!(sink.0.lock().unwrap().is_empty());
        sessions.shutdown().await;
    }
}

#[tokio::test]
async fn legacy_parent_without_incarnation_fails_closed_without_manufacturing_task_provenance() {
    let provider = Arc::new(ScriptedProvider::new("legacy-task", |request| {
        if request.prompt == "child" {
            return vec![
                Ok(CompletionEvent::Delta("legacy child result".into())),
                Ok(CompletionEvent::Finished),
            ];
        }
        if let Some(result) = request.tool_results().next() {
            assert!(!result.is_error);
            vec![Ok(CompletionEvent::Finished)]
        } else {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "call",
                TASK_TOOL_NAME,
                json!({"prompt":"child","mode":"sync"}),
            )))]
        }
    }));
    let root = tempfile::tempdir().unwrap();
    let sandbox = TestSandbox::new(root.path()).await;
    let environment = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().to_path_buf(),
        FileCredentialStore::new(root.path().join("auth.json")),
        crate::host_tests::agent_definitions::task_agent(1),
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), provider.clone())));
    let directory = root.path().join("sessions");
    std::fs::create_dir_all(&directory).unwrap();
    let mut legacy = waltr_agent::SessionRecord::new("parent");
    legacy.metadata.incarnation = None;
    legacy.inference_settings = Some(test_inference_settings());
    std::fs::write(
        directory.join("parent.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let sink = Arc::new(SemanticSink::default());
    let sessions = SessionCoordinator::new(waltr_agent::FilesystemSessionStore::new(directory));
    let environment = environment
        .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new()));
    let sessions = sessions
        .clone()
        .with_runtime_factory(environment.session_factory(sessions))
        .with_session_event_sink(sink.clone());
    let result = sessions
        .submit_prompt(
            SessionId::new("parent"),
            "spawn",
            RunConflictPolicy::RejectIfActive,
        )
        .await;
    assert!(
        result.is_err(),
        "legacy record without an incarnation must not acquire new authority"
    );
    assert!(
        sessions
            .load(&SessionId::new("task_parent_0_call"))
            .await
            .is_err()
    );
    assert!(sink.0.lock().unwrap().is_empty());
    sessions.shutdown().await;
}

#[tokio::test]
async fn failed_task_child_never_emits_completed_fact() {
    let provider = Arc::new(ScriptedProvider::new("failed-task", |request| {
        if request.prompt == "child" {
            return vec![Err(waltr_agent::LlmError::Protocol {
                provider: "test",
                message: "child failed".into(),
            })];
        }
        if request.tool_results().next().is_none() {
            vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                "call",
                TASK_TOOL_NAME,
                json!({"prompt":"child"}),
            )))]
        } else {
            vec![Ok(CompletionEvent::Finished)]
        }
    }));
    let sink = Arc::new(SemanticSink::default());
    let (sessions, _sandbox, _root) = task_sessions_with_sink(provider, 1, Some(sink.clone())).await;
    let mut request = waltr_agent::SessionCreateRequest::new("parent");
    request.inference_settings = Some(test_inference_settings());
    sessions.create(request).await.unwrap();
    sessions
        .submit_prompt(
            SessionId::new("parent"),
            "spawn",
            RunConflictPolicy::RejectIfActive,
        )
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    let child = sessions
        .load(&SessionId::new("task_parent_0_call"))
        .await
        .unwrap()
        .record;
    assert!(matches!(
        child.snapshot().status,
        waltr_agent::SessionStatus::Failed { .. }
    ));
    assert!(sink.0.lock().unwrap().is_empty());
    sessions.shutdown().await;
}

#[tokio::test]
async fn cancelled_actual_task_children_never_emit_completed_facts() {
    let provider = Arc::new(AsyncTaskProvider::new());
    let sink = Arc::new(SemanticSink::default());
    let (sessions, _sandbox, _root) = task_sessions_with_sink(provider.clone(), 1, Some(sink.clone())).await;
    let mut request = waltr_agent::SessionCreateRequest::new("parent");
    request.inference_settings = Some(test_inference_settings());
    sessions.create(request).await.unwrap();
    sessions
        .submit_prompt(
            SessionId::new("parent"),
            "start tasks",
            RunConflictPolicy::RejectIfActive,
        )
        .await
        .unwrap()
        .handle
        .wait()
        .await
        .unwrap();
    timeout(
        Duration::from_secs(2),
        provider.wait_for_child_prompt("first child"),
    )
    .await
    .unwrap();
    timeout(
        Duration::from_secs(2),
        provider.wait_for_child_prompt("second child"),
    )
    .await
    .unwrap();
    for id in ["task_parent_0_task-1", "task_parent_0_task-2"] {
        let child = sessions
            .interrupt(&SessionId::new(id))
            .await
            .unwrap()
            .record;
        assert!(
            !child
                .journal
                .iter()
                .any(|event| matches!(event, SessionJournalEvent::Finished))
        );
    }
    sessions.shutdown().await;
    assert!(sink.0.lock().unwrap().is_empty());
}
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn task_tool_sync_mode_returns_child_assistant_text() -> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "task_sync_provider",
        |request| {
            let tool_results = request.tool_results().collect::<Vec<_>>();
            match (request.prompt, tool_results.as_slice()) {
                ("spawn child", []) => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "task-call",
                    TASK_TOOL_NAME,
                    json!({
                        "mode": "sync",
                        "description": "child work",
                        "prompt": "child prompt"
                    }),
                )))],
                ("child prompt", []) => vec![
                    Ok(CompletionEvent::Delta("child-result".to_owned())),
                    Ok(CompletionEvent::Finished),
                ],
                ("spawn child", [result]) => {
                    let output: Value = serde_json::from_str(&result.content).expect("task output");
                    assert_eq!(output["status"], "finished");
                    assert_eq!(output["result"], "child-result");
                    assert_eq!(output["task_id"], "task_ephemeral_0_task-call");
                    vec![
                        Ok(CompletionEvent::Delta("parent-finished".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                (prompt, results) => panic!("unexpected prompt/results: {prompt:?} {results:?}"),
            }
        },
    )));
    let (runs, _sandbox, _root) = task_sessions(provider.clone(), 1).await;
    let parent_id = SessionId::new("ephemeral");
    let agent_definition = task_agent_definition();
    let mut request = waltr_agent::SessionCreateRequest::new(parent_id.clone());
    request.inference_settings = Some(test_inference_settings());
    request.agent_definition = Some(agent_definition.clone());
    runs.create(request).await?;

    let run = runs
        .submit_prompt(parent_id, "spawn child", RunConflictPolicy::ReplaceActive)
        .await?;
    let outcome = timeout(Duration::from_secs(2), run.handle.wait())
        .await
        .expect("root run should finish")?;
    assert_eq!(outcome.assistant_text.as_deref(), Some("parent-finished"));
    let task_tool = provider
        .tool_named(TASK_TOOL_NAME)
        .unwrap_or_else(|| panic!("task tool should be exposed\n{}", provider.tool_context()));
    assert!(task_tool.input_schema["properties"].get("mode").is_some());
    assert!(
        task_tool.input_schema["properties"]
            .get("description")
            .is_some()
    );
    assert!(task_tool.input_schema["properties"].get("prompt").is_some());
    assert!(task_tool.input_schema["properties"].get("action").is_none());
    assert!(
        task_tool.input_schema["properties"]
            .get("task_id")
            .is_none()
    );
    let child = runs
        .load(&SessionId::new("task_ephemeral_0_task-call"))
        .await?;
    assert_eq!(child.record.metadata.values["task.depth"], json!(1));
    assert_eq!(
        child.record.inference_settings,
        Some(test_inference_settings())
    );
    assert_eq!(child.record.agent_definition, Some(agent_definition));
    let progress = task_progress_events(&runs, &SessionId::new("ephemeral")).await?;
    assert!(
        progress
            .iter()
            .any(|event| event["description"] == "child work"
                && event["mode"] == "sync"
                && event["status"] == "running"),
        "progress events: {progress:#?}"
    );
    assert!(
        progress
            .iter()
            .any(|event| event["description"] == "child work"
                && event["mode"] == "sync"
                && event["status"] == "finished"),
        "progress events: {progress:#?}"
    );

    Ok(())
}

#[tokio::test]
async fn task_tool_default_depth_blocks_subagents_from_spawning_tasks() -> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "task_depth_default_provider",
        |request| {
            let tool_results = request.tool_results().collect::<Vec<_>>();
            match (request.prompt, tool_results.as_slice()) {
                ("root spawn", []) => {
                    assert!(request_has_task(request));
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "root-task",
                        TASK_TOOL_NAME,
                        json!({ "mode": "sync", "prompt": "child spawn" }),
                    )))]
                }
                ("child spawn", []) => {
                    assert!(!request_has_task(request));
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "nested-task",
                        TASK_TOOL_NAME,
                        json!({ "mode": "sync", "prompt": "grandchild prompt" }),
                    )))]
                }
                ("child spawn", [result]) => {
                    assert!(
                        result.content.contains("subagent depth limit exceeded"),
                        "{}",
                        result.content
                    );
                    vec![
                        Ok(CompletionEvent::Delta("nested-blocked".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                ("root spawn", [result]) => {
                    let output: Value = serde_json::from_str(&result.content).expect("task output");
                    assert_eq!(output["result"], "nested-blocked");
                    vec![
                        Ok(CompletionEvent::Delta("root-finished".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                ("grandchild prompt", _) => panic!("grandchild should not run by default"),
                (prompt, results) => panic!("unexpected prompt/results: {prompt:?} {results:?}"),
            }
        },
    )));
    let (runs, _sandbox, _root) = task_sessions(provider.clone(), 1).await;
    let parent_id = SessionId::new("ephemeral");
    let mut request = waltr_agent::SessionCreateRequest::new(parent_id.clone());
    request.inference_settings = Some(test_inference_settings());
    runs.create(request).await?;

    let run = runs
        .submit_prompt(parent_id, "root spawn", RunConflictPolicy::ReplaceActive)
        .await?;
    let outcome = timeout(Duration::from_secs(2), run.handle.wait())
        .await
        .expect("root run should finish")?;
    assert_eq!(outcome.assistant_text.as_deref(), Some("root-finished"));
    let child = runs
        .load(&SessionId::new("task_ephemeral_0_root-task"))
        .await?;
    assert_eq!(child.record.metadata.values["task.depth"], json!(1));
    assert_eq!(child.record.agent_definition, None);
    assert!(
        runs.load(&SessionId::new(
            "task_task_ephemeral_0_root-task_0_nested-task"
        ))
        .await
        .is_err()
    );

    Ok(())
}

#[tokio::test]
async fn task_tool_max_depth_allows_configured_nested_subagent() -> waltr_agent::Result<()> {
    let provider = Arc::new(CapturingProvider::new(ScriptedProvider::new(
        "task_depth_configured_provider",
        |request| {
            let tool_results = request.tool_results().collect::<Vec<_>>();
            match (request.prompt, tool_results.as_slice()) {
                ("root spawn nested", []) => {
                    assert!(request_has_task(request));
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "root-task",
                        TASK_TOOL_NAME,
                        json!({ "mode": "sync", "prompt": "child spawn nested" }),
                    )))]
                }
                ("child spawn nested", []) => {
                    assert!(request_has_task(request));
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "nested-task",
                        TASK_TOOL_NAME,
                        json!({ "mode": "sync", "prompt": "grandchild prompt" }),
                    )))]
                }
                ("grandchild prompt", []) => {
                    assert!(!request_has_task(request));
                    vec![
                        Ok(CompletionEvent::Delta("grandchild-result".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                ("child spawn nested", [result]) => {
                    let output: Value = serde_json::from_str(&result.content).expect("task output");
                    assert_eq!(output["result"], "grandchild-result");
                    vec![
                        Ok(CompletionEvent::Delta("child-saw-grandchild".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                ("root spawn nested", [result]) => {
                    let output: Value = serde_json::from_str(&result.content).expect("task output");
                    assert_eq!(output["result"], "child-saw-grandchild");
                    vec![
                        Ok(CompletionEvent::Delta("root-finished-nested".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                (prompt, results) => panic!("unexpected prompt/results: {prompt:?} {results:?}"),
            }
        },
    )));
    let (runs, _sandbox, _root) = task_sessions(provider.clone(), 2).await;
    let parent_id = SessionId::new("ephemeral");
    let mut request = waltr_agent::SessionCreateRequest::new(parent_id.clone());
    request.inference_settings = Some(test_inference_settings());
    runs.create(request).await?;

    let run = runs
        .submit_prompt(
            parent_id,
            "root spawn nested",
            RunConflictPolicy::ReplaceActive,
        )
        .await?;
    let outcome = timeout(Duration::from_secs(2), run.handle.wait())
        .await
        .expect("root run should finish")?;
    assert_eq!(
        outcome.assistant_text.as_deref(),
        Some("root-finished-nested")
    );
    let nested = runs
        .load(&SessionId::new(
            "task_task_ephemeral_0_root-task_0_nested-task",
        ))
        .await?;
    assert_eq!(nested.record.metadata.values["task.depth"], json!(2));

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn task_tool_async_mode_returns_generated_task_ids() -> waltr_agent::Result<()> {
    let provider = Arc::new(AsyncTaskProvider::new());
    let sink = Arc::new(SemanticSink::default());
    let (runs, _sandbox, _root) = task_sessions_with_sink(provider.clone(), 1, Some(sink.clone())).await;

    let parent_id = SessionId::new("parent");
    let parent_definition = task_agent_definition();
    let mut request = waltr_agent::SessionCreateRequest::new(parent_id.clone());
    request.inference_settings = Some(test_inference_settings());
    request.agent_definition = Some(parent_definition.clone());
    runs.create(request).await?;
    let existing_child_id = SessionId::new("task_parent_0_task-2");
    let existing_definition = basic_agent_definition("existing");
    let mut request = waltr_agent::SessionCreateRequest::new(existing_child_id.clone());
    request.inference_settings = Some(test_inference_settings());
    request.agent_definition = Some(existing_definition.clone());
    request.metadata_values =
        std::collections::BTreeMap::from([("existing".to_owned(), json!(true))]);
    let child = runs.create(request).await?.record;
    let parent = runs.load(&parent_id).await?.record;
    sink.1.lock().unwrap().as_ref().unwrap().register(&child, SessionRole::Task(TaskOrigin {
        parent: parent_id.clone(), incarnation: parent.metadata.incarnation.unwrap(), turn: 0, call: "task-2".into(), depth: 1,
    })).unwrap();
    let parent_run = runs
        .submit_prompt(
            parent_id.clone(),
            "start tasks",
            RunConflictPolicy::ReplaceActive,
        )
        .await?;
    let parent_outcome = timeout(Duration::from_secs(2), parent_run.handle.wait())
        .await
        .expect("parent run should finish")?;
    assert_eq!(
        parent_outcome.assistant_text.as_deref(),
        Some("parent-started")
    );

    let first_task_id = SessionId::new("task_parent_0_task-1");
    let second_task_id = existing_child_id;
    let first_child = runs.load(&first_task_id).await?;
    assert_eq!(first_child.record.agent_definition, Some(parent_definition));
    let second_child = runs.load(&second_task_id).await?;
    assert_eq!(
        second_child.record.agent_definition,
        Some(existing_definition)
    );
    assert_eq!(second_child.record.metadata.values["existing"], json!(true));
    assert!(
        runs.active_state(&first_task_id).await.is_some(),
        "first child task should still be running"
    );
    assert!(
        runs.active_state(&second_task_id).await.is_some(),
        "second child task should still be running"
    );
    let progress = task_progress_events(&runs, &parent_id).await?;
    assert_eq!(progress.len(), 2, "progress events: {progress:#?}");
    assert!(
        progress
            .iter()
            .any(|event| event["taskId"] == "task_parent_0_task-1"
                && event["mode"] == "async"
                && event["status"] == "running"),
        "progress events: {progress:#?}"
    );
    assert!(
        progress
            .iter()
            .any(|event| event["taskId"] == "task_parent_0_task-2"
                && event["mode"] == "async"
                && event["status"] == "running"),
        "progress events: {progress:#?}"
    );

    timeout(
        Duration::from_secs(2),
        provider.wait_for_child_prompt("first child"),
    )
    .await
    .expect("first child prompt should start");
    timeout(
        Duration::from_secs(2),
        provider.wait_for_child_prompt("second child"),
    )
    .await
    .expect("second child prompt should start");
    provider.release_one();
    provider.release_one();
    timeout(
        Duration::from_secs(2),
        wait_until_inactive(&runs, first_task_id),
    )
    .await
    .expect("first child run should finish");
    timeout(
        Duration::from_secs(2),
        wait_until_inactive(&runs, second_task_id),
    )
    .await
    .expect("second child run should finish");
    timeout(Duration::from_secs(2), provider.wait_for_callback_count(2))
        .await
        .expect("async task completions should prompt the parent session");
    timeout(
        Duration::from_secs(2),
        wait_until_inactive(&runs, parent_id.clone()),
    )
    .await
    .expect("parent callback runs should finish");

    let parent = runs.load(&parent_id).await?;
    let prompts = parent
        .record
        .journal
        .iter()
        .filter_map(|event| match event {
            SessionJournalEvent::PromptAppended { prompt } => Some(prompt.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.first().copied(), Some("start tasks"));
    assert!(
        prompts.iter().any(|prompt| prompt.contains(
            "Task ID: task_parent_0_task-1\nStatus: finished\n\nResult:\nfirst child done"
        )),
        "parent prompts: {prompts:#?}"
    );
    assert!(
        prompts.iter().any(|prompt| prompt.contains(
            "Task ID: task_parent_0_task-2\nStatus: finished\n\nResult:\nsecond child done"
        )),
        "parent prompts: {prompts:#?}"
    );

    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn async_task_callbacks_coalesce_while_parent_is_active() -> waltr_agent::Result<()> {
    let provider = Arc::new(AsyncTaskProvider::new());
    let (runs, _sandbox, _root) = task_sessions(provider.clone(), 1).await;

    let parent_id = SessionId::new("parent");
    let mut request = waltr_agent::SessionCreateRequest::new(parent_id.clone());
    request.inference_settings = Some(test_inference_settings());
    runs.create(request).await?;
    let parent_run = runs
        .submit_prompt(
            parent_id.clone(),
            "start tasks",
            RunConflictPolicy::ReplaceActive,
        )
        .await?;
    timeout(Duration::from_secs(2), parent_run.handle.wait())
        .await
        .expect("parent run should finish")?;

    timeout(
        Duration::from_secs(2),
        provider.wait_for_child_prompt("first child"),
    )
    .await
    .expect("first child prompt should start");
    timeout(
        Duration::from_secs(2),
        provider.wait_for_child_prompt("second child"),
    )
    .await
    .expect("second child prompt should start");

    let parent_hold = runs
        .submit_prompt(
            parent_id.clone(),
            "hold parent",
            RunConflictPolicy::ReplaceActive,
        )
        .await?;
    provider.release_one();
    provider.release_one();

    timeout(Duration::from_secs(2), async {
        loop {
            if runs
                .active_state(&parent_id)
                .await
                .is_some_and(|state| state.queued_count >= 2)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("task callbacks should queue behind active parent");

    provider.release_parent();
    timeout(Duration::from_secs(2), parent_hold.handle.wait())
        .await
        .expect("parent hold should finish")?;
    timeout(
        Duration::from_secs(2),
        provider.wait_for_callback_batch_count(1),
    )
    .await
    .expect("callback batch should run");
    timeout(
        Duration::from_secs(2),
        wait_until_inactive(&runs, parent_id.clone()),
    )
    .await
    .expect("parent callback batch should finish");

    let batches = provider.callback_batches.lock().await.clone();
    assert_eq!(batches.len(), 1, "callback batches: {batches:#?}");
    assert_eq!(batches[0].len(), 2, "callback batches: {batches:#?}");
    assert!(
        batches[0][0].contains("Task ID: task_parent_0_task-1"),
        "callback batches: {batches:#?}"
    );
    assert!(
        batches[0][1].contains("Task ID: task_parent_0_task-2"),
        "callback batches: {batches:#?}"
    );

    Ok(())
}

fn test_model() -> ModelRef {
    ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
}

fn request_has_task(request: &CompletionRequest<'_>) -> bool {
    request.tools.iter().any(|tool| tool.name == TASK_TOOL_NAME)
}

async fn task_progress_events(
    runs: &SessionCoordinator<MemorySessionStore>,
    session_id: &SessionId,
) -> waltr_agent::Result<Vec<Value>> {
    let events = runs
        .timeline(
            session_id,
            TimelineQuery::forward(None, 128)
                .with_channels([TimelineChannel::from(CHANNEL_TOOL_PROGRESS_INFO)]),
        )
        .await?;
    Ok(events
        .into_iter()
        .filter_map(|event| match event.content {
            TimelineContent::Json { value }
                if value.get("kind").and_then(Value::as_str) == Some("task_progress") =>
            {
                Some(value)
            }
            _ => None,
        })
        .collect())
}

async fn task_sessions<P>(
    provider: Arc<P>,
    max_subagent_depth: usize,
) -> (
    SessionCoordinator<MemorySessionStore>,
    TestSandbox,
    tempfile::TempDir,
)
where
    P: LlmStream + 'static,
{
    task_sessions_with_sink(provider, max_subagent_depth, None).await
}

async fn task_sessions_with_sink<P: LlmStream + 'static>(
    provider: Arc<P>,
    max_subagent_depth: usize,
    sink: Option<Arc<SemanticSink>>,
) -> (
    SessionCoordinator<MemorySessionStore>,
    TestSandbox,
    tempfile::TempDir,
) {
    let root = tempfile::tempdir().expect("task workspace");
    let sandbox = TestSandbox::new(root.path()).await;
    let environment = AgentSessionRuntimeFactory::new(
        sandbox.handle(),
        root.path().to_path_buf(),
        FileCredentialStore::new(root.path().join("auth.json")),
        crate::host_tests::agent_definitions::task_agent(max_subagent_depth),
    )
    .with_runtime_provider(move |_| Ok(AgentRuntimeProvider::new(test_model(), provider.clone())));

    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let sessions = match sink {
        Some(sink) => {
            *sink.1.lock().unwrap() = Some(environment.session_roles.clone());
            sessions.with_session_event_sink(sink)
        },
        None => sessions,
    };
    let environment = environment
        .with_capability_policy(ServerPermissionPolicy::sessions(InteractionBroker::new()));
    (
        sessions
            .clone()
            .with_runtime_factory(environment.session_factory(sessions)),
        sandbox,
        root,
    )
}

fn test_inference_settings() -> InferenceSettings {
    InferenceSettings {
        provider: InferenceProviderSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: std::collections::BTreeMap::default(),
            oauth: None,
            reasoning_effort: None,
        },
    }
}

fn task_agent_definition() -> Value {
    serde_json::to_value(AgentDefinitionBundle::new(
        "test",
        vec![
            r"apiVersion: waltr.dev/v1alpha1
kind: Agent
metadata: { name: test }
spec:
  features: [{ featureSetRef: { name: task } }]
---
apiVersion: waltr.dev/v1alpha1
kind: FeatureSet
metadata: { name: task }
spec:
  task: { maxSubagentDepth: 1, defaultPermissions: true }
"
            .to_owned(),
        ],
    ))
    .unwrap()
}

fn basic_agent_definition(name: &str) -> Value {
    serde_json::to_value(AgentDefinitionBundle::new(
        name,
        vec![format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: {name} }}\nspec: {{}}\n"
        )],
    )).unwrap()
}

async fn wait_until_inactive(runs: &SessionCoordinator<MemorySessionStore>, session_id: SessionId) {
    loop {
        if runs.active_state(&session_id).await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Debug)]
struct AsyncTaskProvider {
    release: Arc<Notify>,
    release_parent: Arc<Notify>,
    child_prompts: Arc<Mutex<Vec<String>>>,
    callback_prompts: Arc<Mutex<Vec<String>>>,
    callback_batches: Arc<Mutex<Vec<Vec<String>>>>,
}

impl AsyncTaskProvider {
    fn new() -> Self {
        Self {
            release: Arc::new(Notify::new()),
            release_parent: Arc::new(Notify::new()),
            child_prompts: Arc::new(Mutex::new(Vec::new())),
            callback_prompts: Arc::new(Mutex::new(Vec::new())),
            callback_batches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn release_one(&self) {
        self.release.notify_one();
    }

    fn release_parent(&self) {
        self.release_parent.notify_waiters();
    }

    async fn wait_for_child_prompt(&self, prompt: &str) {
        loop {
            if self
                .child_prompts
                .lock()
                .await
                .iter()
                .any(|candidate| candidate == prompt)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_callback_count(&self, count: usize) {
        loop {
            if self.callback_prompts.lock().await.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_callback_batch_count(&self, count: usize) {
        loop {
            if self.callback_batches.lock().await.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl LlmStream for AsyncTaskProvider {
    fn name(&self) -> &'static str {
        "task_queue_provider"
    }

    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            let tool_results = request.tool_results().collect::<Vec<_>>();
            match (request.prompt, tool_results.as_slice()) {
                ("start tasks", []) => Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "task-1",
                        TASK_TOOL_NAME,
                        json!({ "mode": "async", "prompt": "first child" }),
                    ))),
                    Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "task-2",
                        TASK_TOOL_NAME,
                        json!({ "mode": "async", "prompt": "second child" }),
                    ))),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream),
                ("start tasks", [first, second]) => {
                    let first: Value = serde_json::from_str(&first.content).expect("first output");
                    let second: Value =
                        serde_json::from_str(&second.content).expect("second output");
                    assert_eq!(first["status"], "running");
                    assert_eq!(first["task_id"], "task_parent_0_task-1");
                    assert_eq!(first["queued_count"], 0);
                    assert_eq!(second["status"], "running");
                    assert_eq!(second["task_id"], "task_parent_0_task-2");
                    assert_eq!(second["queued_count"], 0);
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::Delta("parent-started".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                }
                ("first child" | "second child", []) => {
                    let prompt = request.prompt.to_owned();
                    let release = self.release.clone();
                    self.child_prompts.lock().await.push(prompt.clone());
                    let stream = stream::once(async move {
                        release.notified().await;
                        Ok(CompletionEvent::Delta(format!("{prompt} done")))
                    })
                    .chain(stream::iter(vec![Ok(CompletionEvent::Finished)]));
                    Ok(Box::pin(stream) as BoxCompletionStream)
                }
                ("hold parent", _) => {
                    let release_parent = self.release_parent.clone();
                    let stream = stream::once(async move {
                        release_parent.notified().await;
                        Ok(CompletionEvent::Delta("parent-held".to_owned()))
                    })
                    .chain(stream::iter(vec![Ok(CompletionEvent::Finished)]));
                    Ok(Box::pin(stream) as BoxCompletionStream)
                }
                (prompt, _) if prompt.starts_with("Async task completed.") => {
                    let callback_prompts = request
                        .replay_items()
                        .into_iter()
                        .filter_map(|item| match item {
                            CompletionReplayItem::Prompt(prompt)
                                if prompt.starts_with("Async task completed.") =>
                            {
                                Some(prompt.to_owned())
                            }
                            CompletionReplayItem::Prompt(_) | CompletionReplayItem::Turn(_) => None,
                        })
                        .collect::<Vec<_>>();
                    self.callback_prompts
                        .lock()
                        .await
                        .extend(callback_prompts.clone());
                    self.callback_batches.lock().await.push(callback_prompts);
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::Delta("parent-saw-task".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                }
                (prompt, results) => panic!("unexpected prompt/results: {prompt:?} {results:?}"),
            }
        })
    }
}
