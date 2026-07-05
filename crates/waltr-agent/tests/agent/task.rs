//! `task` tool integration tests.

use std::sync::Arc;

use futures_util::{StreamExt as _, stream};
use waltr_agent::{
    AgentRuntime, AgentSessionRuntimeFactory, BoxCompletionStream, BoxStreamFuture,
    CHANNEL_TOOL_PROGRESS_INFO, CompletionEvent, CompletionReplayItem, CompletionRequest,
    FileCredentialStore, LlmStream, MemorySessionStore, ModelApi, ModelRef, ProviderId,
    RunConflictPolicy, SessionCoordinator, SessionId, SessionJournalEvent,
    SessionProviderRuntimeSettings, SessionRuntimeSettings, TimelineChannel, TimelineContent,
    TimelineQuery, ToolCall, tools::task::TASK_TOOL_NAME,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, timeout};

use crate::scripted_provider::{CapturingProvider, ScriptedProvider};
#[tokio::test]
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
    let runs = task_sessions(provider.clone(), 1);
    let parent_id = SessionId::new("ephemeral");
    runs.create_with_id_and_runtime_settings(
        parent_id.clone(),
        Some(test_runtime_settings()),
        Default::default(),
    )
    .await?;

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
    assert_eq!(child.record.metadata.values["waltr.task.depth"], json!(1));
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
    let runs = task_sessions(provider.clone(), 1);
    let parent_id = SessionId::new("ephemeral");
    runs.create_with_id_and_runtime_settings(
        parent_id.clone(),
        Some(test_runtime_settings()),
        Default::default(),
    )
    .await?;

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
    assert_eq!(child.record.metadata.values["waltr.task.depth"], json!(1));
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
    let runs = task_sessions(provider.clone(), 2);
    let parent_id = SessionId::new("ephemeral");
    runs.create_with_id_and_runtime_settings(
        parent_id.clone(),
        Some(test_runtime_settings()),
        Default::default(),
    )
    .await?;

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
    assert_eq!(nested.record.metadata.values["waltr.task.depth"], json!(2));

    Ok(())
}

#[tokio::test]
async fn task_tool_async_mode_returns_generated_task_ids() -> waltr_agent::Result<()> {
    let provider = Arc::new(AsyncTaskProvider::new());
    let runs = task_sessions(provider.clone(), 1);

    let parent_id = SessionId::new("parent");
    runs.create_with_id_and_runtime_settings(
        parent_id.clone(),
        Some(test_runtime_settings()),
        Default::default(),
    )
    .await?;
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
    let second_task_id = SessionId::new("task_parent_0_task-2");
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
async fn async_task_callbacks_coalesce_while_parent_is_active() -> waltr_agent::Result<()> {
    let provider = Arc::new(AsyncTaskProvider::new());
    let runs = task_sessions(provider.clone(), 1);

    let parent_id = SessionId::new("parent");
    runs.create_with_id_and_runtime_settings(
        parent_id.clone(),
        Some(test_runtime_settings()),
        Default::default(),
    )
    .await?;
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

fn task_sessions<P>(
    provider: Arc<P>,
    max_subagent_depth: usize,
) -> SessionCoordinator<MemorySessionStore>
where
    P: LlmStream + 'static,
{
    let environment = AgentSessionRuntimeFactory::new(
        std::env::temp_dir(),
        FileCredentialStore::new(std::env::temp_dir().join("waltr-agent-task-test-auth.json")),
    )
    .with_task_max_subagent_depth(max_subagent_depth)
    .with_runtime_builder(move |_| Ok(AgentRuntime::builder(test_model(), provider.clone())));

    SessionCoordinator::new(MemorySessionStore::new()).with_runtime_factory(environment)
}

fn test_runtime_settings() -> SessionRuntimeSettings {
    SessionRuntimeSettings {
        provider: SessionProviderRuntimeSettings {
            key: "test".to_owned(),
            api: ModelApi::OpenAiResponses,
            url: String::new(),
            model_id: "test-model".to_owned(),
            model_cost: None,
            headers: Default::default(),
            oauth: None,
            reasoning_effort: None,
        },
        mcp_servers: Vec::new(),
        mcp_trace_propagation: false,
    }
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
