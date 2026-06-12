//! Agent configuration and loop execution.

use std::fmt;

use crate::{LlmStream, ModelRef, PermissionPolicy, middleware::chain::MiddlewareChain};

mod builder;
mod event;
mod r#loop;
mod run;
pub mod session;
mod state;
mod tool;

pub use builder::AgentBuilder;
pub use event::{AgentEvent, BoxAgentEventStream};
pub use run::AgentRunBuilder;
pub use session::{
    AgentSession, AgentSessionRuntime, AgentSessions, CancelReason, ExternalRequest,
    ExternalRequestId, ExternalRequestKind, ExternalRequestStatus, ExternalResponse,
    FilesystemSessionStore, MemorySessionStore, OpenDalSessionStore, PendingWork,
    PendingWorkStatus, RequestId, RequestRecord, ResumeInput, Revision, RuntimeResumeOutput,
    S3SessionStore, ScyllaSessionStore, SessionEvent, SessionId, SessionRecord, SessionSnapshot,
    SessionStatus, SessionStore, SessionStoreError, SessionStoreResult, Suspension,
    opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use state::{AgentState, AgentTranscriptEvent};
pub use tool::AgentTool;

/// Agent configuration and enabled capabilities.
#[derive(Clone)]
pub struct Agent {
    pub(super) model: ModelRef,
    pub(super) tools: Vec<AgentTool>,
    pub(super) middleware_chain: MiddlewareChain,
    pub(super) permission_policy: PermissionPolicy,
}

impl Agent {
    /// Start building an agent for a model.
    #[must_use]
    pub fn builder(model: ModelRef) -> AgentBuilder {
        AgentBuilder::new(model)
    }

    /// Return the model identity configured for this agent.
    #[must_use]
    pub const fn model(&self) -> &ModelRef {
        &self.model
    }

    /// Start building an agent loop request.
    pub fn prompt<'a, P>(
        &'a self,
        provider: &'a P,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<'a, P>
    where
        P: LlmStream + ?Sized,
    {
        AgentRunBuilder::new(self, provider, prompt.into())
    }

    /// Bind this agent configuration to a durable session store.
    pub const fn sessions<S>(self, store: S) -> AgentSessions<S>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        AgentSessions::new(self, store)
    }

    /// Return configured host tools.
    #[must_use]
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("middleware_chain_len", &self.middleware_chain.len())
            .field("permission_policy", &self.permission_policy)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::{StreamExt, stream};
    use serde_json::json;

    use crate::{
        AgentsPromptConfig, BoxCompletionStream, BoxStreamFuture, CompletionEvent,
        CompletionRequest, ExternalRequestId, ExternalResponse, FilesystemPromptConfig, LlmError,
        MemorySessionStore, ModelApi, ModelRef, ProviderId, ResumeInput, SessionEvent,
        SessionStatus, SessionStore, SkillsConfig, ThinkingBlock, ThinkingBlockKind, ThinkingDelta,
        ToolCall, ToolDefinition,
        middleware::{
            AgentMiddleware, ExternalRequestKind, HookOutcome,
            chain::MiddlewareChain,
            context::{MiddlewareStateAccess, ModelRequestDraft, RequestContext},
        },
        tools::{
            filesystem::FilesystemConfig,
            question::{QUESTION_TOOL_NAME, QuestionConfig},
        },
    };

    use super::{Agent, AgentEvent, AgentTool};

    struct NoopMiddleware;

    impl AgentMiddleware for NoopMiddleware {}

    #[test]
    fn builder_assembles_system_prompt_middleware_by_default() {
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);

        let agent = Agent::builder(model).build();

        assert_eq!(agent.middleware_chain.len(), 1);
    }

    #[test]
    fn builder_accepts_middleware_chain() {
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let mut chain = MiddlewareChain::new();
        chain.push(NoopMiddleware);

        let agent = Agent::builder(model.clone())
            .with_middleware(NoopMiddleware)
            .build();
        let chain_agent = Agent::builder(model).with_middleware_chain(chain).build();

        assert_eq!(agent.middleware_chain.len(), 2);
        assert_eq!(chain_agent.middleware_chain.len(), 2);
    }

    #[test]
    fn filesystem_tools_are_opt_in() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);

        assert!(Agent::builder(model.clone()).build().tools().is_empty());
        assert!(
            Agent::builder(model)
                .with_filesystem_tools(FilesystemConfig::new("."))
                .build()
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "apply_patch")
        );
    }

    #[tokio::test]
    async fn durable_session_resume_continues_same_agent_loop() {
        let provider = DurableResumeProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_question_tool(QuestionConfig::new())
            .build();
        let store = MemorySessionStore::new();
        let sessions = agent.sessions(store.clone());
        let session = sessions.create().await.expect("created session");
        let session_id = session.id().clone();

        let mut stream = sessions
            .prompt(&provider, session_id.clone(), "ask then continue")
            .stream();
        let mut request_id = None;
        while let Some(event) = stream.next().await {
            if let AgentEvent::ExternalRequest { request, .. } = event.expect("agent event") {
                assert_eq!(request.kind, ExternalRequestKind::Question);
                request_id = Some(request.id);
            }
        }
        let request_id = request_id.expect("question request");
        assert!(
            store
                .load(&session_id)
                .await
                .expect("stored session")
                .snapshot
                .external_requests
                .iter()
                .any(|request| request.kind == super::ExternalRequestKind::Question)
        );

        let mut resumed = sessions
            .resume(
                &provider,
                session_id.clone(),
                ResumeInput::ExternalResponse {
                    request_id: ExternalRequestId::new(request_id),
                    response: ExternalResponse::new(json!("resume-answer-123")),
                },
            )
            .stream();
        let mut text = String::new();
        while let Some(event) = resumed.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "resumed-with-answer");
        assert!(provider.saw_resumed_tool_result());
        assert!(
            store
                .load(&session_id)
                .await
                .expect("stored session")
                .snapshot
                .tool_results()
                .any(|result| result.name == QUESTION_TOOL_NAME
                    && result.content == "resume-answer-123")
        );
    }

    #[derive(Default)]
    struct DurableResumeProvider {
        requests: Mutex<usize>,
        saw_resumed_tool_result: Mutex<bool>,
    }

    impl DurableResumeProvider {
        fn saw_resumed_tool_result(&self) -> bool {
            *self
                .saw_resumed_tool_result
                .lock()
                .expect("resume flag lock")
        }
    }

    impl crate::LlmStream for DurableResumeProvider {
        fn name(&self) -> &'static str {
            "durable_resume_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("request lock");
            let events = match *requests {
                0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                    "question-call-1",
                    QUESTION_TOOL_NAME,
                    json!({
                        "questions": [
                            {
                                "question": "Choose a path",
                                "header": "Path",
                                "options": [
                                    {
                                        "label": "Resume",
                                        "description": "Continue the durable session"
                                    }
                                ],
                                "multiple": false
                            }
                        ]
                    }),
                )))],
                1 => {
                    assert!(
                        request
                            .tool_results()
                            .any(|result| result.name == QUESTION_TOOL_NAME
                                && result.content == "resume-answer-123"),
                        "resumed provider request did not include question answer: {request:?}"
                    );
                    *self
                        .saw_resumed_tool_result
                        .lock()
                        .expect("resume flag lock") = true;
                    vec![
                        Ok(CompletionEvent::Delta("resumed-with-answer".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                turn => panic!("unexpected provider request turn {turn}"),
            };
            *requests += 1;
            drop(requests);
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
    }

    struct StateRecordingMiddleware {
        namespace: &'static str,
        write: Option<serde_json::Value>,
        observed: Arc<Mutex<Vec<Option<serde_json::Value>>>>,
    }

    impl AgentMiddleware for StateRecordingMiddleware {
        async fn before_provider_request(
            &self,
            ctx: &mut RequestContext<'_>,
            _request: &mut ModelRequestDraft,
        ) -> crate::Result<HookOutcome> {
            self.observed
                .lock()
                .expect("observed state lock")
                .push(ctx.state(self.namespace).cloned());
            if let Some(state) = &self.write
                && ctx.state(self.namespace).is_none()
            {
                ctx.set_state(self.namespace, state.clone());
            }
            Ok(HookOutcome::Continue)
        }
    }

    struct TextOnlyProvider;

    impl crate::LlmStream for TextOnlyProvider {
        fn name(&self) -> &'static str {
            "text_only_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("ok".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn middleware_state_write_is_journaled_before_provider_request_record() {
        let provider = TextOnlyProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder(model)
            .with_middleware(StateRecordingMiddleware {
                namespace: "test-state",
                write: Some(json!({ "writes": 1 })),
                observed: Arc::clone(&observed),
            })
            .build();
        let store = MemorySessionStore::new();
        let sessions = agent.sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        drain(
            sessions
                .prompt(&provider, session_id.clone(), "hello")
                .stream(),
        )
        .await;

        let record = store.load(&session_id).await.expect("stored session");
        let state_index = record
            .events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    SessionEvent::MiddlewareStateUpdated { namespace, .. }
                        if namespace == "test-state"
                )
            })
            .expect("state event journaled");
        let request_index = record
            .events
            .iter()
            .position(|event| matches!(event, SessionEvent::ProviderRequestRecorded { .. }))
            .expect("request record journaled");
        assert!(
            state_index < request_index,
            "state event must precede the provider request record it influenced"
        );
        assert_eq!(
            record.snapshot.middleware_state.get("test-state"),
            Some(&json!({ "writes": 1 }))
        );
        assert_eq!(*observed.lock().expect("observed state lock"), vec![None]);
    }

    #[tokio::test]
    async fn middleware_state_written_before_suspension_is_observable_after_resume() {
        let store = MemorySessionStore::new();
        let provider = DurableResumeProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let written = json!({ "grants": ["shell"] });

        let writer_observed = Arc::new(Mutex::new(Vec::new()));
        let writer_agent = Agent::builder(model.clone())
            .with_question_tool(QuestionConfig::new())
            .with_middleware(StateRecordingMiddleware {
                namespace: "permissions",
                write: Some(written.clone()),
                observed: Arc::clone(&writer_observed),
            })
            .build();
        let sessions = writer_agent.sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        let mut events = sessions
            .prompt(&provider, session_id.clone(), "ask then continue")
            .stream();
        let mut request_id = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ExternalRequest { request, .. } = event.expect("agent event") {
                request_id = Some(request.id);
            }
        }
        drop(events);
        let request_id = request_id.expect("question request");
        assert_eq!(
            *writer_observed.lock().expect("observed state lock"),
            vec![None]
        );

        let reader_observed = Arc::new(Mutex::new(Vec::new()));
        let reader_agent = Agent::builder(model)
            .with_question_tool(QuestionConfig::new())
            .with_middleware(StateRecordingMiddleware {
                namespace: "permissions",
                write: None,
                observed: Arc::clone(&reader_observed),
            })
            .build();
        drain(
            reader_agent
                .sessions(store.clone())
                .resume(
                    &provider,
                    session_id,
                    ResumeInput::ExternalResponse {
                        request_id: ExternalRequestId::new(request_id),
                        response: ExternalResponse::new(json!("resume-answer-123")),
                    },
                )
                .stream(),
        )
        .await;

        assert_eq!(
            *reader_observed.lock().expect("observed state lock"),
            vec![Some(written)],
            "resumed run must observe state written before suspension"
        );
    }

    fn expected_turn_metadata() -> crate::ProviderTurnMetadata {
        crate::ProviderTurnMetadata {
            provider: "metadata_provider".to_owned(),
            url: "memory://metadata-provider".to_owned(),
            model: "metadata-model".to_owned(),
            usage: Some(crate::TokenUsage {
                input_tokens: Some(11),
                output_tokens: Some(3),
            }),
            resume: json!({ "response_id": "resp-1" }),
        }
    }

    #[derive(Default)]
    struct MetadataEmittingProvider {
        requests: Mutex<usize>,
    }

    impl crate::LlmStream for MetadataEmittingProvider {
        fn name(&self) -> &'static str {
            "metadata_provider"
        }

        fn project_request(&self, request: &CompletionRequest<'_>) -> Option<serde_json::Value> {
            Some(json!({ "wire": "projected", "prompt": request.prompt }))
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("request lock");
            let events = match *requests {
                0 => vec![
                    Ok(CompletionEvent::Delta("first".to_owned())),
                    Ok(CompletionEvent::ProviderMetadata(expected_turn_metadata())),
                    Ok(CompletionEvent::Finished),
                ],
                1 => {
                    assert_eq!(
                        request
                            .turns
                            .first()
                            .and_then(|turn| turn.metadata.as_ref()),
                        Some(&expected_turn_metadata()),
                        "same-family provider should see prior turn metadata"
                    );
                    vec![
                        Ok(CompletionEvent::Delta("second".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                }
                turn => panic!("unexpected provider request {turn}"),
            };
            *requests += 1;
            drop(requests);
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
    }

    struct ForeignMetadataProvider;

    impl crate::LlmStream for ForeignMetadataProvider {
        fn name(&self) -> &'static str {
            "other_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            assert_eq!(
                request
                    .turns
                    .first()
                    .and_then(|turn| turn.metadata.as_ref()),
                Some(&expected_turn_metadata()),
                "generic agent loop should not filter provider metadata: {request:?}"
            );
            let events = vec![Ok(CompletionEvent::Finished)];
            Box::pin(async move { Ok(Box::pin(stream::iter(events)) as BoxCompletionStream) })
        }
    }

    async fn drain<S>(mut stream: S)
    where
        S: futures_util::Stream<Item = crate::Result<AgentEvent>> + Unpin,
    {
        while let Some(event) = stream.next().await {
            event.expect("agent event");
        }
    }

    #[tokio::test]
    async fn provider_projection_and_turn_metadata_are_journaled() {
        let provider = MetadataEmittingProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model).build();
        let store = MemorySessionStore::new();
        let sessions = agent.sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        drain(
            sessions
                .prompt(&provider, session_id.clone(), "hello")
                .stream(),
        )
        .await;

        let record = store.load(&session_id).await.expect("stored session");
        let request = record.requests.first().expect("journaled request record");
        assert_eq!(
            request.projected_request,
            json!({ "wire": "projected", "prompt": "hello" }),
            "projected request must be the provider wire payload"
        );
        assert_ne!(request.projected_request, request.canonical_request);
        assert_eq!(
            record
                .snapshot
                .turns
                .first()
                .and_then(|turn| turn.metadata.clone()),
            Some(expected_turn_metadata())
        );
    }

    #[tokio::test]
    async fn provider_turn_metadata_is_forwarded_without_agent_filtering() {
        let provider = MetadataEmittingProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model).build();
        let store = MemorySessionStore::new();
        let sessions = agent.sessions(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        drain(
            sessions
                .prompt(&provider, session_id.clone(), "first prompt")
                .stream(),
        )
        .await;
        // Same provider sees the journaled metadata (asserted in-provider).
        drain(
            sessions
                .prompt(&provider, session_id.clone(), "second prompt")
                .stream(),
        )
        .await;
        // Filtering is provider-owned, not performed by the generic agent loop.
        drain(
            sessions
                .prompt(&ForeignMetadataProvider, session_id, "third prompt")
                .stream(),
        )
        .await;
    }

    #[tokio::test]
    async fn agents_system_prompt_is_sent_to_provider() {
        let temp = TempRoot::new("waltr-agent-static-agents-test");
        temp.write("AGENTS.md", "Prefer concise answers.\n");
        let provider = AgentsSystemPromptProvider {
            expected_path: temp.path().join("AGENTS.md"),
        };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "hello").stream();
        let mut text = String::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "ok");
    }

    struct AgentsSystemPromptProvider {
        expected_path: PathBuf,
    }

    impl crate::LlmStream for AgentsSystemPromptProvider {
        fn name(&self) -> &'static str {
            "agents_system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains(&format!(
                    "Instructions from: {}",
                    self.expected_path.display()
                )));
                assert!(system_prompt.contains("Prefer concise answers."));

                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("ok".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn filesystem_read_agents_context_is_opt_in_and_loaded_once() {
        let temp = TempRoot::new("waltr-agent-read-agents-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = RepeatedReadProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
            .build();

        let mut events = agent.prompt(&provider, "read twice").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("<system-reminder>"));
        assert!(tool_results[0].contains("Nested instructions."));
        assert!(tool_results[0].contains("Root instructions."));
        assert!(!tool_results[1].contains("<system-reminder>"));
    }

    #[tokio::test]
    async fn filesystem_read_agents_context_is_disabled_by_default() {
        let temp = TempRoot::new("waltr-agent-read-agents-disabled-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_filesystem_tools(FilesystemConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(!tool_result.contains("<system-reminder>"));
        assert!(!tool_result.contains("Root instructions."));
        assert!(!tool_result.contains("Nested instructions."));
    }

    #[tokio::test]
    async fn filesystem_read_agents_context_skips_static_root_instruction() {
        let temp = TempRoot::new("waltr-agent-read-static-agents-test");
        temp.write("AGENTS.md", "Root instructions.\n");
        temp.write("src/AGENTS.md", "Nested instructions.\n");
        temp.write("src/main.rs", "fn main() {}\n");
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_agents_system_prompt(AgentsPromptConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_agents_read_context())
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(tool_result.contains("Nested instructions."));
        assert!(!tool_result.contains("Root instructions."));
    }

    #[tokio::test]
    async fn skills_system_prompt_lists_root_skills_and_exposes_loader() {
        let temp = TempRoot::new("waltr-agent-skills-system-test");
        temp.write_skill(
            "example",
            "Use for example tasks.",
            "# Example\nFollow example steps.\n",
        );
        let provider = SkillsSystemPromptProvider {
            expected_root: temp.path().canonicalize().expect("canonical temp root"),
        };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "hello").stream();
        while events.next().await.is_some() {}
    }

    #[tokio::test]
    async fn load_skill_loads_content_and_lists_resources_once() {
        let temp = TempRoot::new("waltr-agent-load-skill-test");
        temp.write_skill(
            "example",
            "Use for example tasks.",
            "# Example\nFollow example steps.\n",
        );
        temp.write(".agents/skills/example/references/guide.md", "details\n");
        let provider = LoadSkillProvider::new(json!({ "name": "example" }));
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "load skill").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("<skill_content name=\"example\">"));
        assert!(tool_results[0].contains("Follow example steps."));
        assert!(tool_results[0].contains("- references/guide.md"));
        assert!(tool_results[1].contains("already loaded"));
    }

    #[tokio::test]
    async fn duplicate_skill_names_require_root() {
        let temp = TempRoot::new("waltr-agent-duplicate-skills-test");
        temp.write_skill("example", "Use root example.", "# Root Example\n");
        temp.write(
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
        );
        temp.write("src/main.rs", "fn main() {}\n");
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let provider = DuplicateSkillProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "read and load").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("New skills are available"));
        assert!(tool_results[0].contains("Use app example."));
        assert!(tool_results[1].contains("Tool error:"));
        assert!(tool_results[1].contains("ambiguous"));
    }

    #[tokio::test]
    async fn duplicate_skill_names_load_with_root() {
        let temp = TempRoot::new("waltr-agent-rooted-skill-test");
        temp.write_skill("example", "Use root example.", "# Root Example\n");
        temp.write(
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
        );
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let app_root = temp
            .path()
            .join("packages/app")
            .canonicalize()
            .expect("canonical app root");
        let provider = RootedSkillProvider { root: app_root };
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "read and load").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[1].contains("<skill_content name=\"example\">"));
        assert!(tool_results[1].contains("# App Example"));
        assert!(!tool_results[1].contains("# Root Example"));
    }

    #[tokio::test]
    async fn glob_directory_walk_reveals_skills_once() {
        let temp = TempRoot::new("waltr-agent-glob-skills-test");
        temp.write(
            "packages/app/.agents/skills/app-skill/SKILL.md",
            "---\nname: app-skill\ndescription: Use app skill.\n---\n# App Skill\n",
        );
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let provider = RepeatedGlobProvider::default();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()).with_skills_context())
            .build();

        let mut events = agent.prompt(&provider, "glob twice").stream();
        let mut tool_results = Vec::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_results.push(result.content);
            }
        }

        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].contains("New skills are available"));
        assert!(tool_results[0].contains("app-skill: Use app skill."));
        assert!(!tool_results[1].contains("New skills are available"));
    }

    #[tokio::test]
    async fn dynamic_skills_discovery_is_opt_in() {
        let temp = TempRoot::new("waltr-agent-skills-disabled-test");
        temp.write(
            "packages/app/.agents/skills/example/SKILL.md",
            "---\nname: example\ndescription: Use app example.\n---\n# App Example\n",
        );
        temp.write("src/main.rs", "fn main() {}\n");
        temp.write("packages/app/src/main.rs", "fn main() {}\n");
        let provider = SingleReadProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_skills(SkillsConfig::new(temp.path()))
            .with_filesystem_tools(FilesystemConfig::new(temp.path()))
            .build();

        let mut events = agent.prompt(&provider, "read once").stream();
        let mut tool_result = None;
        while let Some(event) = events.next().await {
            if let AgentEvent::ToolResult { result, .. } = event.expect("agent event") {
                tool_result = Some(result.content);
            }
        }

        let tool_result = tool_result.expect("tool result");
        assert!(!tool_result.contains("New skills are available"));
        assert!(!tool_result.contains("Use app example."));
    }

    struct SkillsSystemPromptProvider {
        expected_root: PathBuf,
    }

    impl crate::LlmStream for SkillsSystemPromptProvider {
        fn name(&self) -> &'static str {
            "skills_system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains("Available skills:"));
                assert!(system_prompt.contains("example: Use for example tasks."));
                assert!(system_prompt.contains(&format!("Root: {}", self.expected_root.display())));
                assert!(request.tools.iter().any(|tool| tool.name == "load_skill"));

                Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream)
            })
        }
    }

    struct LoadSkillProvider {
        requests: Mutex<usize>,
        arguments: serde_json::Value,
    }

    impl LoadSkillProvider {
        fn new(arguments: serde_json::Value) -> Self {
            Self {
                requests: Mutex::new(0),
                arguments,
            }
        }
    }

    impl crate::LlmStream for LoadSkillProvider {
        fn name(&self) -> &'static str {
            "load_skill_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("lock requests");
            let request_index = *requests;
            *requests += 1;
            drop(requests);
            let arguments = self.arguments.clone();

            Box::pin(async move {
                let events = if request_index < 2 {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("load_{request_index}"),
                        "load_skill",
                        arguments,
                    )))]
                } else {
                    vec![Ok(CompletionEvent::Finished)]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct DuplicateSkillProvider;

    impl crate::LlmStream for DuplicateSkillProvider {
        fn name(&self) -> &'static str {
            "duplicate_skill_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = match request.tool_results().count() {
                    0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "read_once",
                        "read",
                        json!({ "filePath": "packages/app/src/main.rs" }),
                    )))],
                    1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "load_once",
                        "load_skill",
                        json!({ "name": "example" }),
                    )))],
                    _ => vec![Ok(CompletionEvent::Finished)],
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct RootedSkillProvider {
        root: PathBuf,
    }

    impl crate::LlmStream for RootedSkillProvider {
        fn name(&self) -> &'static str {
            "rooted_skill_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let root = self.root.display().to_string();
            Box::pin(async move {
                let events = match request.tool_results().count() {
                    0 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "read_once",
                        "read",
                        json!({ "filePath": "packages/app/src/main.rs" }),
                    )))],
                    1 => vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "load_once",
                        "load_skill",
                        json!({ "name": "example", "root": root }),
                    )))],
                    _ => vec![Ok(CompletionEvent::Finished)],
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[derive(Default)]
    struct RepeatedGlobProvider {
        requests: Mutex<usize>,
    }

    impl crate::LlmStream for RepeatedGlobProvider {
        fn name(&self) -> &'static str {
            "repeated_glob_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("lock requests");
            let request_index = *requests;
            *requests += 1;
            drop(requests);

            Box::pin(async move {
                let events = if request_index < 2 {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("glob_{request_index}"),
                        "glob",
                        json!({ "pattern": "**/*.rs" }),
                    )))]
                } else {
                    vec![Ok(CompletionEvent::Finished)]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[derive(Default)]
    struct RepeatedReadProvider {
        requests: Mutex<usize>,
    }

    impl crate::LlmStream for RepeatedReadProvider {
        fn name(&self) -> &'static str {
            "repeated_read_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let mut requests = self.requests.lock().expect("lock requests");
            let request_index = *requests;
            *requests += 1;
            drop(requests);

            Box::pin(async move {
                let events = if request_index < 2 {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        format!("read_{request_index}"),
                        "read",
                        json!({ "filePath": "src/main.rs" }),
                    )))]
                } else {
                    vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    struct SingleReadProvider;

    impl crate::LlmStream for SingleReadProvider {
        fn name(&self) -> &'static str {
            "single_read_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let is_continuation = request.tool_results().next().is_some();
            Box::pin(async move {
                let events = if is_continuation {
                    vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                } else {
                    vec![Ok(CompletionEvent::ToolCall(ToolCall::new(
                        "read_once",
                        "read",
                        json!({ "filePath": "src/main.rs" }),
                    )))]
                };
                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn agent_system_prompt_fragments_are_sent_in_order() {
        let provider = SystemPromptProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model)
            .with_base_system_prompt()
            .with_system_date("2026-06-02")
            .with_filesystem_system_prompt(FilesystemPromptConfig::new("/tmp/waltr-root"))
            .with_system_prompt("custom fragment")
            .build();

        let mut events = agent
            .prompt(&provider, "hello")
            .with_system_prompt_fragment("run fragment")
            .stream();
        let mut text = String::new();
        while let Some(event) = events.next().await {
            if let AgentEvent::TextDelta { delta, .. } = event.expect("agent event") {
                text.push_str(&delta);
            }
        }

        assert_eq!(text, "ok");
    }

    struct SystemPromptProvider;

    impl crate::LlmStream for SystemPromptProvider {
        fn name(&self) -> &'static str {
            "system_prompt_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let system_prompt = request.system_prompt.expect("system prompt");
                assert!(system_prompt.contains(crate::BASE_SYSTEM_PROMPT));
                assert!(system_prompt.contains("Current date: 2026-06-02."));
                assert!(system_prompt.contains("Filesystem tools are available"));
                assert!(system_prompt.contains("custom fragment"));
                assert!(system_prompt.ends_with("run fragment"));
                assert!(
                    system_prompt.find(crate::BASE_SYSTEM_PROMPT)
                        < system_prompt.find("Current date: 2026-06-02.")
                );
                assert!(
                    system_prompt.find("Current date: 2026-06-02.")
                        < system_prompt.find("Filesystem tools are available")
                );
                assert!(
                    system_prompt.find("Filesystem tools are available")
                        < system_prompt.find("custom fragment")
                );
                assert!(system_prompt.find("custom fragment") < system_prompt.find("run fragment"));

                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::Delta("ok".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn tool_errors_are_reported_to_agent_for_recovery() {
        let provider = RecoveringProvider;
        let tool = AgentTool::new(
            ToolDefinition::new(
                "fail_once",
                "Always fails.",
                json!({ "type": "object", "additionalProperties": false }),
            ),
            |_call| {
                Err(LlmError::Protocol {
                    provider: "test_tool",
                    message: "validation failed".to_owned(),
                })
            },
        );
        let agent = Agent::builder(ModelRef::new(
            ProviderId::OpenAi,
            "test-model",
            ModelApi::OpenAiResponses,
        ))
        .with_tool(tool)
        .build();

        let mut events = agent.prompt(&provider, "recover").stream();
        let mut saw_error_result = false;
        let mut final_text = String::new();

        while let Some(event) = events.next().await {
            match event.expect("agent event") {
                AgentEvent::ToolResult { result, .. } => {
                    saw_error_result = result.content.contains("Tool error:")
                        && result.content.contains("validation failed");
                }
                AgentEvent::TextDelta { delta, .. } => final_text.push_str(&delta),
                AgentEvent::ThinkingDelta { .. }
                | AgentEvent::ExternalRequest { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::TurnFinished { .. }
                | AgentEvent::Finished { .. } => {}
            }
        }

        assert!(saw_error_result);
        assert_eq!(final_text, "recovered");
    }

    #[tokio::test]
    async fn agent_surfaces_and_stores_thinking_without_final_text_pollution() {
        let provider = ThinkingProvider;
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = Agent::builder(model).build();

        let mut events = agent.prompt(&provider, "think").stream();
        let mut saw_thinking_delta = false;
        let mut final_snapshot = None;

        while let Some(event) = events.next().await {
            match event.expect("agent event") {
                AgentEvent::ThinkingDelta { delta, .. } => {
                    saw_thinking_delta = delta == "hidden thought";
                }
                AgentEvent::Finished { snapshot } => final_snapshot = Some(snapshot),
                AgentEvent::TextDelta { .. }
                | AgentEvent::ExternalRequest { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::ToolResult { .. }
                | AgentEvent::TurnFinished { .. } => {}
            }
        }

        let snapshot = final_snapshot.expect("final snapshot");
        assert!(saw_thinking_delta);
        assert_eq!(snapshot.status, SessionStatus::Finished);
        assert_eq!(snapshot.state.last_assistant_text(), "final answer");
        assert!(matches!(
            snapshot.state.events[0],
            super::AgentTranscriptEvent::Thinking { ref text, .. } if text == "hidden thought"
        ));
    }

    struct ThinkingProvider;

    impl crate::LlmStream for ThinkingProvider {
        fn name(&self) -> &'static str {
            "thinking_provider"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![
                    Ok(CompletionEvent::ThinkingDelta(ThinkingDelta {
                        text: "hidden thought".to_owned(),
                    })),
                    Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                        kind: ThinkingBlockKind::OpenAiResponses,
                        text: "hidden thought".to_owned(),
                        payload: json!({ "type": "reasoning", "id": "rs_1" }),
                    })),
                    Ok(CompletionEvent::Delta("final answer".to_owned())),
                    Ok(CompletionEvent::Finished),
                ])) as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn agent_replays_thinking_blocks_on_tool_continuation() {
        let provider = ReplayThinkingProvider::default();
        let tool = AgentTool::new(
            ToolDefinition::new(
                "lookup",
                "Look up test data.",
                json!({ "type": "object", "additionalProperties": false }),
            ),
            |_call| Ok("tool output".to_owned()),
        );
        let agent = Agent::builder(ModelRef::new(
            ProviderId::OpenAi,
            "test-model",
            ModelApi::OpenAiResponses,
        ))
        .with_tool(tool)
        .build();

        let mut events = agent.prompt(&provider, "use tool").stream();
        while events.next().await.is_some() {}

        assert!(provider.saw_replayed_thinking());
    }

    #[derive(Default)]
    struct ReplayThinkingProvider {
        calls: Mutex<usize>,
        saw_replayed_thinking: Mutex<bool>,
    }

    impl ReplayThinkingProvider {
        fn saw_replayed_thinking(&self) -> bool {
            *self.saw_replayed_thinking.lock().expect("lock")
        }
    }

    impl crate::LlmStream for ReplayThinkingProvider {
        fn name(&self) -> &'static str {
            "replay_thinking_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let mut calls = self.calls.lock().expect("lock");
                *calls += 1;
                if *calls == 1 {
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::ThinkingBlock(ThinkingBlock {
                            kind: ThinkingBlockKind::OpenAiResponses,
                            text: "reasoning".to_owned(),
                            payload: json!({ "type": "reasoning", "id": "rs_1" }),
                        })),
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "call_1",
                            "lookup",
                            json!({}),
                        ))),
                    ])) as BoxCompletionStream)
                } else {
                    *self.saw_replayed_thinking.lock().expect("lock") =
                        request.thinking_blocks().count() == 1;
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::Delta("done".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                }
            })
        }
    }

    struct RecoveringProvider;

    impl crate::LlmStream for RecoveringProvider {
        fn name(&self) -> &'static str {
            "recovering_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                let events = if request.tool_results().next().is_none() {
                    vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "call_1",
                            "fail_once",
                            json!({}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ]
                } else {
                    assert!(
                        request
                            .tool_results()
                            .next()
                            .expect("tool result")
                            .content
                            .contains("validation failed")
                    );
                    vec![
                        Ok(CompletionEvent::Delta("recovered".to_owned())),
                        Ok(CompletionEvent::Finished),
                    ]
                };

                Ok(Box::pin(stream::iter(events)) as BoxCompletionStream)
            })
        }
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

        fn write_skill(&self, name: &str, description: &str, body: &str) {
            self.write(
                &format!(".agents/skills/{name}/SKILL.md"),
                &format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
            );
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}
