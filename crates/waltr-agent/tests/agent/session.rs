//! Shared agent test session helpers.

use std::{env, fmt::Write};

use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentTimelineEvent, CHANNEL_ASSISTANT_TEXT, CHANNEL_TOOL_CALL,
    CHANNEL_TOOL_RESULT, ExternalRequest, LlmError, LlmStream, TimelineContent, TimelineEventKind,
    TimelineStatus, ToolCall, ToolResult,
};

#[allow(dead_code)]
pub const REPORT_TOOL_ERRORS_ENV: &str = "WALTR_AGENT_REPORT_TOOL_ERRORS";
pub const TEST_HARNESS_SYSTEM_PROMPT: &str = "You are running inside an automated test harness. Follow the user instructions exactly. Use tools when the user explicitly asks you to use them. When asked to report exact values, include those values verbatim. Keep final answers concise and do not add extra commentary unless requested.";

#[derive(Debug, Default)]
pub struct AgentTestSession {
    pub text: String,
    pub timeline: Vec<AgentTimelineEvent>,
    pub finished: bool,
    pub saw_text_after_tool_result: bool,
    pub external_requests: Vec<ExternalRequest>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub tool_errors: Vec<ToolResult>,
}

#[allow(dead_code)]
pub fn report_tool_errors(provider: &impl LlmStream, session: &AgentTestSession, label: &str) {
    session.report_tool_errors_if_requested(provider.name(), label);
}

pub fn session_context(provider: &impl LlmStream, session: &AgentTestSession) -> String {
    format!(
        "{}: {}\n{}",
        provider.name(),
        session.text,
        session.tool_error_report()
    )
}

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
                TimelineContent::ExternalRequest { request } => {
                    session.external_requests.push(request.clone());
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
    use std::sync::Arc;

    use futures_util::StreamExt;
    use waltr_agent::{
        AgentRuntime, AgentSessions, AgentTimelineEvent, AgentTool, CompletionEvent,
        MemorySessionStore, ModelApi, ModelRef, ProviderId, SessionId, SessionListEvent,
        SessionStatus, SessionStore, TimelineContent, TimelineEventKind, TimelineStatus, ToolCall,
        ToolDefinition,
    };
    use serde_json::json;
    use tokio::time::{Duration, timeout};

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
        let sessions = AgentSessions::new(store);
        let mut watch = sessions.watch_sessions();

        let created = sessions.create().await.expect("created session");
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
        let sessions = AgentSessions::new(store);
        let mut watch = sessions.watch_sessions();

        let first = sessions.create().await.expect("created first session");
        let second = sessions.create().await.expect("created second session");
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
    async fn cloned_sessions_share_watch_stream() {
        let store = MemorySessionStore::new();
        let sessions = AgentSessions::new(store);
        let sessions_clone = sessions.clone();
        let mut watch = sessions_clone.watch_sessions();

        let created = sessions.create().await.expect("created session");
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
        let sessions = AgentSessions::new(store);
        let existing = sessions.create().await.expect("created existing session");
        let mut watch = sessions.watch_sessions();
        let baseline = sessions.list().await.expect("listed sessions");

        assert!(baseline.iter().any(|session| &session.id == existing.id()));
        assert!(
            timeout(Duration::from_millis(25), watch.next())
                .await
                .is_err(),
            "late watchers should not receive replayed session updates"
        );

        let created = sessions.create().await.expect("created new session");
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
        let sessions = AgentSessions::new(store);
        let created = sessions.create().await.expect("created session");

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
        let sessions = AgentSessions::new(store);
        let created = sessions.create().await.expect("created session");
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
        let sessions = AgentSessions::new(store);
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
        let sessions = AgentSessions::new(store);
        let mut watch = sessions.watch_sessions();

        let created = sessions.create().await.expect("created session");
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
        let sessions = AgentSessions::new(store);
        let watch = sessions.watch_sessions();
        drop(watch);

        let created = sessions.create().await.expect("created session");
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
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
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
    use waltr_agent::{
        MemorySessionStore, Revision, SessionId, SessionJournalEvent, SessionRecord, SessionStore,
    };
    use serde_json::json;

    #[tokio::test]
    async fn middleware_state_round_trips_through_memory_store() {
        let store = MemorySessionStore::new();
        let id = SessionId::new("middleware-state-session");
        store.create(id.clone()).await.expect("created session");

        store
            .append_journal(
                &id,
                Revision::ZERO,
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

#[cfg(test)]
mod tool_projection {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::StreamExt;
    use waltr_agent::{
        AgentRuntime, AgentSessions, CompletionEvent, ExternalRequestId, ExternalResponse,
        MemorySessionStore, ModelApi, ModelRef, ProviderId, ResumeInput, TimelineContent, ToolCall,
        question::QuestionConfig, tools::filesystem::FilesystemConfig,
    };
    use serde_json::json;

    use crate::scripted_provider::{CapturingProvider, ScriptedProvider};

    fn scripted_provider(
        script: Vec<Vec<CompletionEvent>>,
    ) -> Arc<CapturingProvider<ScriptedProvider>> {
        Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "capture_tools_provider",
            script,
        )))
    }

    fn test_model() -> ModelRef {
        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
    }

    fn filesystem_agent(
        root: &Path,
        provider: Arc<CapturingProvider<ScriptedProvider>>,
    ) -> AgentRuntime {
        AgentRuntime::builder(test_model(), provider)
            .with_filesystem_tools(FilesystemConfig::new(root))
            .with_question_tool(QuestionConfig::new())
            .build()
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn root_hint_is_request_scoped_and_absent_from_stored_definitions() {
        let first_root = TempRoot::new("waltr-session-root-hint-a");
        let second_root = TempRoot::new("waltr-session-root-hint-b");
        let first_hint = format!(
            "Filesystem root: {}.",
            first_root.canonical_path().display()
        );
        let second_hint = format!(
            "Filesystem root: {}.",
            second_root.canonical_path().display()
        );

        let stored_tools = FilesystemConfig::new(first_root.path()).into_agent_tools(&test_model());
        for tool in &stored_tools {
            assert!(
                !tool.definition().description.contains("Filesystem root:"),
                "stored definition must stay pristine: {}",
                tool.definition().description
            );
        }

        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "q-1",
                "question",
                json!({
                    "questions": [{
                        "question": "Continue?",
                        "header": "Continue",
                        "options": [{ "label": "Yes", "description": "Keep going" }],
                        "multiple": false
                    }]
                }),
            ))],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);

        let agent = filesystem_agent(&first_root.path(), provider.clone());
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let mut request_id = None;
        let mut stream = sessions
            .prompt(&agent, session_id.clone(), "hello")
            .stream();
        while let Some(event) = stream.next().await {
            if let TimelineContent::ExternalRequest { request } =
                event.expect("agent event").content
            {
                request_id = Some(request.id.as_str().to_owned());
            }
        }
        drop(stream);

        let projected = provider.tool_description(0, "read");
        assert!(projected.contains(&first_hint), "{projected}");

        let resume_agent = filesystem_agent(&second_root.path(), provider.clone());
        let resume_sessions = AgentSessions::new(store);
        let mut stream = resume_sessions
            .resume(
                &resume_agent,
                session_id,
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new(request_id.expect("question request")),
                    response: ExternalResponse::new(json!("yes")),
                },
            )
            .stream();
        while let Some(event) = stream.next().await {
            drop(event.expect("agent event"));
        }
        drop(stream);

        let resumed = provider.tool_description(1, "read");
        assert!(resumed.contains(&second_hint), "{resumed}");
        assert!(
            !resumed.contains(&first_hint),
            "hint must be request-scoped: {resumed}"
        );
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        fn canonical_path(&self) -> PathBuf {
            self.path.canonicalize().expect("canonical temp root")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}

#[cfg(test)]
mod system_prompt_state {
    use std::sync::Arc;

    use futures_util::StreamExt;
    use waltr_agent::{
        AgentRuntime, AgentSessions, AgentTimelineEvent, AgentTool, CompletionEvent,
        ExternalRequestId, ExternalResponse, MemorySessionStore, ModelApi, ModelRef,
        PermissionEffect, PermissionPolicy, PermissionToolSelector, ProviderId, ResumeInput,
        SessionId, SessionJournalEvent, SessionRecord, SessionStore, TimelineContent, ToolCall,
        ToolDefinition,
        question::{QUESTION_TOOL_NAME, QuestionConfig},
    };
    use serde_json::json;

    use crate::scripted_provider::{CapturingProvider, ScriptedProvider};

    fn test_model() -> ModelRef {
        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
    }

    fn question_call(id: &str) -> CompletionEvent {
        CompletionEvent::ToolCall(ToolCall::new(
            id,
            QUESTION_TOOL_NAME,
            json!({
                "questions": [
                    {
                        "question": "Continue?",
                        "header": "Continue",
                        "options": [
                            { "label": "Yes", "description": "Keep going" }
                        ],
                        "multiple": false
                    }
                ]
            }),
        ))
    }

    fn question_answer(request_id: String) -> ResumeInput {
        ResumeInput::ExternalResponse {
            request_id: ExternalRequestId::new(request_id),
            response: ExternalResponse::new(json!("yes")),
        }
    }

    fn scripted_provider(
        script: Vec<Vec<CompletionEvent>>,
    ) -> Arc<CapturingProvider<ScriptedProvider>> {
        Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "scripted_provider",
            script,
        )))
    }

    struct RunOutput {
        tool_results: Vec<String>,
        external_request_id: Option<String>,
    }

    async fn drain_run<S>(mut stream: S) -> RunOutput
    where
        S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
    {
        let mut output = RunOutput {
            tool_results: Vec::new(),
            external_request_id: None,
        };
        while let Some(event) = stream.next().await {
            match event.expect("agent event").content {
                TimelineContent::ToolResult { result } => output.tool_results.push(result.content),
                TimelineContent::ExternalRequest { request } => {
                    output.external_request_id = Some(request.id.as_str().to_owned());
                }
                _ => {}
            }
        }
        output
    }

    fn system_prompt_state_indices(record: &SessionRecord) -> Vec<usize> {
        record
            .journal
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(
                    event,
                    SessionJournalEvent::MiddlewareStateUpdated { namespace, .. }
                        if namespace == "system_prompt"
                )
                .then_some(index)
            })
            .collect()
    }

    fn request_record_indices(record: &SessionRecord) -> Vec<usize> {
        record
            .journal
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event, SessionJournalEvent::ProviderRequestRecorded { .. })
                    .then_some(index)
            })
            .collect()
    }

    async fn create_session_id<S>(store: &S) -> SessionId
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        AgentSessions::new(store.clone())
            .create()
            .await
            .expect("created session")
            .id()
            .clone()
    }

    #[tokio::test]
    async fn rebuilt_agent_renders_stored_prompt_on_initialized_session() {
        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![
            vec![question_call("q-1")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_system_prompt("base prompt marker-first")
            .with_question_tool(QuestionConfig::new())
            .build();
        let session_id = create_session_id(&store).await;
        let run = drain_run(
            AgentSessions::new(store.clone())
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let request_id = run.external_request_id.expect("question request");

        let rebuilt = AgentRuntime::builder(test_model(), provider.clone())
            .with_system_prompt("base prompt marker-second")
            .with_question_tool(QuestionConfig::new())
            .build();
        drain_run(
            AgentSessions::new(store.clone())
                .resume(&rebuilt, session_id.clone(), question_answer(request_id))
                .stream(),
        )
        .await;

        let prompts = provider.system_prompts();
        assert_eq!(prompts.len(), 2);
        let resumed_prompt = prompts[1].as_deref().expect("resumed system prompt");
        assert!(resumed_prompt.contains("marker-first"), "{resumed_prompt}");
        assert!(
            !resumed_prompt.contains("marker-second"),
            "{resumed_prompt}"
        );

        let record = store.load(&session_id).await.expect("stored session");
        let persisted = record
            .provider_requests()
            .last()
            .expect("persisted request record");
        let persisted_prompt = persisted.canonical_request["system_prompt"]
            .as_str()
            .expect("persisted system prompt");
        assert!(
            persisted_prompt.contains("marker-first"),
            "{persisted_prompt}"
        );
        assert!(
            !persisted_prompt.contains("marker-second"),
            "{persisted_prompt}"
        );
    }

    #[tokio::test]
    async fn run_override_on_fresh_session_becomes_captured_first_turn_state() {
        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![vec![
            CompletionEvent::Delta("ok".to_owned()),
            CompletionEvent::Finished,
        ]]);
        let agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_system_prompt("configured marker-configured")
            .build();
        let session_id = create_session_id(&store).await;

        drain_run(
            AgentSessions::new(store.clone())
                .prompt(&agent, session_id.clone(), "hello")
                .with_system_prompt("override marker-override")
                .stream(),
        )
        .await;

        let prompt = provider.system_prompts()[0].clone().expect("system prompt");
        assert!(prompt.contains("marker-override"), "{prompt}");
        assert!(!prompt.contains("marker-configured"), "{prompt}");

        let record = store.load(&session_id).await.expect("stored session");
        let state_indices = system_prompt_state_indices(&record);
        assert_eq!(
            state_indices.len(),
            1,
            "override must fold into the single first-turn capture"
        );
        let request_indices = request_record_indices(&record);
        assert!(state_indices[0] < request_indices[0]);
        let snapshot = record.snapshot();
        assert_eq!(
            snapshot.middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "override marker-override" }))
        );
    }

    #[tokio::test]
    async fn run_override_on_initialized_session_journals_update_and_replays() {
        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![
            vec![question_call("q-1")],
            vec![question_call("q-2")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_system_prompt("configured marker-configured")
            .with_question_tool(QuestionConfig::new())
            .build();
        let session_id = create_session_id(&store).await;
        let sessions = AgentSessions::new(store.clone());

        let first = drain_run(
            sessions
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let first_request_id = first.external_request_id.expect("first question request");

        let second = drain_run(
            sessions
                .resume(
                    &agent,
                    session_id.clone(),
                    question_answer(first_request_id),
                )
                .with_system_prompt("override marker-override")
                .stream(),
        )
        .await;
        let second_request_id = second.external_request_id.expect("second question request");

        drain_run(
            sessions
                .resume(
                    &agent,
                    session_id.clone(),
                    question_answer(second_request_id),
                )
                .stream(),
        )
        .await;

        let prompts = provider.system_prompts();
        assert_eq!(prompts.len(), 3);
        let initial_prompt = prompts[0].as_deref().expect("initial prompt");
        assert!(initial_prompt.contains("marker-configured"));
        for prompt in &prompts[1..] {
            let prompt = prompt.as_deref().expect("overridden prompt");
            assert!(prompt.contains("marker-override"), "{prompt}");
            assert!(!prompt.contains("marker-configured"), "{prompt}");
        }

        let record = store.load(&session_id).await.expect("stored session");
        let state_indices = system_prompt_state_indices(&record);
        let request_indices = request_record_indices(&record);
        assert_eq!(
            state_indices.len(),
            2,
            "initialized-session override must journal one state update"
        );
        assert_eq!(request_indices.len(), 3);
        assert!(
            request_indices[0] < state_indices[1] && state_indices[1] < request_indices[1],
            "override update must be journaled before the request it influenced"
        );
        let snapshot = record.snapshot();
        assert_eq!(
            snapshot.middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "override marker-override" }))
        );
    }

    #[tokio::test]
    async fn execution_only_overrides_apply_without_state_events() {
        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![
            vec![question_call("q-1")],
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "blocked-1",
                "blocked_tool",
                json!({}),
            ))],
            vec![CompletionEvent::ToolCall(ToolCall::new(
                "extra-1",
                "extra_tool",
                json!({}),
            ))],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_question_tool(QuestionConfig::new())
            .build();
        let session_id = create_session_id(&store).await;
        let sessions = AgentSessions::new(store.clone());

        let first = drain_run(
            sessions
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let request_id = first.external_request_id.expect("question request");

        let schema = json!({ "type": "object", "additionalProperties": false });
        let extra_tool = AgentTool::new(
            ToolDefinition::new("extra_tool", "Extra run tool.", schema.clone()),
            |call| Ok(call.result("extra output")),
        );
        let blocked_tool = AgentTool::new(
            ToolDefinition::new("blocked_tool", "Blocked run tool.", schema),
            |call| Ok(call.result("blocked output")),
        );
        let policy = PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("blocked_tool"),
            PermissionEffect::deny(),
        );

        let resumed = drain_run(
            sessions
                .resume(&agent, session_id.clone(), question_answer(request_id))
                .with_tool(extra_tool)
                .with_tool(blocked_tool)
                .with_permission_policy(policy)
                .stream(),
        )
        .await;

        let blocked_result = resumed
            .tool_results
            .iter()
            .find(|result| result.contains("not permitted"))
            .expect("blocked tool result");
        assert!(blocked_result.contains("blocked_tool"), "{blocked_result}");
        assert!(
            resumed
                .tool_results
                .iter()
                .any(|result| result == "extra output"),
            "{:?}",
            resumed.tool_results
        );

        let record = store.load(&session_id).await.expect("stored session");
        let state_events = record
            .journal
            .iter()
            .filter(|event| matches!(event, SessionJournalEvent::MiddlewareStateUpdated { .. }))
            .count();
        assert_eq!(
            state_events, 1,
            "execution-only overrides must not journal middleware state"
        );
    }
}

#[cfg(test)]
mod agents_skills_state {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::StreamExt;
    use waltr_agent::{
        AgentRuntime, AgentSessions, AgentTimelineEvent, AgentsPromptConfig, CompletionEvent,
        ExternalRequestId, ExternalResponse, MemorySessionStore, ModelApi, ModelRef, ProviderId,
        ResumeInput, SessionId, SkillsConfig, TimelineContent, ToolCall,
        question::{QUESTION_TOOL_NAME, QuestionConfig},
        tools::filesystem::FilesystemConfig,
    };
    use serde_json::json;

    use crate::scripted_provider::{CapturingProvider, ScriptedProvider};

    fn test_model() -> ModelRef {
        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
    }

    fn question_call(id: &str) -> CompletionEvent {
        CompletionEvent::ToolCall(ToolCall::new(
            id,
            QUESTION_TOOL_NAME,
            json!({
                "questions": [
                    {
                        "question": "Continue?",
                        "header": "Continue",
                        "options": [
                            { "label": "Yes", "description": "Keep going" }
                        ],
                        "multiple": false
                    }
                ]
            }),
        ))
    }

    fn scripted_provider(
        script: Vec<Vec<CompletionEvent>>,
    ) -> Arc<CapturingProvider<ScriptedProvider>> {
        Arc::new(CapturingProvider::new(ScriptedProvider::from_turns(
            "scripted_provider",
            script,
        )))
    }

    struct RunOutput {
        tool_results: Vec<String>,
        external_request_id: Option<String>,
    }

    async fn drain_run<S>(mut stream: S) -> RunOutput
    where
        S: futures_util::Stream<Item = waltr_agent::Result<AgentTimelineEvent>> + Unpin,
    {
        let mut output = RunOutput {
            tool_results: Vec::new(),
            external_request_id: None,
        };
        while let Some(event) = stream.next().await {
            match event.expect("agent event").content {
                TimelineContent::ToolResult { result } => output.tool_results.push(result.content),
                TimelineContent::ExternalRequest { request } => {
                    output.external_request_id = Some(request.id.as_str().to_owned());
                }
                _ => {}
            }
        }
        output
    }

    fn question_answer(request_id: String) -> ResumeInput {
        ResumeInput::ExternalResponse {
            request_id: ExternalRequestId::new(request_id),
            response: ExternalResponse::new(json!("yes")),
        }
    }

    #[tokio::test]
    async fn agents_prompt_replays_stored_content_after_resume_despite_disk_changes() {
        let temp = TempRoot::new("waltr-session-agents-static");
        temp.write("AGENTS.md", "Original instructions marker-original.\n");
        let other = TempRoot::new("waltr-session-agents-other");
        other.write("AGENTS.md", "Other instructions marker-other.\n");
        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![
            vec![question_call("q-1")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);

        let agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .with_question_tool(QuestionConfig::new())
            .build();
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let run = drain_run(
            sessions
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let request_id = run.external_request_id.expect("question request");

        temp.write("AGENTS.md", "Modified instructions marker-modified.\n");

        let resume_agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_agents_system_prompt(AgentsPromptConfig::new(other.path()))
            .with_question_tool(QuestionConfig::new())
            .build();
        drain_run(
            AgentSessions::new(store)
                .resume(&resume_agent, session_id, question_answer(request_id))
                .stream(),
        )
        .await;

        let prompts = provider.system_prompts();
        assert_eq!(prompts.len(), 2);
        for prompt in &prompts {
            let prompt = prompt.as_deref().expect("system prompt");
            assert!(prompt.contains("marker-original"), "{prompt}");
            assert!(!prompt.contains("marker-modified"), "{prompt}");
            assert!(!prompt.contains("marker-other"), "{prompt}");
        }
    }

    #[tokio::test]
    async fn agents_read_injection_is_not_repeated_after_resume() {
        let temp = TempRoot::new("waltr-session-agents-read");
        temp.write("src/AGENTS.md", "Nested instructions marker-nested.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let store = MemorySessionStore::new();
        let read_call = |id: &str| {
            CompletionEvent::ToolCall(ToolCall::new(
                id,
                "read",
                json!({ "filePath": "src/main.rs" }),
            ))
        };
        let provider = scripted_provider(vec![
            vec![read_call("read-1")],
            vec![question_call("q-1")],
            vec![read_call("read-2")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let build_agent = || {
            AgentRuntime::builder(test_model(), provider.clone())
                .with_filesystem_tools(
                    FilesystemConfig::new(temp.path()).with_agents_read_context(),
                )
                .with_question_tool(QuestionConfig::new())
                .build()
        };

        let agent = build_agent();
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let run = drain_run(
            sessions
                .prompt(&agent, session_id.clone(), "read it")
                .stream(),
        )
        .await;
        assert_eq!(run.tool_results.len(), 1);
        assert!(run.tool_results[0].contains("<system-reminder>"));
        assert!(run.tool_results[0].contains("marker-nested"));
        let request_id = run.external_request_id.expect("question request");

        temp.write("src/AGENTS.md", "Changed instructions marker-changed.\n");

        let resumed = drain_run(
            AgentSessions::new(store)
                .resume(&build_agent(), session_id, question_answer(request_id))
                .stream(),
        )
        .await;
        let read_result = resumed
            .tool_results
            .iter()
            .find(|result| result.contains("fn main"))
            .expect("post-resume read result");
        assert!(!read_result.contains("<system-reminder>"), "{read_result}");
        assert!(!read_result.contains("marker-changed"), "{read_result}");
    }

    #[tokio::test]
    async fn loaded_skill_state_replays_after_resume_despite_disk_changes() {
        let temp = TempRoot::new("waltr-session-skills-static");
        temp.write(
            ".agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use example.\n---\nOriginal steps marker-skill-original.\n",
        );
        let store = MemorySessionStore::new();
        let load_call = |id: &str| {
            CompletionEvent::ToolCall(ToolCall::new(
                id,
                "load_skill",
                json!({ "name": "example" }),
            ))
        };
        let provider = scripted_provider(vec![
            vec![load_call("load-1")],
            vec![load_call("load-2")],
            vec![question_call("q-1")],
            vec![load_call("load-3")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let build_agent = || {
            AgentRuntime::builder(test_model(), provider.clone())
                .with_skills(SkillsConfig::new(temp.path()))
                .with_question_tool(QuestionConfig::new())
                .build()
        };

        let agent = build_agent();
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let run = drain_run(
            sessions
                .prompt(&agent, session_id.clone(), "load it")
                .stream(),
        )
        .await;
        assert_eq!(run.tool_results.len(), 2);
        assert!(run.tool_results[0].contains("marker-skill-original"));
        assert!(run.tool_results[1].contains("already loaded"));
        let request_id = run.external_request_id.expect("question request");

        temp.write(
            ".agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use example.\n---\nModified steps marker-skill-modified.\n",
        );
        temp.write(
            ".agents/skills/extra/SKILL.md",
            "---\nname: extra\ndescription: Added after suspension.\n---\nExtra steps.\n",
        );

        let resumed = drain_run(
            AgentSessions::new(store)
                .resume(&build_agent(), session_id, question_answer(request_id))
                .stream(),
        )
        .await;
        assert_eq!(resumed.tool_results.len(), 1);
        assert_eq!(resumed.tool_results[0], run.tool_results[1]);
        assert!(!resumed.tool_results[0].contains("marker-skill-modified"));

        let prompts = provider.system_prompts();
        let resumed_prompt = prompts
            .last()
            .cloned()
            .flatten()
            .expect("post-resume system prompt");
        assert!(resumed_prompt.contains("example: Use example."));
        assert!(
            !resumed_prompt.contains("extra"),
            "catalog must replay from stored state: {resumed_prompt}"
        );
    }

    #[tokio::test]
    async fn ephemeral_run_uses_supplied_memory_store_for_middleware_state() {
        let temp = TempRoot::new("waltr-session-agents-ephemeral");
        temp.write("AGENTS.md", "Ephemeral instructions marker-ephemeral.\n");
        let store = MemorySessionStore::new();
        let provider = scripted_provider(vec![vec![
            CompletionEvent::Delta("ok".to_owned()),
            CompletionEvent::Finished,
        ]]);
        let agent = AgentRuntime::builder(test_model(), provider.clone())
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .build();

        drain_run(
            agent
                .prompt("hello")
                .with_session_store(store.clone(), SessionId::new("ephemeral-agents"))
                .stream(),
        )
        .await;

        let prompt = provider.system_prompts()[0].clone().expect("system prompt");
        assert!(prompt.contains("marker-ephemeral"));
        let record = waltr_agent::SessionStore::load(&store, &SessionId::new("ephemeral-agents"))
            .await
            .expect("stored session");
        let snapshot = record.snapshot();
        let state = snapshot
            .middleware_state
            .get("agents")
            .expect("captured agents state");
        assert!(state.to_string().contains("marker-ephemeral"));
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, content).expect("write temp file");
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}
