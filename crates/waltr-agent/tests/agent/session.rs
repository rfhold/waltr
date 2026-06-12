//! Shared agent test session helpers.

use std::{env, fmt::Write};

use futures_util::StreamExt;
use waltr_agent::{
    Agent, AgentEvent, LlmError, LlmStream, SessionSnapshot, ToolResult,
    middleware::ExternalRequestDraft,
};

#[allow(dead_code)]
pub const REPORT_TOOL_ERRORS_ENV: &str = "WALTR_AGENT_REPORT_TOOL_ERRORS";
pub const TEST_HARNESS_SYSTEM_PROMPT: &str = "You are running inside an automated test harness. Follow the user instructions exactly. Use tools when the user explicitly asks you to use them. When asked to report exact values, include those values verbatim. Keep final answers concise and do not add extra commentary unless requested.";

#[derive(Debug, Default)]
// TODO(agent-runtime): Replace helper-only state with durable AgentSession resume assertions.
pub struct AgentTestSession {
    pub text: String,
    pub final_snapshot: Option<SessionSnapshot>,
    pub saw_text_after_tool_result: bool,
    pub external_requests: Vec<ExternalRequestDraft>,
    tool_calls: usize,
    tool_results: usize,
    tool_errors: Vec<ToolResult>,
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
        agent: &Agent,
        provider: &impl LlmStream,
        prompt: &str,
    ) -> waltr_agent::Result<Self> {
        // TODO(agent-runtime): Exercise suspension, resume input, and pending external requests.
        let mut stream = agent
            .prompt(provider, prompt)
            .with_system_prompt_fragment(TEST_HARNESS_SYSTEM_PROMPT)
            .stream();
        let mut session = Self::default();
        let mut saw_tool_result = false;

        while let Some(event) = stream.next().await {
            match event? {
                AgentEvent::TextDelta { delta, .. } => {
                    if saw_tool_result && !delta.is_empty() {
                        session.saw_text_after_tool_result = true;
                    }
                    session.text.push_str(&delta);
                }
                AgentEvent::ToolCall { .. } => session.tool_calls += 1,
                AgentEvent::ToolResult { result, .. } => {
                    session.tool_results += 1;
                    saw_tool_result = true;
                    if result.content.starts_with("Tool error:") {
                        session.tool_errors.push(result);
                    }
                }
                AgentEvent::ExternalRequest { request, .. } => {
                    session.external_requests.push(request);
                }
                AgentEvent::ThinkingDelta { .. } | AgentEvent::TurnFinished { .. } => {}
                AgentEvent::Finished { snapshot } => session.final_snapshot = Some(snapshot),
            }
        }

        Ok(session)
    }

    pub fn require_tool_execution(&self, provider: &'static str) -> waltr_agent::Result<()> {
        if self.tool_calls == 0 || self.tool_results == 0 {
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

#[allow(dead_code)]
fn report_tool_errors_enabled() -> bool {
    env::var(REPORT_TOOL_ERRORS_ENV)
        .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

#[cfg(test)]
mod completion {
    use std::sync::Mutex;

    use futures_util::{StreamExt, stream};
    use waltr_agent::{
        Agent, AgentEvent, AgentTool, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
        CompletionRequest, LlmStream, MemorySessionStore, ModelApi, ModelRef, ProviderId,
        SessionSnapshot, SessionStatus, SessionStore, ToolCall, ToolDefinition,
    };
    use serde_json::json;

    struct ScriptedProvider {
        requests: Mutex<usize>,
        script: Vec<Vec<CompletionEvent>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Vec<CompletionEvent>>) -> Self {
            Self {
                requests: Mutex::new(0),
                script,
            }
        }
    }

    impl LlmStream for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("request lock");
            let index = *requests;
            *requests += 1;
            drop(requests);
            let events = self
                .script
                .get(index)
                .unwrap_or_else(|| panic!("unexpected provider request {index}"))
                .clone()
                .into_iter()
                .map(Ok)
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
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
            |_call| Ok("echo output".to_owned()),
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

    async fn collect_finished_snapshot<S>(mut stream: S) -> Option<SessionSnapshot>
    where
        S: futures_util::Stream<Item = waltr_agent::Result<AgentEvent>> + Unpin,
    {
        let mut finished = None;
        while let Some(event) = stream.next().await {
            if let AgentEvent::Finished { snapshot } = event.expect("agent event") {
                finished = Some(snapshot);
            }
        }
        finished
    }

    #[tokio::test]
    async fn finished_snapshot_matches_stored_snapshot() {
        let store = MemorySessionStore::new();
        let provider = ScriptedProvider::new(tool_then_text_script());
        let agent = Agent::builder(test_model()).with_tool(echo_tool()).build();
        let sessions = agent.sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        let finished = collect_finished_snapshot(
            sessions
                .prompt(&provider, session_id.clone(), "run the tool")
                .stream(),
        )
        .await
        .expect("finished snapshot");

        let record = store.load(&session_id).await.expect("stored record");
        assert_eq!(finished.revision, record.revision);
        assert_eq!(finished, record.snapshot);
        assert_eq!(finished.status, SessionStatus::Finished);
    }

    #[tokio::test]
    async fn ephemeral_run_emits_complete_snapshot() {
        let provider = ScriptedProvider::new(tool_then_text_script());
        let agent = Agent::builder(test_model())
            .with_system_prompt("ephemeral marker-prompt")
            .with_tool(echo_tool())
            .build();

        let snapshot = collect_finished_snapshot(agent.prompt(&provider, "run the tool").stream())
            .await
            .expect("finished snapshot");

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
        assert_eq!(snapshot.state.last_assistant_text(), "done");
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
        MemorySessionStore, Revision, SessionEvent, SessionId, SessionRecord, SessionSnapshot,
        SessionStore,
    };
    use serde_json::json;

    #[tokio::test]
    async fn middleware_state_round_trips_through_memory_store() {
        let store = MemorySessionStore::new();
        let id = SessionId::new("middleware-state-session");
        store
            .create(SessionRecord::new(id.clone()))
            .await
            .expect("created session");

        let revision = store
            .append(
                &id,
                vec![
                    SessionEvent::MiddlewareStateUpdated {
                        namespace: "agents".to_owned(),
                        state: json!({ "loaded": ["a"] }),
                    },
                    SessionEvent::MiddlewareStateUpdated {
                        namespace: "agents".to_owned(),
                        state: json!({ "loaded": ["a", "b"] }),
                    },
                ],
                Revision::ZERO,
            )
            .await
            .expect("appended events");

        let record = store.load(&id).await.expect("loaded record");
        let mut snapshot = SessionSnapshot::new(id.clone());
        snapshot.apply_events(&record.events);
        store
            .save_snapshot(&id, snapshot, revision)
            .await
            .expect("saved snapshot");

        let record = store.load(&id).await.expect("loaded record");
        assert_eq!(
            record.snapshot.middleware_state.get("agents"),
            Some(&json!({ "loaded": ["a", "b"] })),
            "last write per namespace must win after rebuild"
        );

        let serialized = serde_json::to_value(&record).expect("serialized record");
        let restored: SessionRecord = serde_json::from_value(serialized).expect("restored record");
        assert_eq!(restored, record);
    }

    #[test]
    fn record_without_middleware_state_field_deserializes_to_empty_map() {
        let record = SessionRecord::new("legacy-session");
        let mut value = serde_json::to_value(&record).expect("serialized record");
        value["snapshot"]
            .as_object_mut()
            .expect("snapshot object")
            .remove("middleware_state")
            .expect("field present in current schema");

        let restored: SessionRecord = serde_json::from_value(value).expect("legacy record");

        assert!(restored.snapshot.middleware_state.is_empty());
    }
}

#[cfg(test)]
mod tool_projection {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::{StreamExt, stream};
    use waltr_agent::{
        Agent, AgentEvent, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
        CompletionRequest, ExternalRequestId, ExternalResponse, LlmStream, MemorySessionStore,
        ModelApi, ModelRef, ProviderId, ResumeInput, ToolCall, ToolDefinition,
        question::QuestionConfig, tools::filesystem::FilesystemConfig,
    };
    use serde_json::json;

    struct CaptureToolsProvider {
        script: Vec<Vec<CompletionEvent>>,
        requests: Mutex<usize>,
        tools: Mutex<Vec<Vec<ToolDefinition>>>,
    }

    impl CaptureToolsProvider {
        fn new(script: Vec<Vec<CompletionEvent>>) -> Self {
            Self {
                script,
                requests: Mutex::new(0),
                tools: Mutex::new(Vec::new()),
            }
        }

        fn tool_description(&self, request: usize, name: &str) -> String {
            self.tools.lock().expect("captured tools")[request]
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("request {request} should expose tool {name}"))
                .description
                .clone()
        }
    }

    impl LlmStream for CaptureToolsProvider {
        fn name(&self) -> &'static str {
            "capture_tools_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("request lock");
            let index = *requests;
            *requests += 1;
            drop(requests);
            self.tools
                .lock()
                .expect("captured tools")
                .push(request.tools);
            let events = self
                .script
                .get(index)
                .unwrap_or_else(|| panic!("unexpected provider request {index}"))
                .clone()
                .into_iter()
                .map(Ok)
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
    }

    fn test_model() -> ModelRef {
        ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses)
    }

    fn filesystem_agent(root: &Path) -> Agent {
        Agent::builder(test_model())
            .with_filesystem_tools(FilesystemConfig::new(root))
            .with_question_tool(QuestionConfig::new())
            .build()
    }

    #[tokio::test]
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
        let provider = CaptureToolsProvider::new(vec![
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

        let sessions = filesystem_agent(&first_root.path()).sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let mut request_id = None;
        let mut stream = sessions
            .prompt(&provider, session_id.clone(), "hello")
            .stream();
        while let Some(event) = stream.next().await {
            if let AgentEvent::ExternalRequest { request, .. } = event.expect("agent event") {
                request_id = Some(request.id);
            }
        }
        drop(stream);

        let projected = provider.tool_description(0, "read");
        assert!(projected.contains(&first_hint), "{projected}");

        let resume_agent = filesystem_agent(&second_root.path());
        let resume_sessions = resume_agent.sessions(store);
        let mut stream = resume_sessions
            .resume(
                &provider,
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
    use std::sync::Mutex;

    use futures_util::{StreamExt, stream};
    use waltr_agent::{
        Agent, AgentEvent, AgentTool, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
        CompletionRequest, ExternalRequestId, ExternalResponse, LlmStream, MemorySessionStore,
        ModelApi, ModelRef, PermissionEffect, PermissionPolicy, PermissionToolSelector, ProviderId,
        ResumeInput, SessionEvent, SessionId, SessionRecord, SessionStore, ToolCall,
        ToolDefinition,
        question::{QUESTION_TOOL_NAME, QuestionConfig},
    };
    use serde_json::json;

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

    struct ScriptedProvider {
        requests: Mutex<usize>,
        system_prompts: Mutex<Vec<Option<String>>>,
        script: Vec<Vec<CompletionEvent>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Vec<CompletionEvent>>) -> Self {
            Self {
                requests: Mutex::new(0),
                system_prompts: Mutex::new(Vec::new()),
                script,
            }
        }

        fn system_prompts(&self) -> Vec<Option<String>> {
            self.system_prompts
                .lock()
                .expect("system prompt lock")
                .clone()
        }
    }

    impl LlmStream for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("request lock");
            let index = *requests;
            *requests += 1;
            drop(requests);
            self.system_prompts
                .lock()
                .expect("system prompt lock")
                .push(request.system_prompt.map(str::to_owned));
            let events = self
                .script
                .get(index)
                .unwrap_or_else(|| panic!("unexpected provider request {index}"))
                .clone()
                .into_iter()
                .map(Ok)
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
    }

    struct RunOutput {
        tool_results: Vec<String>,
        external_request_id: Option<String>,
    }

    async fn drain_run<S>(mut stream: S) -> RunOutput
    where
        S: futures_util::Stream<Item = waltr_agent::Result<AgentEvent>> + Unpin,
    {
        let mut output = RunOutput {
            tool_results: Vec::new(),
            external_request_id: None,
        };
        while let Some(event) = stream.next().await {
            match event.expect("agent event") {
                AgentEvent::ToolResult { result, .. } => output.tool_results.push(result.content),
                AgentEvent::ExternalRequest { request, .. } => {
                    output.external_request_id = Some(request.id);
                }
                _ => {}
            }
        }
        output
    }

    fn system_prompt_state_indices(record: &SessionRecord) -> Vec<usize> {
        record
            .events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(
                    event,
                    SessionEvent::MiddlewareStateUpdated { namespace, .. }
                        if namespace == "system_prompt"
                )
                .then_some(index)
            })
            .collect()
    }

    fn request_record_indices(record: &SessionRecord) -> Vec<usize> {
        record
            .events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event, SessionEvent::ProviderRequestRecorded { .. }).then_some(index)
            })
            .collect()
    }

    async fn create_session_id<S>(agent: &Agent, store: &S) -> SessionId
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        agent
            .clone()
            .sessions(store.clone())
            .create()
            .await
            .expect("created session")
            .id()
            .clone()
    }

    #[tokio::test]
    async fn rebuilt_agent_renders_stored_prompt_on_initialized_session() {
        let store = MemorySessionStore::new();
        let provider = ScriptedProvider::new(vec![
            vec![question_call("q-1")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let agent = Agent::builder(test_model())
            .with_system_prompt("base prompt marker-first")
            .with_question_tool(QuestionConfig::new())
            .build();
        let session_id = create_session_id(&agent, &store).await;
        let run = drain_run(
            agent
                .sessions(store.clone())
                .prompt(&provider, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let request_id = run.external_request_id.expect("question request");

        let rebuilt = Agent::builder(test_model())
            .with_system_prompt("base prompt marker-second")
            .with_question_tool(QuestionConfig::new())
            .build();
        drain_run(
            rebuilt
                .sessions(store.clone())
                .resume(&provider, session_id.clone(), question_answer(request_id))
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
        let persisted = record.requests.last().expect("persisted request record");
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
        let provider = ScriptedProvider::new(vec![vec![
            CompletionEvent::Delta("ok".to_owned()),
            CompletionEvent::Finished,
        ]]);
        let agent = Agent::builder(test_model())
            .with_system_prompt("configured marker-configured")
            .build();
        let session_id = create_session_id(&agent, &store).await;

        drain_run(
            agent
                .sessions(store.clone())
                .prompt(&provider, session_id.clone(), "hello")
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
        assert_eq!(
            record.snapshot.middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "override marker-override" }))
        );
    }

    #[tokio::test]
    async fn run_override_on_initialized_session_journals_update_and_replays() {
        let store = MemorySessionStore::new();
        let provider = ScriptedProvider::new(vec![
            vec![question_call("q-1")],
            vec![question_call("q-2")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let agent = Agent::builder(test_model())
            .with_system_prompt("configured marker-configured")
            .with_question_tool(QuestionConfig::new())
            .build();
        let session_id = create_session_id(&agent, &store).await;
        let sessions = agent.sessions(store.clone());

        let first = drain_run(
            sessions
                .prompt(&provider, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let first_request_id = first.external_request_id.expect("first question request");

        let second = drain_run(
            sessions
                .resume(
                    &provider,
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
                    &provider,
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
        assert_eq!(
            record.snapshot.middleware_state.get("system_prompt"),
            Some(&json!({ "prompt": "override marker-override" }))
        );
    }

    #[tokio::test]
    async fn execution_only_overrides_apply_without_state_events() {
        let store = MemorySessionStore::new();
        let provider = ScriptedProvider::new(vec![
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
        let agent = Agent::builder(test_model())
            .with_question_tool(QuestionConfig::new())
            .build();
        let session_id = create_session_id(&agent, &store).await;
        let sessions = agent.sessions(store.clone());

        let first = drain_run(
            sessions
                .prompt(&provider, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let request_id = first.external_request_id.expect("question request");

        let schema = json!({ "type": "object", "additionalProperties": false });
        let extra_tool = AgentTool::new(
            ToolDefinition::new("extra_tool", "Extra run tool.", schema.clone()),
            |_call| Ok("extra output".to_owned()),
        );
        let blocked_tool = AgentTool::new(
            ToolDefinition::new("blocked_tool", "Blocked run tool.", schema),
            |_call| Ok("blocked output".to_owned()),
        );
        let policy = PermissionPolicy::default().with_tool_rule(
            PermissionToolSelector::exact("blocked_tool"),
            PermissionEffect::deny(),
        );

        let resumed = drain_run(
            sessions
                .resume(&provider, session_id.clone(), question_answer(request_id))
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
            .events
            .iter()
            .filter(|event| matches!(event, SessionEvent::MiddlewareStateUpdated { .. }))
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
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::{StreamExt, stream};
    use waltr_agent::{
        Agent, AgentEvent, AgentsPromptConfig, BoxCompletionStream, BoxStreamFuture,
        CompletionEvent, CompletionRequest, ExternalRequestId, ExternalResponse, LlmStream,
        MemorySessionStore, ModelApi, ModelRef, ProviderId, ResumeInput, SessionId, SkillsConfig,
        ToolCall,
        question::{QUESTION_TOOL_NAME, QuestionConfig},
        tools::filesystem::FilesystemConfig,
    };
    use serde_json::json;

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

    struct ScriptedProvider {
        requests: Mutex<usize>,
        system_prompts: Mutex<Vec<Option<String>>>,
        script: Vec<Vec<CompletionEvent>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Vec<CompletionEvent>>) -> Self {
            Self {
                requests: Mutex::new(0),
                system_prompts: Mutex::new(Vec::new()),
                script,
            }
        }

        fn system_prompts(&self) -> Vec<Option<String>> {
            self.system_prompts
                .lock()
                .expect("system prompt lock")
                .clone()
        }
    }

    impl LlmStream for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("request lock");
            let index = *requests;
            *requests += 1;
            drop(requests);
            self.system_prompts
                .lock()
                .expect("system prompt lock")
                .push(request.system_prompt.map(str::to_owned));
            let events = self
                .script
                .get(index)
                .unwrap_or_else(|| panic!("unexpected provider request {index}"))
                .clone()
                .into_iter()
                .map(Ok)
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
    }

    struct RunOutput {
        tool_results: Vec<String>,
        external_request_id: Option<String>,
    }

    async fn drain_run<S>(mut stream: S) -> RunOutput
    where
        S: futures_util::Stream<Item = waltr_agent::Result<AgentEvent>> + Unpin,
    {
        let mut output = RunOutput {
            tool_results: Vec::new(),
            external_request_id: None,
        };
        while let Some(event) = stream.next().await {
            match event.expect("agent event") {
                AgentEvent::ToolResult { result, .. } => output.tool_results.push(result.content),
                AgentEvent::ExternalRequest { request, .. } => {
                    output.external_request_id = Some(request.id);
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
        let provider = ScriptedProvider::new(vec![
            vec![question_call("q-1")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);

        let agent = Agent::builder(test_model())
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .with_question_tool(QuestionConfig::new())
            .build();
        let sessions = agent.sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let run = drain_run(
            sessions
                .prompt(&provider, session_id.clone(), "hello")
                .stream(),
        )
        .await;
        let request_id = run.external_request_id.expect("question request");

        temp.write("AGENTS.md", "Modified instructions marker-modified.\n");

        let resume_agent = Agent::builder(test_model())
            .with_agents_system_prompt(AgentsPromptConfig::new(other.path()))
            .with_question_tool(QuestionConfig::new())
            .build();
        drain_run(
            resume_agent
                .sessions(store)
                .resume(&provider, session_id, question_answer(request_id))
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
        let provider = ScriptedProvider::new(vec![
            vec![read_call("read-1")],
            vec![question_call("q-1")],
            vec![read_call("read-2")],
            vec![
                CompletionEvent::Delta("done".to_owned()),
                CompletionEvent::Finished,
            ],
        ]);
        let build_agent = || {
            Agent::builder(test_model())
                .with_filesystem_tools(
                    FilesystemConfig::new(temp.path()).with_agents_read_context(),
                )
                .with_question_tool(QuestionConfig::new())
                .build()
        };

        let sessions = build_agent().sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let run = drain_run(
            sessions
                .prompt(&provider, session_id.clone(), "read it")
                .stream(),
        )
        .await;
        assert_eq!(run.tool_results.len(), 1);
        assert!(run.tool_results[0].contains("<system-reminder>"));
        assert!(run.tool_results[0].contains("marker-nested"));
        let request_id = run.external_request_id.expect("question request");

        temp.write("src/AGENTS.md", "Changed instructions marker-changed.\n");

        let resumed = drain_run(
            build_agent()
                .sessions(store)
                .resume(&provider, session_id, question_answer(request_id))
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
        let provider = ScriptedProvider::new(vec![
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
            Agent::builder(test_model())
                .with_skills(SkillsConfig::new(temp.path()))
                .with_question_tool(QuestionConfig::new())
                .build()
        };

        let sessions = build_agent().sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let run = drain_run(
            sessions
                .prompt(&provider, session_id.clone(), "load it")
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
            build_agent()
                .sessions(store)
                .resume(&provider, session_id, question_answer(request_id))
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
        let provider = ScriptedProvider::new(vec![vec![
            CompletionEvent::Delta("ok".to_owned()),
            CompletionEvent::Finished,
        ]]);
        let agent = Agent::builder(test_model())
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .build();

        drain_run(
            agent
                .prompt(&provider, "hello")
                .with_session_store(store.clone(), SessionId::new("ephemeral-agents"))
                .stream(),
        )
        .await;

        let prompt = provider.system_prompts()[0].clone().expect("system prompt");
        assert!(prompt.contains("marker-ephemeral"));
        let record = waltr_agent::SessionStore::load(&store, &SessionId::new("ephemeral-agents"))
            .await
            .expect("stored session");
        let state = record
            .snapshot
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
