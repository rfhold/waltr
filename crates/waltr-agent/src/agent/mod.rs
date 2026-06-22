//! `AgentRuntime` configuration and loop execution.

use std::{fmt, sync::Arc};

use crate::{LlmStream, ModelRef, PermissionPolicy, middleware::chain::MiddlewareChain};

mod builder;
mod event;
mod r#loop;
mod run;
pub mod session;
mod tool;

pub use builder::AgentRuntimeBuilder;
pub use event::BoxAgentTimelineStream;
pub use run::AgentRunBuilder;
pub use session::{
    AgentSession, AgentSessionRuntime, AgentSessions, CancelReason, ExternalRequest,
    ExternalRequestId, ExternalRequestKind, ExternalRequestStatus, ExternalResponse,
    FilesystemSessionStore, MemorySessionStore, OpenDalSessionStore, PendingWork,
    PendingWorkStatus, RequestId, RequestRecord, ResumeInput, Revision, RuntimeResumeOutput,
    S3SessionStore, ScyllaSessionStore, SessionId, SessionJournalEvent, SessionListEvent,
    SessionListSubscription, SessionListUpdate, SessionRecord, SessionSnapshot, SessionStatus,
    SessionStore, SessionStoreError, SessionStoreResult, SessionSummary,
    SessionTimelineSubscription, SessionTimelineUpdate, Suspension,
    opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use tool::{AgentTool, ToolExecutionContext, ToolProgressSink};

/// Executable agent runtime configuration and enabled capabilities.
#[derive(Clone)]
pub struct AgentRuntime {
    pub(super) provider: Arc<dyn LlmStream>,
    pub(super) model: ModelRef,
    pub(super) tools: Vec<AgentTool>,
    pub(super) middleware_chain: MiddlewareChain,
    pub(super) permission_policy: PermissionPolicy,
}

impl AgentRuntime {
    /// Start building an agent runtime for a model.
    #[must_use]
    pub fn builder<P>(model: ModelRef, provider: Arc<P>) -> AgentRuntimeBuilder
    where
        P: LlmStream + 'static,
    {
        AgentRuntimeBuilder::new(model, provider)
    }

    /// Return the model identity configured for this runtime.
    #[must_use]
    pub const fn model(&self) -> &ModelRef {
        &self.model
    }

    /// Start building an agent loop request.
    pub fn prompt(&self, prompt: impl Into<String>) -> AgentRunBuilder {
        AgentRunBuilder::new(self, prompt.into())
    }

    /// Return configured host tools.
    #[must_use]
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }
}

impl fmt::Debug for AgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntime")
            .field("provider", &self.provider.name())
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("middleware_chain_len", &self.middleware_chain.len())
            .field("permission_policy", &self.permission_policy)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        AgentTimelineEvent, BoxCompletionStream, BoxStreamFuture, CHANNEL_ASSISTANT_TEXT,
        CHANNEL_USER_TEXT, CompletionEvent, CompletionRequest, ExternalRequestId, ExternalResponse,
        MemorySessionStore, ModelApi, ModelRef, ProviderId, ResumeInput, SessionJournalEvent,
        SessionStore, TimelineContent, TimelineEventKind, TimelineQuery, ToolCall,
        middleware::{
            AgentMiddleware, HookOutcome,
            chain::MiddlewareChain,
            context::{MiddlewareStateAccess, ModelRequestDraft, RequestContext},
        },
        tools::{
            filesystem::FilesystemConfig,
            question::{QUESTION_TOOL_NAME, QuestionConfig},
        },
    };
    use futures_util::{StreamExt, stream};
    use std::sync::{Arc, Mutex};

    use super::{AgentRuntime, AgentSessions};

    struct NoopMiddleware;

    impl AgentMiddleware for NoopMiddleware {}

    #[test]
    fn builder_assembles_system_prompt_middleware_by_default() {
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);

        let agent = AgentRuntime::builder(model, Arc::new(TextOnlyProvider)).build();

        assert_eq!(agent.middleware_chain.len(), 1);
    }

    #[test]
    fn builder_accepts_middleware_chain() {
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let mut chain = MiddlewareChain::new();
        chain.push(NoopMiddleware);

        let agent = AgentRuntime::builder(model.clone(), Arc::new(TextOnlyProvider))
            .with_middleware(NoopMiddleware)
            .build();
        let chain_agent = AgentRuntime::builder(model, Arc::new(TextOnlyProvider))
            .with_middleware_chain(chain)
            .build();

        assert_eq!(agent.middleware_chain.len(), 2);
        assert_eq!(chain_agent.middleware_chain.len(), 2);
    }

    #[test]
    fn filesystem_tools_are_opt_in() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);

        assert!(
            AgentRuntime::builder(model.clone(), Arc::new(TextOnlyProvider))
                .build()
                .tools()
                .is_empty()
        );
        assert!(
            AgentRuntime::builder(model, Arc::new(TextOnlyProvider))
                .with_filesystem_tools(FilesystemConfig::new("."))
                .build()
                .tools()
                .iter()
                .any(|tool| tool.definition().name == "apply_patch")
        );
    }

    #[derive(Default)]
    struct DurableResumeProvider {
        requests: Mutex<usize>,
        saw_resumed_tool_result: Mutex<bool>,
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
            let current = ctx.state_as::<serde_json::Value>(self.namespace)?;
            self.observed
                .lock()
                .expect("observed state lock")
                .push(current.clone());
            if let Some(state) = &self.write
                && current.is_none()
            {
                ctx.set_state_as(self.namespace, state)?;
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
    async fn durable_session_publishes_user_prompt_and_final_assistant_snapshot() {
        let provider = Arc::new(TextOnlyProvider);
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, provider).build();
        let sessions = AgentSessions::new(MemorySessionStore::new());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();
        let mut watch = sessions.watch_timeline(session_id.clone());

        drain(
            sessions
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;

        let mut watched = Vec::new();
        while watched.len() < 4 {
            let update = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
                .await
                .expect("timeline update timeout")
                .expect("timeline update");
            watched.push(update.event);
        }

        assert!(watched.iter().any(|event| {
            event.channel.as_ref().map(|channel| channel.as_str()) == Some(CHANNEL_USER_TEXT)
                && event.kind == TimelineEventKind::ItemFinished
                && matches!(&event.content, TimelineContent::TextSnapshot { text } if text == "hello")
        }));
        assert!(watched.iter().any(|event| {
            event.channel.as_ref().map(|channel| channel.as_str()) == Some(CHANNEL_ASSISTANT_TEXT)
                && event.kind == TimelineEventKind::ItemFinished
                && matches!(&event.content, TimelineContent::TextSnapshot { text } if text == "ok")
        }));

        let timeline = sessions
            .timeline(&session_id, TimelineQuery::forward(None, 16))
            .await
            .expect("timeline read");
        assert_eq!(timeline, watched);
    }

    #[tokio::test]
    async fn middleware_state_write_is_journaled_before_provider_request_record() {
        let provider = Arc::new(TextOnlyProvider);
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let agent = AgentRuntime::builder(model, provider)
            .with_middleware(StateRecordingMiddleware {
                namespace: "test-state",
                write: Some(json!({ "writes": 1 })),
                observed: Arc::clone(&observed),
            })
            .build();
        let store = MemorySessionStore::new();
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        drain(
            sessions
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;

        let record = store.load(&session_id).await.expect("stored session");
        let state_index = record
            .journal
            .iter()
            .position(|event| {
                matches!(
                    event,
                    SessionJournalEvent::MiddlewareStateUpdated { namespace, .. }
                        if namespace == "test-state"
                )
            })
            .expect("state event journaled");
        let request_index = record
            .journal
            .iter()
            .position(|event| matches!(event, SessionJournalEvent::ProviderRequestRecorded { .. }))
            .expect("request record journaled");
        assert!(
            state_index < request_index,
            "state event must precede the provider request record it influenced"
        );
        let snapshot = record.snapshot();
        assert_eq!(
            snapshot.middleware_state.get("test-state"),
            Some(&json!({ "writes": 1 }))
        );
        assert_eq!(*observed.lock().expect("observed state lock"), vec![None]);
    }

    #[tokio::test]
    async fn middleware_state_written_before_suspension_is_observable_after_resume() {
        let store = MemorySessionStore::new();
        let provider = Arc::new(DurableResumeProvider::default());
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let written = json!({ "grants": ["shell"] });

        let writer_observed = Arc::new(Mutex::new(Vec::new()));
        let writer_agent = AgentRuntime::builder(model.clone(), Arc::clone(&provider))
            .with_question_tool(QuestionConfig::new())
            .with_middleware(StateRecordingMiddleware {
                namespace: "permissions",
                write: Some(written.clone()),
                observed: Arc::clone(&writer_observed),
            })
            .build();
        let sessions = AgentSessions::new(store.clone());
        let session_id = sessions
            .create()
            .await
            .expect("created session")
            .id()
            .clone();

        let mut events = sessions
            .prompt(&writer_agent, session_id.clone(), "ask then continue")
            .stream();
        let mut request_id = None;
        while let Some(event) = events.next().await {
            if let TimelineContent::ExternalRequest { request } =
                event.expect("agent event").content
            {
                request_id = Some(request.id.as_str().to_owned());
            }
        }
        drop(events);
        let request_id = request_id.expect("question request");
        assert_eq!(
            *writer_observed.lock().expect("observed state lock"),
            vec![None]
        );

        let reader_observed = Arc::new(Mutex::new(Vec::new()));
        let reader_agent = AgentRuntime::builder(model, Arc::clone(&provider))
            .with_question_tool(QuestionConfig::new())
            .with_middleware(StateRecordingMiddleware {
                namespace: "permissions",
                write: None,
                observed: Arc::clone(&reader_observed),
            })
            .build();
        drain(
            AgentSessions::new(store.clone())
                .resume(
                    &reader_agent,
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

    async fn drain<S>(mut stream: S)
    where
        S: futures_util::Stream<Item = crate::Result<AgentTimelineEvent>> + Unpin,
    {
        while let Some(event) = stream.next().await {
            event.expect("agent event");
        }
    }
}
