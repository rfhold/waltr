//! `AgentRuntime` configuration and loop execution.

use std::{fmt, sync::Arc};

use crate::{LlmStream, ModelCost, ModelRef, PermissionPolicy, middleware::chain::MiddlewareChain};

mod builder;
mod event;
mod r#loop;
mod retry;
mod run;
pub mod session;
mod tool;

pub use builder::AgentRuntimeBuilder;
pub use event::{
    BoxAgentTimelineStream, SessionCompletionAdmissionFinalizer, SessionCompletionAdmissionHandler,
    SessionCompletionFinalizationFuture, SessionCompletionPreparation,
    SessionCompletionPreparationFuture, SessionCompletionWorkerFuture, SessionEvent,
    SessionEventSink, SessionEventSinkFuture, SessionRunCompletionSnapshot,
};
pub use retry::ProviderRetryConfig;
pub use run::AgentRunBuilder;
pub use session::{
    ActiveSessionMask, AgentRunLimits, AgentRuntimeProvider, AgentSession,
    AgentSessionActiveRunState, AgentSessionRunDisposition, AgentSessionRunError,
    AgentSessionRunHandle, AgentSessionRunInput, AgentSessionRunOutcome, AgentSessionRunSubmission,
    AgentSessionRuntime, AgentSessionRuntimeError, AgentSessionRuntimeFactory, CancelReason,
    CompletedSessionCapture, CredentialRefreshCoordinator, CredentialRefreshError, CredentialStore,
    ExternalRequest, ExternalRequestId, ExternalRequestKind, ExternalRequestStatus,
    ExternalResponse, FileCredentialStore, FilesystemSessionStore, InferenceProviderSettings,
    InferenceSettings, MAX_ACTIVE_SESSION_MASKS, MAX_SESSION_COORDINATION_REASON_ATTRIBUTE_BYTES,
    MAX_SESSION_COORDINATION_REASON_CODE_BYTES, MAX_SESSION_MASK_KEY_BYTES,
    MAX_SESSION_MASK_MUTATION_ITEMS, MAX_SESSION_MASK_REPLACEMENT_BYTES, McpAppError, McpAppHandle,
    McpConnectionKind, McpConnectionResolveFuture, McpConnectionResolveRequest, McpOAuthContext,
    McpOAuthEndpoints, McpOAuthRegistration, McpOAuthRegistrationMethod, McpResolvedConnection,
    MemorySessionStore, MetadataSetCondition, MetadataSetOutcome, OAUTH_CREDENTIAL_VERSION,
    OAuthCredential, OAuthCredentialContext, OAuthCredentialRecord, OAuthCredentialStore,
    OAuthCredentialStoreFuture, OAuthRefreshClaim, OAuthRefreshClaimResult, OAuthTokenSet,
    OpenDalSessionStore, PendingWork, PendingWorkStatus, RefreshMode, ReplacementUserContent,
    RequestId, RequestRecord, ResumeInput, Revision, RunConflictPolicy, RuntimeResumeOutput,
    S3SessionStore, S3SessionWriteMode, SandboxDispatch, ScyllaSessionStore, SessionCapabilities,
    SessionCaptureError, SessionCaptureLimits, SessionCompletionCoordinationFuture,
    SessionCompletionCoordinationHandler, SessionCompletionCoordinationPreparation,
    SessionContextMode, SessionCoordinationPhase, SessionCoordinationReason,
    SessionCoordinationReasonError, SessionCoordinationState, SessionCoordinator, SessionCopy,
    SessionCopyError, SessionCreateRequest, SessionId, SessionJournalEvent, SessionListEvent,
    SessionListSubscription, SessionListUpdate, SessionMask, SessionMaskAddition, SessionMaskError,
    SessionMaskId, SessionMaskMutation, SessionMaskMutationOutcome, SessionMaskSubtraction,
    SessionMaterializedContext, SessionMcpApp, SessionMetrics, SessionOAuthJwtMetadata,
    SessionOAuthJwtToken, SessionOAuthProviderSettings, SessionOAuthTokenField,
    SessionOAuthTokenFieldTarget, SessionOAuthTokenRequestStyle, SessionPauseError, SessionPauseId,
    SessionPauseLease, SessionProgress, SessionProgressPhase, SessionPromptDisposition,
    SessionRecord, SessionRunStage, SessionSnapshot, SessionStart, SessionStartMessage,
    SessionStartMessageError, SessionStatus, SessionStore, SessionStoreError, SessionStoreResult,
    SessionSummary, SessionSyntheticTimelineEvent, SessionTimelineEvent, SessionTimelineMode,
    SessionTimelinePage, SessionTimelinePresentation, SessionTimelineProjectedEvent,
    SessionTimelineProjectionEvent, SessionTimelineProjectionSubscription,
    SessionTimelineProjectionUpdate, SessionTimelineProjectionUpdateEvent,
    SessionTimelineSubscription, SessionTimelineUpdate, StoredCredential, Suspension,
    ThroughCursor, opendal_postgresql_session_store, run_postgresql_migrations,
};
pub use tool::{AgentTool, ToolExecutionContext, ToolProgressSink};

/// Executable agent runtime configuration and enabled capabilities.
#[derive(Clone)]
pub struct AgentRuntime {
    pub(super) provider: Arc<dyn LlmStream>,
    pub(super) model: ModelRef,
    pub(super) model_cost: Option<ModelCost>,
    pub(super) tools: Vec<AgentTool>,
    pub(super) middleware_chain: MiddlewareChain,
    pub(super) permission_policy: PermissionPolicy,
    pub(super) retry_config: ProviderRetryConfig,
    pub(super) template_base_dir: Option<std::path::PathBuf>,
    pub(super) max_provider_turns: Option<u64>,
    pub(super) sandbox: Option<crate::sandbox_gateway::SandboxHandle>,
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

    /// Return pricing configured for this runtime, when available.
    #[must_use]
    pub const fn model_cost(&self) -> Option<&ModelCost> {
        self.model_cost.as_ref()
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

    pub(crate) const fn middleware_chain(&self) -> &MiddlewareChain {
        &self.middleware_chain
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for AgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntime")
            .field("provider", &self.provider.name())
            .field("model", &self.model)
            .field("model_cost", &self.model_cost)
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
        MemorySessionStore, ModelApi, ModelRef, ProviderId, ResumeInput, Revision,
        SessionCreateRequest, SessionId, SessionJournalEvent, SessionListEvent, SessionRecord,
        SessionStatus, SessionStore, SessionTimelineEvent, TimelineChannel, TimelineContent,
        TimelineEventKind, TimelineQuery, ToolCall,
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::{
        AgentRuntime, SessionCoordinator,
        session::{SessionControl, SessionRunContext, SessionTimelineBroadcaster},
    };

    struct RecordingEventSink {
        store: MemorySessionStore,
        tx: tokio::sync::mpsc::UnboundedSender<(crate::SessionEvent, SessionRecord)>,
        fail: bool,
    }

    #[derive(Clone)]
    struct FailAfterPromptAppendStore {
        inner: MemorySessionStore,
        fail_next_load: Arc<AtomicBool>,
    }

    impl FailAfterPromptAppendStore {
        fn new() -> Self {
            Self {
                inner: MemorySessionStore::new(),
                fail_next_load: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl SessionStore for FailAfterPromptAppendStore {
        fn create(
            &self,
            request: crate::SessionCreateRequest,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<SessionRecord>> + Send
        {
            let inner = self.inner.clone();
            let fail = Arc::clone(&self.fail_next_load);
            async move {
                let has_prompt = match &request.start {
                    crate::SessionStart::Fresh { messages }
                    | crate::SessionStart::Copy { messages, .. } => !messages.is_empty(),
                };
                let record = inner.create(request).await?;
                if has_prompt {
                    fail.store(true, Ordering::SeqCst);
                }
                Ok(record)
            }
        }

        fn load(
            &self,
            id: &crate::SessionId,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<SessionRecord>> + Send
        {
            let inner = self.inner.clone();
            let id = id.clone();
            let fail = Arc::clone(&self.fail_next_load);
            async move {
                if fail.swap(false, Ordering::SeqCst) {
                    Err(crate::SessionStoreError::Backend {
                        message: "injected post-append load failure".to_owned(),
                    })
                } else {
                    inner.load(&id).await
                }
            }
        }

        fn delete(
            &self,
            id: &crate::SessionId,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<()>> + Send {
            self.inner.delete(id)
        }

        fn wipe(&self) -> impl std::future::Future<Output = crate::SessionStoreResult<()>> + Send {
            self.inner.wipe()
        }

        fn list(
            &self,
        ) -> impl std::future::Future<
            Output = crate::SessionStoreResult<Vec<crate::SessionSummary>>,
        > + Send {
            self.inner.list()
        }

        fn append_timeline(
            &self,
            id: &crate::SessionId,
            expected_revision: crate::Revision,
            events: Vec<crate::PendingTimelineEvent>,
        ) -> impl std::future::Future<
            Output = crate::SessionStoreResult<Vec<crate::AgentTimelineEvent>>,
        > + Send {
            self.inner.append_timeline(id, expected_revision, events)
        }

        fn append_journal(
            &self,
            id: &crate::SessionId,
            expected_revision: crate::Revision,
            events: Vec<crate::SessionJournalEvent>,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<crate::Revision>> + Send
        {
            let inner = self.inner.clone();
            let id = id.clone();
            let fail = Arc::clone(&self.fail_next_load);
            async move {
                let prompt_appended = events.iter().any(|event| {
                    matches!(event, crate::SessionJournalEvent::PromptAppended { .. })
                });
                let revision = inner.append_journal(&id, expected_revision, events).await?;
                if prompt_appended {
                    fail.store(true, Ordering::SeqCst);
                }
                Ok(revision)
            }
        }

        fn mutate_masks(
            &self,
            id: &crate::SessionId,
            mutation: crate::SessionMaskMutation,
        ) -> impl std::future::Future<
            Output = crate::SessionStoreResult<crate::SessionMaskMutationOutcome>,
        > + Send {
            self.inner.mutate_masks(id, mutation)
        }

        fn set_metadata_value_conditionally(
            &self,
            id: &crate::SessionId,
            key: String,
            value: serde_json::Value,
            condition: crate::MetadataSetCondition,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<crate::MetadataSetOutcome>> + Send
        {
            self.inner
                .set_metadata_value_conditionally(id, key, value, condition)
        }

        fn remove_metadata_value(
            &self,
            id: &crate::SessionId,
            key: String,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<SessionRecord>> + Send
        {
            self.inner.remove_metadata_value(id, key)
        }

        fn set_inference_settings(
            &self,
            id: &crate::SessionId,
            inference_settings: Option<crate::InferenceSettings>,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<SessionRecord>> + Send
        {
            self.inner.set_inference_settings(id, inference_settings)
        }

        fn set_agent_definition(
            &self,
            id: &crate::SessionId,
            agent_definition: Option<crate::AgentDefinitionBundle>,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<SessionRecord>> + Send
        {
            self.inner.set_agent_definition(id, agent_definition)
        }

        fn timeline(
            &self,
            id: &crate::SessionId,
            query: crate::TimelineQuery,
        ) -> impl std::future::Future<
            Output = crate::SessionStoreResult<Vec<crate::AgentTimelineEvent>>,
        > + Send {
            self.inner.timeline(id, query)
        }

        fn timeline_item_text(
            &self,
            id: &crate::SessionId,
            item_id: &crate::TimelineItemId,
        ) -> impl std::future::Future<Output = crate::SessionStoreResult<crate::TimelineItemText>> + Send
        {
            self.inner.timeline_item_text(id, item_id)
        }
    }

    struct EventOnlySink(tokio::sync::mpsc::UnboundedSender<crate::SessionEvent>);

    impl crate::SessionEventSink for EventOnlySink {
        fn send(&self, event: crate::SessionEvent) -> crate::SessionEventSinkFuture {
            let tx = self.0.clone();
            Box::pin(async move { tx.send(event).map_err(|error| error.to_string()) })
        }
    }

    impl crate::SessionEventSink for RecordingEventSink {
        fn send(&self, event: crate::SessionEvent) -> crate::SessionEventSinkFuture {
            let store = self.store.clone();
            let tx = self.tx.clone();
            let fail = self.fail;
            Box::pin(async move {
                let crate::SessionEvent::UserMessageCreated { session_id, .. } = &event;
                let record = store
                    .load(session_id)
                    .await
                    .map_err(|error| error.to_string())?;
                tx.send((event, record))
                    .map_err(|error| error.to_string())?;
                if fail {
                    Err("injected sink failure".to_owned())
                } else {
                    Ok(())
                }
            })
        }
    }

    struct NoopMiddleware;

    impl AgentMiddleware for NoopMiddleware {}

    #[test]
    fn builder_assembles_system_prompt_middleware_by_default() {
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);

        let agent = AgentRuntime::builder(model, Arc::new(TextOnlyProvider)).build();

        assert_eq!(agent.middleware_chain.len(), 3);
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

        assert_eq!(agent.middleware_chain.len(), 4);
        assert_eq!(chain_agent.middleware_chain.len(), 4);
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
                .with_sandbox(crate::sandbox_gateway::SandboxGateway::new("test".into()).handle())
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

    struct EffectiveContextProvider(AtomicBool);

    impl crate::LlmStream for EffectiveContextProvider {
        fn name(&self) -> &'static str {
            "effective_context_provider"
        }

        fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            let prompts = request
                .replay_items()
                .into_iter()
                .filter_map(|item| match item {
                    crate::CompletionReplayItem::Prompt(prompt) => Some(prompt),
                    crate::CompletionReplayItem::Turn(_) => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(prompts, ["compact summary"]);
            assert!(request.turns.is_empty());
            self.0.store(true, Ordering::SeqCst);
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream)
            })
        }
    }

    #[tokio::test]
    async fn automatic_creation_publishes_initial_list_and_timeline() {
        let provider = Arc::new(TextOnlyProvider);
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, provider).build();
        let store = MemorySessionStore::new();
        let session_id = SessionId::new("automatic-publication");
        let control = SessionControl::new();
        let timeline = SessionTimelineBroadcaster::new();
        let mut session_updates = control.subscribe();
        let mut timeline_updates = timeline.subscribe(session_id.clone());
        let context = SessionRunContext::new(control.session(session_id.clone()), timeline, None);
        let mut stream = agent
            .prompt("hello")
            .with_session_store(store, session_id.clone())
            .with_session_run_context(context)
            .stream();

        let output = stream
            .next()
            .await
            .expect("initial run output")
            .expect("successful initial run output");
        let session_update = session_updates.next().await.expect("session list update");
        let timeline_update = timeline_updates.next().await.expect("timeline update");

        assert_eq!(session_update.sequence, 1);
        assert!(matches!(
            session_update.event,
            SessionListEvent::Upsert(summary)
                if summary.id == session_id && summary.revision == Revision::ZERO
        ));
        assert_eq!(timeline_update.sequence, 1);
        assert_eq!(timeline_update.event, SessionTimelineEvent::Event(output));
    }

    #[tokio::test]
    async fn ordinary_resume_hydrates_provider_from_effective_context() {
        let store = MemorySessionStore::new();
        let id = SessionId::new("effective-hydration");
        store
            .create(SessionCreateRequest::new(id.clone()))
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                Revision::ZERO,
                vec![SessionJournalEvent::PromptAppended {
                    prompt: "original question".to_owned(),
                }],
            )
            .await
            .unwrap();
        store
            .append_timeline(
                &id,
                Revision::new(1),
                vec![
                    crate::PendingTimelineEvent::user_text_snapshot(
                        id.clone(),
                        0,
                        crate::PendingTimelineItemId::New,
                        "original question",
                    ),
                    crate::PendingTimelineEvent::assistant_text_snapshot(
                        id.clone(),
                        0,
                        crate::PendingTimelineItemId::New,
                        "original answer",
                    ),
                ],
            )
            .await
            .unwrap();
        store
            .append_journal(
                &id,
                Revision::new(2),
                vec![SessionJournalEvent::TurnFinished { turn: 0 }],
            )
            .await
            .unwrap();
        let mut mutation = crate::SessionMaskMutation::new(Some(crate::TimelineCursor::new(2)));
        mutation.add.push(crate::SessionMaskAddition::new(
            "context.compaction-prefix",
            crate::ThroughCursor::new(crate::TimelineCursor::new(2)),
            crate::ReplacementUserContent::new("compact summary"),
        ));
        store.mutate_masks(&id, mutation).await.unwrap();

        let provider = Arc::new(EffectiveContextProvider(AtomicBool::new(false)));
        let agent = AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
            Arc::clone(&provider),
        )
        .build();
        drain(
            SessionCoordinator::new(store)
                .resume(&agent, id, ResumeInput::Continue)
                .stream(),
        )
        .await;

        assert!(provider.0.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn durable_session_publishes_user_prompt_and_final_assistant_snapshot() {
        let provider = Arc::new(TextOnlyProvider);
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, provider).build();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let session_id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
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
            if let SessionTimelineEvent::Event(event) = update.event {
                watched.push(event);
            }
        }

        assert!(watched.iter().any(|event| {
            event.channel.as_ref().map(TimelineChannel::as_str) == Some(CHANNEL_USER_TEXT)
                && event.kind == TimelineEventKind::ItemFinished
                && matches!(&event.content, TimelineContent::TextSnapshot { text } if text == "hello")
        }));
        assert!(watched.iter().any(|event| {
            event.channel.as_ref().map(TimelineChannel::as_str) == Some(CHANNEL_ASSISTANT_TEXT)
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
    async fn user_message_event_is_post_durability_and_sink_failure_is_non_fatal() {
        let store = MemorySessionStore::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = Arc::new(RecordingEventSink {
            store: store.clone(),
            tx,
            fail: true,
        });
        let sessions = SessionCoordinator::new(store.clone()).with_session_event_sink(sink);
        let session_id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap()
            .id()
            .clone();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, Arc::new(TextOnlyProvider)).build();

        drain(
            sessions
                .prompt(&agent, session_id.clone(), "hello")
                .stream(),
        )
        .await;

        let (event, loaded_when_delivered) =
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("semantic event timeout")
                .expect("semantic event");
        let crate::SessionEvent::UserMessageCreated {
            id,
            prompt_index,
            user_message_count,
            text,
            metadata,
            revision,
            ..
        } = event;
        assert_eq!(id, format!("user-message:{}:j0", session_id.as_str()));
        assert_eq!(prompt_index, 0);
        assert_eq!(user_message_count, 1);
        assert_eq!(text, "hello");
        assert!(metadata.is_empty());
        assert!(loaded_when_delivered.revision >= revision);
        assert_eq!(loaded_when_delivered.snapshot().prompts, ["hello"]);
        assert_eq!(
            store.load(&session_id).await.unwrap().snapshot().status,
            SessionStatus::Finished
        );
    }

    #[tokio::test]
    async fn auto_create_does_not_read_after_prompt_append_to_prepare_event() {
        let store = FailAfterPromptAppendStore::new();
        let session_id = crate::SessionId::new("auto-create-load-failure");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, Arc::new(TextOnlyProvider)).build();

        drain(
            agent
                .prompt("hello")
                .with_session_store(store.clone(), session_id.clone())
                .with_session_event_sink(Arc::new(EventOnlySink(tx)))
                .stream(),
        )
        .await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("event timeout")
            .expect("user message event");
        assert!(matches!(
            event,
            crate::SessionEvent::UserMessageCreated { text, .. } if text == "hello"
        ));
        assert!(matches!(
            store.load(&session_id).await,
            Err(crate::SessionStoreError::Backend { message })
                if message == "injected post-append load failure"
        ));
        assert_eq!(
            store
                .inner
                .load(&session_id)
                .await
                .unwrap()
                .snapshot()
                .status,
            SessionStatus::Finished
        );
    }

    #[tokio::test]
    async fn max_provider_turns_fails_before_the_next_logical_dispatch() {
        struct ToolThenPanicProvider {
            requests: AtomicUsize,
        }
        impl crate::LlmStream for ToolThenPanicProvider {
            fn name(&self) -> &'static str {
                "tool_then_panic"
            }

            fn stream_request<'a>(
                &'a self,
                _request: CompletionRequest<'a>,
            ) -> BoxStreamFuture<'a> {
                let request = self.requests.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    request, 0,
                    "provider must not receive a second logical turn"
                );
                Box::pin(async move {
                    Ok(Box::pin(stream::iter(vec![
                        Ok(CompletionEvent::ToolCall(ToolCall::new(
                            "call-1",
                            "missing",
                            json!({}),
                        ))),
                        Ok(CompletionEvent::Finished),
                    ])) as BoxCompletionStream)
                })
            }
        }

        let provider = Arc::new(ToolThenPanicProvider {
            requests: AtomicUsize::new(0),
        });
        let model = ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses);
        let agent = AgentRuntime::builder(model, Arc::clone(&provider))
            .with_max_provider_turns(1)
            .build();
        let store = MemorySessionStore::new();
        let sessions = SessionCoordinator::new(store.clone());
        let session_id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap()
            .id()
            .clone();

        drain(
            sessions
                .prompt(&agent, session_id.clone(), "use a tool")
                .stream(),
        )
        .await;

        assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
        assert!(matches!(
            store.load(&session_id).await.unwrap().snapshot().status,
            SessionStatus::Failed { message, .. }
                if message == "maximum provider turn limit exceeded"
        ));
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
        let sessions = SessionCoordinator::new(store.clone());
        let session_id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
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
        let sessions = SessionCoordinator::new(store.clone());
        let session_id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
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
            SessionCoordinator::new(store.clone())
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
