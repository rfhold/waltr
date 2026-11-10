//! Generic provider, fake-tool, completion, and durable-session coverage.
use crate::scripted_provider::ScriptedProvider;
use futures_util::StreamExt;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, fmt::Write, sync::Arc};
use waltr_agent::*;

#[tokio::test]
async fn execution_only_fake_tools_do_not_leak_into_later_turns() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let provider = Arc::new(ScriptedProvider::new("run-overrides", move |request| {
        let turn = observed.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            request.tools.iter().any(|tool| tool.name == "run_only"),
            turn == 0
        );
        assert!(request.system_prompt.unwrap().contains("explicit-first"));
        vec![Ok(CompletionEvent::Finished)]
    }));
    let runtime = AgentRuntime::builder(
        ModelRef::new(ProviderId::OpenAi, "test", ModelApi::OpenAiResponses),
        provider,
    )
    .with_system_prompt("configured")
    .build();
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let id = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await
        .unwrap()
        .id()
        .clone();
    let tool = AgentTool::new(
        ToolDefinition::new("run_only", "test", json!({"type":"object"})),
        |ctx| Ok(ctx.result("fake")),
    );
    sessions
        .prompt(&runtime, id.clone(), "first")
        .with_system_prompt("explicit-first")
        .with_tool(tool)
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<waltr_agent::Result<Vec<_>>>()
        .unwrap();
    sessions
        .prompt(&runtime, id, "second")
        .stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<waltr_agent::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    sessions.shutdown().await;
}

#[allow(dead_code)]
pub const REPORT_TOOL_ERRORS_ENV: &str = "WALTR_AGENT_REPORT_TOOL_ERRORS";
#[allow(dead_code)]
pub const TEST_HARNESS_SYSTEM_PROMPT: &str = "You are running inside an automated test harness. Follow the user instructions exactly. Use tools when the user explicitly asks you to use them. When asked to report exact values, include those values verbatim. Keep final answers concise and do not add extra commentary unless requested.";

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct AgentTestSession {
    pub text: String,
    pub timeline: Vec<AgentTimelineEvent>,
    pub finished: bool,
    pub saw_text_after_tool_result: bool,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub tool_errors: Vec<ToolResult>,
}

#[allow(dead_code)]
pub fn report_tool_errors(provider: &impl LlmStream, session: &AgentTestSession, label: &str) {
    session.report_tool_errors_if_requested(provider.name(), label);
}

#[allow(dead_code)]
pub fn session_context(provider: &impl LlmStream, session: &AgentTestSession) -> String {
    format!(
        "{}: {}\n{}",
        provider.name(),
        session.text,
        session.tool_error_report()
    )
}

#[allow(dead_code)]
impl AgentTestSession {
    pub async fn run(
        agent: &AgentRuntime,
        _provider: &impl LlmStream,
        prompt: &str,
    ) -> waltr_agent::Result<Self> {
        let mut stream = agent
            .prompt(prompt)
            .with_system_prompt_fragment(TEST_HARNESS_SYSTEM_PROMPT)
            .stream();
        let mut session = Self::default();
        let mut saw_tool_result = false;

        while let Some(event) = stream.next().await {
            let event = event?;
            match &event.content {
                TimelineContent::TextDelta { delta }
                    if channel_is(&event, CHANNEL_ASSISTANT_TEXT) =>
                {
                    if saw_tool_result && !delta.is_empty() {
                        session.saw_text_after_tool_result = true;
                    }
                    session.text.push_str(delta);
                }
                TimelineContent::ToolCall { call } if channel_is(&event, CHANNEL_TOOL_CALL) => {
                    session.tool_calls.push(call.clone());
                }
                TimelineContent::ToolResult { result }
                    if channel_is(&event, CHANNEL_TOOL_RESULT) =>
                {
                    session.tool_results.push(result.clone());
                    saw_tool_result = true;
                    if result.content.starts_with("Tool error:") {
                        session.tool_errors.push(result.clone());
                    }
                }
                TimelineContent::Empty
                    if event.kind == TimelineEventKind::SessionStatusChanged
                        && event.status == Some(TimelineStatus::Finished) =>
                {
                    session.finished = true;
                }
                TimelineContent::Empty
                | TimelineContent::TextDelta { .. }
                | TimelineContent::TextSnapshot { .. }
                | TimelineContent::ContentRef { .. }
                | TimelineContent::Json { .. }
                | TimelineContent::ToolCall { .. }
                | TimelineContent::ToolResult { .. } => {}
            }
            session.timeline.push(event);
        }

        Ok(session)
    }

    pub fn require_tool_execution(&self, provider: &'static str) -> waltr_agent::Result<()> {
        if self.tool_calls.is_empty() || self.tool_results.is_empty() {
            return Err(LlmError::Protocol {
                provider,
                message: "agent prompt did not execute a tool".to_owned(),
            });
        }

        Ok(())
    }

    pub fn tool_error_report(&self) -> String {
        if self.tool_errors.is_empty() {
            return "tool errors: none".to_owned();
        }

        let mut report = String::from("tool errors:");
        for error in &self.tool_errors {
            write!(
                report,
                "\n- {}({}): {}",
                error.name, error.call_id, error.content
            )
            .expect("write string report");
        }
        report
    }

    #[allow(dead_code)]
    pub fn report_tool_errors_if_requested(&self, provider: &str, label: &str) {
        if !report_tool_errors_enabled() || self.tool_errors.is_empty() {
            return;
        }

        eprintln!(
            "{provider} {label} {}\n{}",
            REPORT_TOOL_ERRORS_ENV,
            self.tool_error_report()
        );
    }
}

pub fn channel_is(event: &AgentTimelineEvent, channel: &str) -> bool {
    event
        .channel
        .as_ref()
        .is_some_and(|candidate| candidate.as_str() == channel)
}

#[allow(dead_code)]
fn report_tool_errors_enabled() -> bool {
    env::var(REPORT_TOOL_ERRORS_ENV)
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

#[cfg(test)]
mod completion {
    use std::{collections::BTreeMap, sync::Arc};

    use futures_util::StreamExt;
    use serde_json::json;
    use tokio::{
        sync::Notify,
        time::{Duration, timeout},
    };
    use waltr_agent::{
        AgentRuntime, AgentTimelineEvent, AgentTool, BoxCompletionStream, BoxStreamFuture,
        CompletionEvent, CompletionRequest, LlmStream, MemorySessionStore, ModelApi, ModelRef,
        PendingTimelineEvent, PendingTimelineItemId, ProviderId, SessionCoordinator,
        SessionCreateRequest, SessionId, SessionListEvent, SessionStatus, SessionStore,
        SessionTimelineEvent, TimelineContent, TimelineCursor, TimelineEventKind, TimelineQuery,
        TimelineStatus, ToolCall, ToolDefinition,
    };

    use crate::scripted_provider::{CapturingProvider, ScriptedProvider};

    fn scripted_provider(
        script: Vec<Vec<CompletionEvent>>,
    ) -> Arc<CapturingProvider<ScriptedProvider>> {
        Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "scripted_provider",
            script,
        )))
    }

    fn test_model() -> ModelRef {
        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
    }

    fn echo_tool() -> AgentTool {
        AgentTool::new(
            ToolDefinition::new(
                "echo",
                "Echoes a marker.",
                json!({ "type": "object", "additionalProperties": false }),
            ),
            |call| Ok(call.result("echo output")),
        )
    }

    fn tool_then_text_script() -> Vec<Vec<CompletionEvent>> {
        vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "call-1",
                "echo",
                json!({}),
            ))],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]
    }

    #[derive(Default)]
    struct GatedProvider {
        entered: Notify,
        release: Notify,
    }

    impl LlmStream for GatedProvider {
        fn name(&self) -> &'static str {
            "gated_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(Box::pin(futures_util::stream::iter(vec![
                    Ok(CompletionEvent::Delta("done".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn stale_run_does_not_publish_after_delete_and_same_id_recreate() {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = SessionId::new("recreated-publication");
        let mut old = SessionCreateRequest::new(id.clone());
        old.metadata_values = BTreeMap::from([("owner".to_owned(), json!("old"))]);
        sessions.create(old).await.expect("create old incarnation");
        let mut watch = sessions.watch_sessions();
        let provider = Arc::new(GatedProvider::default());
        let agent = AgentRuntime::builder(test_model(), Arc::clone(&provider)).build();
        let run_sessions = sessions.clone();
        let run_id = id.clone();
        let run = tokio::spawn(async move {
            run_sessions
                .prompt(&agent, run_id, "run")
                .stream()
                .collect::<Vec<_>>()
                .await
        });
        provider.entered.notified().await;

        sessions.delete(&id).await.expect("delete old incarnation");
        let mut replacement = SessionCreateRequest::new(id.clone());
        replacement.metadata_values = BTreeMap::from([("owner".to_owned(), json!("new"))]);
        sessions
            .create(replacement)
            .await
            .expect("create replacement incarnation");
        provider.release.notify_one();
        drop(run.await.expect("run task"));

        let mut updates = Vec::new();
        while let Ok(Some(update)) = timeout(Duration::from_millis(25), watch.next()).await {
            updates.push(update);
        }
        let deleted = updates
            .iter()
            .position(|update| update.event == SessionListEvent::Deleted(id.clone()))
            .expect("delete publication");
        assert!(updates[deleted + 1..].iter().all(|update| {
            !matches!(
                &update.event,
                SessionListEvent::Upsert(summary)
                    if summary.id == id
                        && summary.metadata_values.get("owner") == Some(&json!("old"))
            )
        }));
    }

    async fn saw_finished_status<S>(mut stream: S) -> bool
    where
        S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
    {
        let mut finished = false;
        while let Some(event) = stream.next().await {
            let event = event.expect("agent event");
            if event.kind == TimelineEventKind::SessionStatusChanged
                && event.status == Some(TimelineStatus::Finished)
                && event.content == TimelineContent::Empty
            {
                finished = true;
            }
        }
        finished
    }

    #[tokio::test]
    async fn create_emits_session_list_update() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let mut watch = sessions.watch_sessions();

        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("session update")
            .expect("session update item");
        let listed = sessions.list().await.expect("listed sessions");

        assert_eq!(update.sequence, 1);
        assert_eq!(
            update.event,
            SessionListEvent::Upsert(created.record.summary())
        );
        assert!(listed.iter().any(|session| &session.id == created.id()));
    }

    #[tokio::test]
    async fn create_updates_have_monotonic_sequences() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let mut watch = sessions.watch_sessions();

        let first = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created first session");
        let second = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created second session");
        let first_update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("first update")
            .expect("first update item");
        let second_update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("second update")
            .expect("second update item");
        let listed = sessions.list().await.expect("listed sessions");

        assert_eq!(first_update.sequence, 1);
        assert_eq!(second_update.sequence, 2);
        assert_eq!(
            first_update.event,
            SessionListEvent::Upsert(first.record.summary())
        );
        assert_eq!(
            second_update.event,
            SessionListEvent::Upsert(second.record.summary())
        );
        assert!(listed.iter().any(|session| &session.id == first.id()));
        assert!(listed.iter().any(|session| &session.id == second.id()));
    }

    #[tokio::test]
    async fn rollback_to_cursor_preserves_selected_timeline_event() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let session = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let session_id = session.id().clone();
        let seeded = sessions
            .append_timeline(
                &session_id,
                vec![
                    PendingTimelineEvent::user_text_snapshot(
                        session_id.clone(),
                        1,
                        PendingTimelineItemId::New,
                        "first prompt",
                    ),
                    PendingTimelineEvent::user_text_snapshot(
                        session_id.clone(),
                        2,
                        PendingTimelineItemId::New,
                        "second prompt",
                    ),
                ],
            )
            .await
            .expect("seed timeline");

        sessions
            .rollback_to_cursor(session_id.clone(), seeded[1].cursor)
            .await
            .expect("rollback to cursor");
        let events = sessions
            .timeline(&session_id, TimelineQuery::forward(None, 10))
            .await
            .expect("timeline after rollback");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].item_id.as_str(), "i1");
        assert_eq!(events[1].item_id.as_str(), "i2");
    }

    #[tokio::test]
    async fn rollback_before_cursor_removes_selected_timeline_event_and_publishes_truncation() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let session = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let session_id = session.id().clone();
        let seeded = sessions
            .append_timeline(
                &session_id,
                vec![
                    PendingTimelineEvent::user_text_snapshot(
                        session_id.clone(),
                        1,
                        PendingTimelineItemId::New,
                        "first prompt",
                    ),
                    PendingTimelineEvent::user_text_snapshot(
                        session_id.clone(),
                        2,
                        PendingTimelineItemId::New,
                        "second prompt",
                    ),
                ],
            )
            .await
            .expect("seed timeline");
        let mut watch = sessions.watch_timeline(session_id.clone());

        sessions
            .rollback_before_cursor(session_id.clone(), seeded[1].cursor)
            .await
            .expect("rollback before cursor");
        let update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("timeline truncation update")
            .expect("timeline truncation item");
        let events = sessions
            .timeline(&session_id, TimelineQuery::forward(None, 10))
            .await
            .expect("timeline after rollback");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].item_id.as_str(), "i1");
        assert!(matches!(
            update.event,
            SessionTimelineEvent::Truncated {
                after_cursor: Some(cursor),
                ..
            } if cursor == seeded[0].cursor
        ));
        let continued = sessions
            .append_timeline(
                &session_id,
                vec![PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    2,
                    PendingTimelineItemId::New,
                    "replacement prompt",
                )],
            )
            .await
            .expect("append after rollback");
        assert_eq!(continued[0].cursor, TimelineCursor::new(3));
        assert_eq!(continued[0].item_id.as_str(), "i3");
    }

    #[tokio::test]
    async fn rollback_before_first_cursor_clears_visible_timeline() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let session = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let session_id = session.id().clone();
        let seeded = sessions
            .append_timeline(
                &session_id,
                vec![PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    PendingTimelineItemId::New,
                    "first prompt",
                )],
            )
            .await
            .expect("seed timeline");

        sessions
            .rollback_before_cursor(session_id.clone(), seeded[0].cursor)
            .await
            .expect("rollback before first cursor");
        let events = sessions
            .timeline(&session_id, TimelineQuery::forward(None, 10))
            .await
            .expect("timeline after rollback");

        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn cloned_sessions_share_watch_stream() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let sessions_clone = sessions.clone();
        let mut watch = sessions_clone.watch_sessions();

        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("session update")
            .expect("session update item");
        let listed = sessions_clone.list().await.expect("listed sessions");

        assert_eq!(update.sequence, 1);
        assert_eq!(
            update.event,
            SessionListEvent::Upsert(created.record.summary())
        );
        assert!(listed.iter().any(|session| &session.id == created.id()));
    }

    #[tokio::test]
    async fn late_watchers_use_list_for_baseline() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let existing = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created existing session");
        let mut watch = sessions.watch_sessions();
        let baseline = sessions.list().await.expect("listed sessions");

        assert!(baseline.iter().any(|session| &session.id == existing.id()));
        assert!(
            timeout(Duration::from_millis(25), watch.next())
                .await
                .is_err(),
            "late watchers should not receive replayed session updates"
        );

        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created new session");
        let update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("session update")
            .expect("session update item");

        assert_eq!(update.sequence, 2);
        assert_eq!(
            update.event,
            SessionListEvent::Upsert(created.record.summary())
        );
    }

    #[tokio::test]
    async fn delete_removes_session_from_list_and_is_idempotent() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");

        sessions
            .delete(created.id())
            .await
            .expect("deleted session");
        sessions
            .delete(created.id())
            .await
            .expect("deleted session again");
        sessions
            .delete(&SessionId::new("never-created"))
            .await
            .expect("deleted missing session");

        let listed = sessions.list().await.expect("listed sessions");
        assert!(!listed.iter().any(|session| &session.id == created.id()));
    }

    #[tokio::test]
    async fn delete_emits_deleted_watch_event_with_monotonic_sequence() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let mut watch = sessions.watch_sessions();

        sessions
            .delete(created.id())
            .await
            .expect("deleted session");
        let update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("session update")
            .expect("session update item");

        assert_eq!(update.sequence, 2);
        assert_eq!(
            update.event,
            SessionListEvent::Deleted(created.id().clone())
        );
    }

    #[tokio::test]
    async fn delete_missing_session_emits_deleted_watch_event() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let mut watch = sessions.watch_sessions();
        let missing = SessionId::new("never-created");

        sessions
            .delete(&missing)
            .await
            .expect("deleted missing session");
        let update = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("session update")
            .expect("session update item");

        assert_eq!(update.sequence, 1);
        assert_eq!(update.event, SessionListEvent::Deleted(missing));
    }

    #[tokio::test]
    async fn create_then_delete_emits_upsert_then_deleted() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let mut watch = sessions.watch_sessions();

        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        sessions
            .delete(created.id())
            .await
            .expect("deleted session");
        let upsert = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("upsert update")
            .expect("upsert update item");
        let deleted = timeout(Duration::from_secs(1), watch.next())
            .await
            .expect("delete update")
            .expect("delete update item");

        assert_eq!(upsert.sequence, 1);
        assert_eq!(
            upsert.event,
            SessionListEvent::Upsert(created.record.summary())
        );
        assert_eq!(deleted.sequence, 2);
        assert_eq!(
            deleted.event,
            SessionListEvent::Deleted(created.id().clone())
        );
    }

    #[tokio::test]
    async fn dropped_watchers_do_not_break_create() {
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store);
        let watch = sessions.watch_sessions();
        drop(watch);

        let created = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session");
        let listed = sessions.list().await.expect("listed sessions");

        assert!(listed.iter().any(|session| &session.id == created.id()));
    }

    #[tokio::test]
    async fn finished_snapshot_matches_stored_snapshot() {
        let store = MemorySessionStore::new();
        let provider = scripted_provider(tool_then_text_script());
        let agent = AgentRuntime::builder(test_model(), provider)
            .with_tool(echo_tool())
            .build();
        let sessions = SessionCoordinator::new(store.clone());
        let session_id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .expect("created session")
            .id()
            .clone();

        let finished = saw_finished_status(
            sessions
                .prompt(&agent, session_id.clone(), "run the tool")
                .stream(),
        )
        .await;

        let record = store.load(&session_id).await.expect("stored record");
        assert!(
            finished,
            "stream should emit finished status timeline event"
        );
        let snapshot = record.snapshot();
        assert_eq!(snapshot.revision, record.revision);
        assert_eq!(snapshot.status, SessionStatus::Finished);
    }

    #[tokio::test]
    async fn ephemeral_run_persists_complete_snapshot_to_supplied_store() {
        let store = MemorySessionStore::new();
        let session_id = SessionId::new("ephemeral-completion");
        let provider = scripted_provider(tool_then_text_script());
        let agent = AgentRuntime::builder(test_model(), provider)
            .with_system_prompt("ephemeral marker-prompt")
            .with_tool(echo_tool())
            .build();

        let finished = saw_finished_status(
            agent
                .prompt("run the tool")
                .with_session_store(store.clone(), session_id.clone())
                .stream(),
        )
        .await;
        let record = store.load(&session_id).await.expect("stored session");
        let assistant_text = record
            .timeline_events()
            .filter_map(|event| match &event.content {
                TimelineContent::TextDelta { delta }
                    if crate::session::channel_is(event, waltr_agent::CHANNEL_ASSISTANT_TEXT) =>
                {
                    Some(delta.as_str())
                }
                _ => None,
            })
            .collect::<String>();
        let snapshot = record.snapshot();

        assert!(
            finished,
            "stream should emit finished status timeline event"
        );
        assert_eq!(assistant_text, "done");
        assert_eq!(snapshot.prompts, ["run the tool"]);
        assert_eq!(snapshot.current_prompt.as_deref(), Some("run the tool"));
        assert_eq!(snapshot.status, SessionStatus::Finished);
        assert_eq!(snapshot.turn, 2);
        assert!(snapshot.tool_calls().any(|call| call.name == "echo"));
        assert!(
            snapshot
                .tool_results()
                .any(|result| result.content == "echo output")
        );
        let prompt_state = snapshot
            .middleware_state
            .get("system_prompt")
            .expect("captured system prompt state");
        assert!(prompt_state.to_string().contains("marker-prompt"));
    }
}

#[cfg(test)]
mod middleware_state {
    use serde_json::json;
    use waltr_agent::{
        MemorySessionStore, SessionCreateRequest, SessionId, SessionJournalEvent, SessionRecord,
        SessionStore,
    };

    #[tokio::test]
    async fn middleware_state_round_trips_through_memory_store() {
        let store = MemorySessionStore::new();
        let id = SessionId::new("middleware-state-session");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .expect("created session");

        store
            .append_journal(
                &id,
                store.load(&id).await.unwrap().execution_token().unwrap(),
                vec![
                    SessionJournalEvent::MiddlewareStateUpdated {
                        namespace: "agents".to_owned(),
                        state: json!({ "loaded": ["a"] }),
                    },
                    SessionJournalEvent::MiddlewareStateUpdated {
                        namespace: "agents".to_owned(),
                        state: json!({ "loaded": ["a", "b"] }),
                    },
                ],
            )
            .await
            .expect("appended events");

        let record = store.load(&id).await.expect("loaded record");
        let snapshot = record.snapshot();
        assert_eq!(
            snapshot.middleware_state.get("agents"),
            Some(&json!({ "loaded": ["a", "b"] })),
            "last write per namespace must win after rebuild"
        );

        let serialized = serde_json::to_value(&record).expect("serialized record");
        let restored: SessionRecord = serde_json::from_value(serialized).expect("restored record");
        assert_eq!(restored, record);
    }

    #[test]
    fn journal_only_record_serializes_without_snapshot() {
        let record = SessionRecord::new("legacy-session");
        let value = serde_json::to_value(&record).expect("serialized record");

        assert!(value.get("snapshot").is_none());
        let restored: SessionRecord = serde_json::from_value(value).expect("restored record");

        assert!(restored.snapshot().middleware_state.is_empty());
    }
}
