//! Implementation of the generated `SessionService`.

use std::{collections::BTreeMap, path::PathBuf, pin::Pin, sync::Arc};

use crate::{agent_definition::AgentDefinitionState, config::ConfigHandle};

use super::{
    convert::session_summary_to_proto,
    error::{agent_error_to_status, session_run_error_to_status},
    inference_settings::{resolve_inference_settings, resolve_inference_settings_for_create},
    timeline_convert::{
        catchup_query, channel_matches, initial_query, projected_timeline_event_to_proto,
        query_from_proto, timeline_event_to_proto, timeline_page_to_proto,
    },
};
use waltr::grpc::waltr::{
    config::v1::Configuration,
    session::v1::{
        ContinueSessionRequest, ContinueSessionResponse, CreateSessionRequest,
        CreateSessionResponse, DeleteSessionRequest, DeleteSessionResponse, GetTimelineRequest,
        GetTimelineResponse, InterruptSessionRequest, InterruptSessionResponse, ListAgentsRequest,
        ListAgentsResponse, ListSessionsRequest, ListSessionsResponse,
        RespondExternalRequestRequest, RespondExternalRequestResponse, RollbackSessionRequest,
        RollbackSessionResponse, SearchSessionTemplatesRequest, SearchSessionTemplatesResponse,
        SelectableAgent, SessionListCleared, SessionListSnapshot,
        SessionTemplateKind as ProtoSessionTemplateKind,
        SessionTemplateOption as ProtoSessionTemplateOption, SessionTemplateOptions,
        SessionTemplateSearchError,
        SessionTemplateSearchTarget as ProtoSessionTemplateSearchTarget, SubmitUserMessageRequest,
        SubmitUserMessageResponse, TimelineProjectionMode as ProtoTimelineProjectionMode,
        TimelineProjectionReset, TimelineTruncation, UpdateSessionAgentRequest,
        UpdateSessionAgentResponse, UpdateSessionInferenceSettingsRequest,
        UpdateSessionInferenceSettingsResponse, WatchSessionsRequest, WatchSessionsResponse,
        WatchTimelineRequest, WatchTimelineResponse, WipeSessionsRequest, WipeSessionsResponse,
        rollback_target, search_session_templates_response,
        session_service_server::SessionService as GeneratedSessionService, watch_sessions_response,
        watch_timeline_response,
    },
};
use waltr_agent::{
    AgentDefinitionCatalog, AgentRunLimits, AgentSession, AgentSessionActiveRunState,
    AgentSessionRunError, AgentSessionRunSubmission, AgentSessionRuntimeError,
    AgentSessionRuntimeFactory, CredentialStore, ExternalRequestId, McpConnectionKind,
    McpConnectionResolveRequest, McpResolvedConnection, MetadataSetCondition, MetadataSetOutcome,
    ResolvedAgent, RunConflictPolicy, SessionCoordinator, SessionCreateRequest, SessionId,
    SessionListEvent, SessionListUpdate, SessionStore, SessionSummary, SessionTemplateKind,
    SessionTemplateOption, SessionTemplateSearchRequest, SessionTemplateSearchTarget,
    SessionTimelineEvent, SessionTimelineMode, SessionTimelinePresentation,
    SessionTimelineProjectedEvent, SessionTimelineProjectionEvent,
    SessionTimelineProjectionUpdateEvent, TimelineCursor,
};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_stream::{Stream, StreamExt as _};
use tonic::{Request, Response, Status};
use tracing::Instrument as _;

/// gRPC session service backed by durable agent session storage.
#[derive(Debug)]
pub struct SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    agent_catalog: AgentCatalogSource,
}

#[derive(Debug, Clone)]
enum AgentCatalogSource {
    Static(AgentDefinitionCatalog),
    Dynamic(AgentDefinitionState),
}

impl AgentCatalogSource {
    fn catalog(&self) -> AgentDefinitionCatalog {
        match self {
            Self::Static(catalog) => catalog.clone(),
            Self::Dynamic(state) => state.snapshot().catalog().clone(),
        }
    }
}

impl<S> SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a session service from a store and config handle.
    #[must_use]
    pub fn new_with_config(
        store: S,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origin: Option<reqwest::Url>,
        agent_catalog: AgentDefinitionCatalog,
    ) -> Arc<Self> {
        Self::new_with_oauth_policy(
            store,
            config,
            credential_store,
            trusted_mcp_oauth_origin.into_iter().collect(),
            agent_catalog,
        )
    }

    pub(crate) fn new_with_oauth_policy(
        store: S,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origins: Vec<reqwest::Url>,
        agent_catalog: AgentDefinitionCatalog,
    ) -> Arc<Self> {
        Self::from_sessions_with_catalog_source(
            SessionCoordinator::new(store),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            AgentCatalogSource::Static(agent_catalog),
        )
    }

    /// Create a session service backed by the current successful agent-definition state.
    #[must_use]
    pub fn new_with_config_and_agent_state(
        store: S,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origin: Option<reqwest::Url>,
        agent_state: AgentDefinitionState,
    ) -> Arc<Self> {
        Self::new_with_oauth_policy_and_agent_state(
            store,
            config,
            credential_store,
            trusted_mcp_oauth_origin.into_iter().collect(),
            agent_state,
        )
    }

    pub(crate) fn new_with_oauth_policy_and_agent_state(
        store: S,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origins: Vec<reqwest::Url>,
        agent_state: AgentDefinitionState,
    ) -> Arc<Self> {
        Self::from_sessions_with_catalog_source(
            SessionCoordinator::new(store),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            AgentCatalogSource::Dynamic(agent_state),
        )
    }

    /// Create a session service from an existing session coordinator and config handle.
    #[must_use]
    pub fn from_sessions_with_config(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origin: Option<reqwest::Url>,
        agent_catalog: AgentDefinitionCatalog,
    ) -> Arc<Self> {
        Self::from_sessions_with_catalog_source(
            sessions,
            config,
            credential_store,
            trusted_mcp_oauth_origin.into_iter().collect(),
            AgentCatalogSource::Static(agent_catalog),
        )
    }

    pub(crate) fn from_sessions_with_oauth_policy(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origins: Vec<reqwest::Url>,
        agent_catalog: AgentDefinitionCatalog,
    ) -> Arc<Self> {
        Self::from_sessions_with_catalog_source(
            sessions,
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            AgentCatalogSource::Static(agent_catalog),
        )
    }

    /// Create a session service from an existing coordinator and live agent-definition state.
    #[must_use]
    pub fn from_sessions_with_config_and_agent_state(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origin: Option<reqwest::Url>,
        agent_state: AgentDefinitionState,
    ) -> Arc<Self> {
        Self::from_sessions_with_oauth_policy_and_agent_state(
            sessions,
            config,
            credential_store,
            trusted_mcp_oauth_origin.into_iter().collect(),
            agent_state,
        )
    }

    pub(crate) fn from_sessions_with_oauth_policy_and_agent_state(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origins: Vec<reqwest::Url>,
        agent_state: AgentDefinitionState,
    ) -> Arc<Self> {
        Self::from_sessions_with_catalog_source(
            sessions,
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            AgentCatalogSource::Dynamic(agent_state),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "runtime construction receives independent session and host policy dependencies"
    )]
    fn from_sessions_with_catalog_source(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origins: Vec<reqwest::Url>,
        agent_catalog: AgentCatalogSource,
    ) -> Arc<Self> {
        let current_catalog = agent_catalog.catalog();
        let default_agent_definition = current_catalog
            .resolved(current_catalog.default_agent_name())
            .expect("catalog default agent must be present")
            .clone();
        let runtime_factory = runtime_factory(
            credential_store,
            trusted_mcp_oauth_origins,
            default_agent_definition,
            &agent_catalog,
            config.clone(),
        );
        let sessions = sessions.with_runtime_factory(runtime_factory);
        Arc::new(Self {
            sessions,
            config,
            agent_catalog,
        })
    }

    async fn load_config(&self) -> Configuration {
        self.config.load().await
    }

    fn selected_agent_bundle(
        catalog: &AgentDefinitionCatalog,
        agent_name: &str,
    ) -> Result<waltr_agent::AgentDefinitionBundle, Status> {
        if agent_name.trim().is_empty() {
            return Err(Status::invalid_argument("agent name must not be empty"));
        }
        catalog.bundle(agent_name).ok_or_else(|| {
            Status::invalid_argument(format!("agent '{agent_name}' is not selectable"))
        })
    }

    fn summary_to_proto(
        summary: SessionSummary,
        config: &Configuration,
        default_agent_name: &str,
    ) -> Result<waltr::grpc::waltr::session::v1::SessionSummary, Status> {
        session_summary_to_proto(summary, config, default_agent_name)
    }

    async fn create_session_internal(
        &self,
        inference_settings: Option<waltr::grpc::waltr::session::v1::InferenceSettingsRequest>,
        agent_name: Option<&str>,
    ) -> Result<(AgentSession, Configuration, String), Status> {
        let config = self.load_config().await;
        let inference_settings =
            resolve_inference_settings_for_create(inference_settings, &config)?;
        let catalog = self.agent_catalog.catalog();
        let default_agent_name = catalog.default_agent_name().to_owned();
        let agent_bundle = match agent_name {
            Some(agent_name) => Self::selected_agent_bundle(&catalog, agent_name)?,
            None => catalog.default_bundle(),
        };
        let mut request = SessionCreateRequest::new(SessionId::generate());
        request.inference_settings = inference_settings;
        request.agent_definition = Some(agent_bundle);
        request.metadata_values = BTreeMap::default();
        let session = self
            .sessions
            .create(request)
            .await
            .map_err(agent_error_to_status)?;
        Ok((session, config, default_agent_name))
    }
}

#[tonic::async_trait]
impl<S> super::SessionHost for SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn set_completion_admission_handler(
        &self,
        handler: Option<Arc<dyn waltr_agent::SessionCompletionCoordinationHandler>>,
    ) {
        if let Some(handler) = handler {
            self.sessions.set_completion_admission_handler(handler);
        } else {
            self.sessions.clear_completion_admission_handler();
        }
    }

    async fn create(&self, agent_name: Option<String>) -> Result<AgentSession, Status> {
        self.create_session_internal(None, agent_name.as_deref())
            .await
            .map(|(session, _, _)| session)
    }

    async fn create_exact(
        &self,
        request: SessionCreateRequest,
    ) -> Result<AgentSession, waltr_agent::LlmError> {
        self.sessions.create(request).await
    }

    async fn capture_latest_completed(
        &self,
        id: &SessionId,
    ) -> Result<waltr_agent::CompletedSessionCapture, waltr_agent::SessionCaptureError> {
        self.sessions.capture_latest_completed(id).await
    }

    async fn acquire_pause_reject_busy(
        &self,
        id: &SessionId,
        reason: waltr_agent::SessionCoordinationReason,
    ) -> Result<waltr_agent::SessionPauseLease, waltr_agent::SessionPauseError> {
        self.sessions.acquire_pause_reject_busy(id, reason).await
    }

    async fn capture_paused_effective(
        &self,
        lease: &waltr_agent::SessionPauseLease,
    ) -> Result<waltr_agent::CompletedSessionCapture, waltr_agent::SessionPauseError> {
        self.sessions.capture_paused_effective(lease).await
    }

    async fn apply_paused_mask_mutation(
        &self,
        lease: &waltr_agent::SessionPauseLease,
        mutation: waltr_agent::SessionMaskMutation,
    ) -> Result<waltr_agent::SessionMaskMutationOutcome, waltr_agent::SessionPauseError> {
        self.sessions
            .apply_paused_mask_mutation(lease, mutation)
            .await
    }

    async fn arm_paused_continuation(
        &self,
        lease: &waltr_agent::SessionPauseLease,
        mutation_cursor: waltr_agent::TimelineCursor,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, waltr_agent::SessionPauseError> {
        self.sessions
            .arm_paused_continuation(lease, mutation_cursor, limits)
            .await
    }

    async fn release_pause(
        &self,
        lease: &waltr_agent::SessionPauseLease,
    ) -> Result<(), waltr_agent::SessionPauseError> {
        self.sessions.release_pause(lease).await
    }

    async fn start_durable_prompt(
        &self,
        id: SessionId,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.sessions.start_durable_prompt(id, limits).await
    }

    async fn list(&self) -> Result<Vec<SessionSummary>, waltr_agent::LlmError> {
        self.sessions.list().await
    }

    async fn load(&self, id: &SessionId) -> Result<AgentSession, waltr_agent::LlmError> {
        self.sessions.load(id).await
    }

    async fn wipe(&self) -> Result<(), waltr_agent::LlmError> {
        self.sessions.wipe_sessions().await
    }

    async fn interrupt(&self, id: &SessionId) -> Result<AgentSession, waltr_agent::LlmError> {
        self.sessions.interrupt(id).await
    }

    async fn active_state(&self, id: &SessionId) -> Option<AgentSessionActiveRunState> {
        self.sessions.active_state(id).await
    }

    async fn submit_prompt(
        &self,
        id: SessionId,
        prompt: String,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError> {
        self.sessions
            .submit_prompt(id, prompt, RunConflictPolicy::QueueIfActive)
            .await
    }

    async fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: String,
        value: serde_json::Value,
        condition: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome, waltr_agent::LlmError> {
        self.sessions
            .set_metadata_value_conditionally(id, key, value, condition)
            .await
    }

    fn watch_sessions(&self) -> Pin<Box<dyn Stream<Item = SessionListUpdate> + Send>> {
        Box::pin(self.sessions.watch_sessions())
    }
}

#[tonic::async_trait]
impl<S> GeneratedSessionService for SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    type WatchSessionsStream =
        Pin<Box<dyn Stream<Item = Result<WatchSessionsResponse, Status>> + Send>>;
    type WatchTimelineStream =
        Pin<Box<dyn Stream<Item = Result<WatchTimelineResponse, Status>> + Send>>;

    async fn list_agents(
        &self,
        _request: Request<ListAgentsRequest>,
    ) -> Result<Response<ListAgentsResponse>, Status> {
        let catalog = self.agent_catalog.catalog();
        Ok(Response::new(ListAgentsResponse {
            agents: catalog
                .entries()
                .into_iter()
                .map(|agent| SelectableAgent {
                    name: agent.name,
                    description: agent.description,
                })
                .collect(),
            default_agent_name: catalog.default_agent_name().to_owned(),
        }))
    }
    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "create_session", session_id)
    )]
    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        let request = request.into_inner();
        tracing::info!("session create accepted");
        let agent_name = request
            .agent_selection
            .as_ref()
            .map(|selection| selection.agent_name.as_str());
        let (session, config, default_agent_name) = self
            .create_session_internal(request.inference_settings, agent_name)
            .await?;
        tracing::Span::current().record("session_id", session.record.id.as_str());
        tracing::info!(session_id = %session.record.id.as_str(), "session create completed");

        Ok(Response::new(CreateSessionResponse {
            session: Some(Self::summary_to_proto(
                session.record.summary(),
                &config,
                &default_agent_name,
            )?),
        }))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        tracing::trace!("list sessions requested");
        let config = self.load_config().await;
        let catalog = self.agent_catalog.catalog();
        let sessions = self
            .sessions
            .list()
            .await
            .map_err(agent_error_to_status)?
            .into_iter()
            .map(|summary| Self::summary_to_proto(summary, &config, catalog.default_agent_name()))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Response::new(ListSessionsResponse { sessions }))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "delete_session", session_id)
    )]
    async fn delete_session(
        &self,
        request: Request<DeleteSessionRequest>,
    ) -> Result<Response<DeleteSessionResponse>, Status> {
        let id = request.into_inner().id;
        if id.trim().is_empty() {
            return Err(Status::invalid_argument("session id must not be empty"));
        }

        tracing::Span::current().record("session_id", id.as_str());
        tracing::info!("session delete accepted");
        let id = SessionId::new(id);
        self.sessions
            .delete(&id)
            .await
            .map_err(agent_error_to_status)?;
        tracing::info!("session delete completed");

        Ok(Response::new(DeleteSessionResponse {}))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "wipe_sessions")
    )]
    async fn wipe_sessions(
        &self,
        request: Request<WipeSessionsRequest>,
    ) -> Result<Response<WipeSessionsResponse>, Status> {
        if !request.into_inner().confirm {
            return Err(Status::invalid_argument("confirm must be true"));
        }

        tracing::info!("session wipe accepted");
        super::SessionHost::wipe(self)
            .await
            .map_err(agent_error_to_status)?;
        tracing::info!("session wipe completed");

        Ok(Response::new(WipeSessionsResponse {}))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "update_session_inference_settings", session_id)
    )]
    async fn update_session_inference_settings(
        &self,
        request: Request<UpdateSessionInferenceSettingsRequest>,
    ) -> Result<Response<UpdateSessionInferenceSettingsResponse>, Status> {
        let request = request.into_inner();
        if request.id.trim().is_empty() {
            return Err(Status::invalid_argument("session id must not be empty"));
        }
        tracing::Span::current().record("session_id", request.id.as_str());
        tracing::info!("session inference settings update accepted");
        let inference_settings = request
            .inference_settings
            .ok_or_else(|| Status::invalid_argument("inference settings are required"))?;
        let config = self.load_config().await;
        let inference_settings = resolve_inference_settings(inference_settings, &config)?;
        let session = self
            .sessions
            .set_inference_settings(&SessionId::new(request.id), Some(inference_settings))
            .await
            .map_err(agent_error_to_status)?;
        tracing::info!("session inference settings update completed");
        let catalog = self.agent_catalog.catalog();

        Ok(Response::new(UpdateSessionInferenceSettingsResponse {
            session: Some(Self::summary_to_proto(
                session.record.summary(),
                &config,
                catalog.default_agent_name(),
            )?),
        }))
    }

    async fn update_session_agent(
        &self,
        request: Request<UpdateSessionAgentRequest>,
    ) -> Result<Response<UpdateSessionAgentResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.id)?;
        let selection = request
            .agent_selection
            .ok_or_else(|| Status::invalid_argument("agent selection is required"))?;
        let catalog = self.agent_catalog.catalog();
        Self::selected_agent_bundle(&catalog, &selection.agent_name)?;
        self.sessions
            .load(&session_id)
            .await
            .map_err(session_load_error_to_status)?;
        let session = self
            .sessions
            .switch_agent(&session_id, &selection.agent_name)
            .await
            .map_err(session_agent_error_to_status)?;
        let config = self.load_config().await;
        Ok(Response::new(UpdateSessionAgentResponse {
            session: Some(Self::summary_to_proto(
                session.record.summary(),
                &config,
                catalog.default_agent_name(),
            )?),
        }))
    }

    async fn watch_sessions(
        &self,
        _request: Request<WatchSessionsRequest>,
    ) -> Result<Response<Self::WatchSessionsStream>, Status> {
        tracing::trace!("watch sessions requested");
        let subscription = self.sessions.watch_sessions();
        let sequence = subscription.initial_sequence();
        let config = self.load_config().await;
        let catalog = self.agent_catalog.catalog();
        let sessions = self
            .sessions
            .list()
            .await
            .map_err(agent_error_to_status)?
            .into_iter()
            .map(|summary| Self::summary_to_proto(summary, &config, catalog.default_agent_name()))
            .collect::<Result<Vec<_>, _>>()?;
        let snapshot = Ok(WatchSessionsResponse {
            sequence,
            event: Some(watch_sessions_response::Event::Snapshot(
                SessionListSnapshot { sessions },
            )),
        });
        let config = self.config.clone();
        let agent_catalog = self.agent_catalog.clone();
        let stream = tokio_stream::once(snapshot).chain(subscription.then(move |update| {
            let config = config.clone();
            let agent_catalog = agent_catalog.clone();
            async move {
                let current_config = config.load().await;
                let catalog = agent_catalog.catalog();
                let event = match update.event {
                    SessionListEvent::Upsert(session) => {
                        watch_sessions_response::Event::Session(session_summary_to_proto(
                            session,
                            &current_config,
                            catalog.default_agent_name(),
                        )?)
                    }
                    SessionListEvent::Deleted(id) => {
                        watch_sessions_response::Event::DeletedId(id.as_str().to_owned())
                    }
                    SessionListEvent::Cleared => {
                        watch_sessions_response::Event::Cleared(SessionListCleared {})
                    }
                };

                Ok(WatchSessionsResponse {
                    sequence: update.sequence,
                    event: Some(event),
                })
            }
        }));

        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_timeline(
        &self,
        request: Request<GetTimelineRequest>,
    ) -> Result<Response<GetTimelineResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        let mode = timeline_mode_from_proto(request.projection_mode)?;
        let page = self
            .sessions
            .timeline_projection(&session_id, mode, query_from_proto(request.query))
            .await
            .map_err(agent_error_to_status)?;
        let (projected_events, projection) = timeline_page_to_proto(page)?;
        let (events, projected_events) = match mode {
            SessionTimelineMode::Raw => (
                projected_events
                    .into_iter()
                    .map(|event| match event.event {
                        Some(
                            waltr::grpc::waltr::session::v1::projected_timeline_event::Event::Durable(
                                event,
                            ),
                        ) => Ok(event),
                        _ => Err(Status::internal(
                            "raw timeline projection returned a synthetic event",
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Vec::new(),
            ),
            SessionTimelineMode::Effective => (Vec::new(), projected_events),
        };

        Ok(Response::new(GetTimelineResponse {
            events,
            projected_events,
            projection: Some(projection),
        }))
    }

    #[tracing::instrument(
        name = "server.watch",
        skip(self, request),
        fields(operation = "watch_timeline", session_id)
    )]
    async fn watch_timeline(
        &self,
        request: Request<WatchTimelineRequest>,
    ) -> Result<Response<Self::WatchTimelineStream>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        let mode = timeline_mode_from_proto(request.projection_mode)?;
        tracing::Span::current().record("session_id", session_id.as_str());
        self.sessions
            .load(&session_id)
            .await
            .map_err(agent_error_to_status)?;
        if mode == SessionTimelineMode::Effective {
            return effective_timeline_watch(&self.sessions, session_id, request).await;
        }
        let subscription = self.sessions.watch_timeline(session_id.clone());
        let initial_sequence = subscription.initial_sequence();
        let channels = request.channels.clone();
        let highest_cursor = Arc::new(AtomicU64::new(request.after_cursor.unwrap_or(0)));
        let mut historical = vec![Ok(WatchTimelineResponse {
            sequence: initial_sequence,
            update: None,
        })];

        if request.after_cursor.is_some() {
            loop {
                let after_cursor = highest_cursor.load(Ordering::Relaxed);
                let catchup = self
                    .sessions
                    .timeline(
                        &session_id,
                        catchup_query(Some(after_cursor), 1000, &channels),
                    )
                    .instrument(tracing::info_span!(
                        "server.watch.timeline_query",
                        session_id = %session_id.as_str(),
                        query_kind = "catchup",
                        after_cursor,
                        limit = 1000_u64,
                    ))
                    .await
                    .map_err(agent_error_to_status)?;
                let event_count = catchup.len();
                tracing::debug!(session_id = %session_id.as_str(), event_count, "watch timeline catchup query completed");
                push_history(&mut historical, &highest_cursor, catchup);

                if event_count < 1000 {
                    break;
                }
            }
        } else if let Some(limit) = request.initial_limit {
            let events = self
                .sessions
                .timeline(&session_id, initial_query(limit, &channels))
                .instrument(tracing::info_span!(
                    "server.watch.timeline_query",
                    session_id = %session_id.as_str(),
                    query_kind = "initial",
                    limit,
                ))
                .await
                .map_err(agent_error_to_status)?;
            tracing::debug!(session_id = %session_id.as_str(), event_count = events.len(), "watch timeline initial query completed");
            push_history(&mut historical, &highest_cursor, events);
        }

        if request.after_cursor.is_none() {
            let catchup = self
                .sessions
                .timeline(&session_id, catchup_query(None, 1000, &channels))
                .instrument(tracing::info_span!(
                    "server.watch.timeline_query",
                    session_id = %session_id.as_str(),
                    query_kind = "catchup",
                    after_cursor = 0_u64,
                    limit = 1000_u64,
                ))
                .await
                .map_err(agent_error_to_status)?;
            tracing::debug!(session_id = %session_id.as_str(), event_count = catchup.len(), "watch timeline catchup query completed");
            push_history(&mut historical, &highest_cursor, catchup);
        }

        let highest_cursor_for_live = highest_cursor.clone();
        let live_channels = channels.clone();
        let live = subscription.filter_map(move |update| {
            let highest_cursor = highest_cursor_for_live.clone();
            let channels = live_channels.clone();
            match update.event {
                SessionTimelineEvent::Event(event) => {
                    let cursor = event.cursor.get();
                    if cursor <= highest_cursor.load(Ordering::Relaxed)
                        || !channel_matches(&event, &channels)
                    {
                        return None;
                    }
                    highest_cursor.fetch_max(cursor, Ordering::Relaxed);
                    tracing::trace!(
                        sequence = update.sequence,
                        cursor,
                        kind = ?event.kind,
                        "watch timeline live event delivered"
                    );
                    Some(
                        timeline_event_to_proto(event).map(|event| WatchTimelineResponse {
                            sequence: update.sequence,
                            update: Some(watch_timeline_response::Update::Event(event)),
                        }),
                    )
                }
                SessionTimelineEvent::Truncated {
                    session_id,
                    after_cursor,
                } => {
                    let cursor = after_cursor.map_or(0, TimelineCursor::get);
                    highest_cursor.store(cursor, Ordering::Relaxed);
                    Some(Ok(WatchTimelineResponse {
                        sequence: update.sequence,
                        update: Some(watch_timeline_response::Update::Truncation(
                            TimelineTruncation {
                                session_id: session_id.as_str().to_owned(),
                                after_cursor: after_cursor.map(TimelineCursor::get),
                            },
                        )),
                    }))
                }
            }
        });
        let stream = tokio_stream::iter(historical).chain(live);

        tracing::trace!(session_id = %session_id.as_str(), sequence = initial_sequence, "watch timeline requested");
        Ok(Response::new(Box::pin(stream)))
    }

    async fn search_session_templates(
        &self,
        request: Request<SearchSessionTemplatesRequest>,
    ) -> Result<Response<SearchSessionTemplatesResponse>, Status> {
        Ok(Response::new(
            handle_template_search(&self.sessions, request.into_inner()).await,
        ))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "submit_user_message", session_id)
    )]
    async fn submit_user_message(
        &self,
        request: Request<SubmitUserMessageRequest>,
    ) -> Result<Response<SubmitUserMessageResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        if request.text.trim().is_empty() {
            return Err(Status::invalid_argument("text must not be empty"));
        }
        tracing::Span::current().record("session_id", session_id.as_str());
        tracing::info!("session prompt accepted");
        self.sessions
            .load(&session_id)
            .await
            .map_err(agent_error_to_status)?;
        self.sessions
            .submit_prompt(session_id, request.text, RunConflictPolicy::QueueIfActive)
            .await
            .map_err(session_run_error_to_status)?;
        tracing::info!("session prompt dispatched");

        Ok(Response::new(SubmitUserMessageResponse {}))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(
            operation = "respond_external_request",
            session_id,
            external_request_id
        )
    )]
    async fn respond_external_request(
        &self,
        request: Request<RespondExternalRequestRequest>,
    ) -> Result<Response<RespondExternalRequestResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        if request.request_id.trim().is_empty() {
            return Err(Status::invalid_argument("request id must not be empty"));
        }
        tracing::Span::current().record("session_id", session_id.as_str());
        tracing::Span::current().record("external_request_id", request.request_id.as_str());
        tracing::info!("external request response accepted");
        let response = serde_json::from_str(&request.response_json)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        self.sessions
            .load(&session_id)
            .await
            .map_err(agent_error_to_status)?;
        self.sessions
            .resume_external_response(
                session_id,
                ExternalRequestId::new(request.request_id),
                response,
                RunConflictPolicy::RejectIfActive,
            )
            .await
            .map_err(session_run_error_to_status)?;
        tracing::info!("external request response dispatched");

        Ok(Response::new(RespondExternalRequestResponse {}))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "interrupt_session", session_id)
    )]
    async fn interrupt_session(
        &self,
        request: Request<InterruptSessionRequest>,
    ) -> Result<Response<InterruptSessionResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        tracing::Span::current().record("session_id", session_id.as_str());
        tracing::info!("session interrupt accepted");
        self.sessions
            .interrupt(&session_id)
            .await
            .map_err(agent_error_to_status)?;
        tracing::info!("session interrupt completed");
        Ok(Response::new(InterruptSessionResponse {}))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "rollback_session", session_id)
    )]
    async fn rollback_session(
        &self,
        request: Request<RollbackSessionRequest>,
    ) -> Result<Response<RollbackSessionResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        tracing::Span::current().record("session_id", session_id.as_str());
        tracing::info!("session rollback accepted");
        let session = match required_rollback_target(request.target)? {
            rollback_target::Target::Turn(turn) => {
                self.sessions
                    .rollback_to_turn(session_id.clone(), turn)
                    .await
            }
            rollback_target::Target::Cursor(cursor) => {
                self.sessions
                    .rollback_to_cursor(session_id.clone(), TimelineCursor::new(cursor))
                    .await
            }
            rollback_target::Target::BeforeCursor(cursor) => {
                self.sessions
                    .rollback_before_cursor(session_id.clone(), TimelineCursor::new(cursor))
                    .await
            }
        }
        .map_err(agent_error_to_status)?;
        let after_cursor = session
            .record
            .timeline_events()
            .last()
            .map(|event| event.cursor.get());
        let config = self.load_config().await;
        let catalog = self.agent_catalog.catalog();
        tracing::info!("session rollback completed");
        Ok(Response::new(RollbackSessionResponse {
            session: Some(Self::summary_to_proto(
                session.record.summary(),
                &config,
                catalog.default_agent_name(),
            )?),
            truncation: Some(TimelineTruncation {
                session_id: session_id.as_str().to_owned(),
                after_cursor,
            }),
        }))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "continue_session", session_id)
    )]
    async fn continue_session(
        &self,
        request: Request<ContinueSessionRequest>,
    ) -> Result<Response<ContinueSessionResponse>, Status> {
        let request = request.into_inner();
        let session_id = required_session_id(&request.session_id)?;
        tracing::Span::current().record("session_id", session_id.as_str());
        tracing::info!("session continue accepted");
        match required_rollback_target(request.target)? {
            rollback_target::Target::Turn(turn) => {
                self.sessions
                    .continue_from_turn(session_id, turn)
                    .await
                    .map_err(session_run_error_to_status)?;
            }
            rollback_target::Target::Cursor(cursor) => {
                self.sessions
                    .continue_from_cursor(session_id, TimelineCursor::new(cursor))
                    .await
                    .map_err(session_run_error_to_status)?;
            }
            rollback_target::Target::BeforeCursor(_) => {
                return Err(Status::invalid_argument(
                    "continue target must be turn or cursor",
                ));
            }
        }
        tracing::info!("session continue dispatched");
        Ok(Response::new(ContinueSessionResponse {}))
    }
}

fn required_session_id(value: &str) -> Result<SessionId, Status> {
    if value.trim().is_empty() {
        Err(Status::invalid_argument("session id must not be empty"))
    } else {
        Ok(SessionId::new(value.to_owned()))
    }
}

fn timeline_mode_from_proto(value: i32) -> Result<SessionTimelineMode, Status> {
    match ProtoTimelineProjectionMode::try_from(value) {
        Ok(ProtoTimelineProjectionMode::Unspecified | ProtoTimelineProjectionMode::Raw) => {
            Ok(SessionTimelineMode::Raw)
        }
        Ok(ProtoTimelineProjectionMode::Effective) => Ok(SessionTimelineMode::Effective),
        Err(_) => Err(Status::invalid_argument(
            "timeline projection mode is invalid",
        )),
    }
}

async fn effective_timeline_watch<S>(
    sessions: &SessionCoordinator<S>,
    session_id: SessionId,
    request: WatchTimelineRequest,
) -> Result<
    Response<Pin<Box<dyn Stream<Item = Result<WatchTimelineResponse, Status>> + Send>>>,
    Status,
>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    const REQUERY_LIMIT: usize = 1000;

    // Subscribe before querying so mutations racing initial projection cannot be missed.
    let subscription = sessions.watch_timeline_projection(session_id.clone());
    let initial_sequence = subscription.initial_sequence();
    let query = if let Some(after_cursor) = request.after_cursor {
        catchup_query(Some(after_cursor), REQUERY_LIMIT, &request.channels)
    } else {
        initial_query(request.initial_limit.unwrap_or(100), &request.channels)
    };
    let page = sessions
        .timeline_projection(&session_id, SessionTimelineMode::Effective, query.clone())
        .await
        .map_err(agent_error_to_status)?;
    let revision = page.projection_revision;
    let highest_cursor = projection_highest_cursor(&page);
    let (events, projection) = timeline_page_to_proto(page)?;
    let initial = Ok(WatchTimelineResponse {
        sequence: initial_sequence,
        update: Some(watch_timeline_response::Update::ProjectionReset(
            TimelineProjectionReset {
                events,
                projection: Some(projection),
            },
        )),
    });

    let sessions = sessions.clone();
    let channels = request.channels;
    let live = futures_util::stream::unfold(
        (subscription, revision, highest_cursor),
        move |(mut subscription, mut revision, mut highest_cursor)| {
            let sessions = sessions.clone();
            let session_id = session_id.clone();
            let query = query.clone();
            let channels = channels.clone();
            async move {
                loop {
                    let update = subscription.next().await?;
                    let response = match update.event {
                        SessionTimelineProjectionUpdateEvent::Event(event) => {
                            let cursor = event.cursor.get();
                            if cursor <= highest_cursor || !channel_matches(&event, &channels) {
                                continue;
                            }
                            highest_cursor = cursor;
                            projected_timeline_event_to_proto(SessionTimelineProjectionEvent {
                                event: SessionTimelineProjectedEvent::Durable(event),
                                presentation: SessionTimelinePresentation::Durable,
                            })
                            .map(|event| WatchTimelineResponse {
                                sequence: update.sequence,
                                update: Some(watch_timeline_response::Update::ProjectedEvent(
                                    event,
                                )),
                            })
                        }
                        SessionTimelineProjectionUpdateEvent::Invalidated {
                            projection_revision,
                            ..
                        } if projection_revision <= revision => continue,
                        SessionTimelineProjectionUpdateEvent::Invalidated { .. }
                        | SessionTimelineProjectionUpdateEvent::ResetRequired { .. } => {
                            match sessions
                                .timeline_projection(
                                    &session_id,
                                    SessionTimelineMode::Effective,
                                    query.clone(),
                                )
                                .await
                                .map_err(agent_error_to_status)
                            {
                                Ok(page) => {
                                    revision = page.projection_revision;
                                    highest_cursor = projection_highest_cursor(&page);
                                    timeline_page_to_proto(page).map(|(events, projection)| {
                                        WatchTimelineResponse {
                                            sequence: update.sequence,
                                            update: Some(
                                                watch_timeline_response::Update::ProjectionReset(
                                                    TimelineProjectionReset {
                                                        events,
                                                        projection: Some(projection),
                                                    },
                                                ),
                                            ),
                                        }
                                    })
                                }
                                Err(status) => Err(status),
                            }
                        }
                    };
                    return Some((response, (subscription, revision, highest_cursor)));
                }
            }
        },
    );
    Ok(Response::new(Box::pin(
        tokio_stream::once(initial).chain(live),
    )))
}

fn projection_highest_cursor(page: &waltr_agent::SessionTimelinePage) -> u64 {
    page.events
        .iter()
        .map(SessionTimelineProjectionEvent::cursor)
        .chain(page.raw_page_boundary)
        .max()
        .map_or(0, TimelineCursor::get)
}

fn session_load_error_to_status(error: waltr_agent::LlmError) -> Status {
    if error.to_string().contains("session not found:") {
        Status::not_found(error.to_string())
    } else {
        agent_error_to_status(error)
    }
}

fn session_agent_error_to_status(error: AgentSessionRunError) -> Status {
    match error {
        AgentSessionRunError::Runtime(AgentSessionRuntimeError::FailedPrecondition(message)) => {
            Status::failed_precondition(message)
        }
        AgentSessionRunError::Agent(error) if error.to_string().contains("session not found:") => {
            Status::not_found(error.to_string())
        }
        error => session_run_error_to_status(error),
    }
}

fn required_rollback_target(
    target: Option<waltr::grpc::waltr::session::v1::RollbackTarget>,
) -> Result<rollback_target::Target, Status> {
    target
        .and_then(|target| target.target)
        .ok_or_else(|| Status::invalid_argument("rollback target is required"))
}

async fn handle_template_search<S>(
    sessions: &SessionCoordinator<S>,
    request: SearchSessionTemplatesRequest,
) -> SearchSessionTemplatesResponse
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let request_id = request.request_id.clone();
    let response = match required_session_id(&request.session_id) {
        Ok(session_id) => {
            let search = SessionTemplateSearchRequest {
                query: request.query,
                limit: request.limit as usize,
                kinds: template_kinds_from_proto(&request.kinds),
                target: template_search_target_from_proto(request.target),
                cursor: request.cursor.map(|cursor| cursor as usize),
            };
            match sessions.search_templates(&session_id, search).await {
                Ok(options) => {
                    search_session_templates_response::Result::Options(SessionTemplateOptions {
                        options: options.into_iter().map(template_option_to_proto).collect(),
                    })
                }
                Err(error) => {
                    search_session_templates_response::Result::Error(SessionTemplateSearchError {
                        message: error.to_string(),
                    })
                }
            }
        }
        Err(error) => {
            search_session_templates_response::Result::Error(SessionTemplateSearchError {
                message: error.message().to_owned(),
            })
        }
    };

    SearchSessionTemplatesResponse {
        request_id,
        result: Some(response),
    }
}

fn template_search_target_from_proto(target: i32) -> SessionTemplateSearchTarget {
    match ProtoSessionTemplateSearchTarget::try_from(target).ok() {
        Some(ProtoSessionTemplateSearchTarget::Prompts) => SessionTemplateSearchTarget::Prompts,
        Some(
            ProtoSessionTemplateSearchTarget::Resources
            | ProtoSessionTemplateSearchTarget::Unspecified,
        )
        | None => SessionTemplateSearchTarget::Resources,
    }
}

fn template_kinds_from_proto(kinds: &[i32]) -> Vec<SessionTemplateKind> {
    kinds
        .iter()
        .filter_map(
            |kind| match ProtoSessionTemplateKind::try_from(*kind).ok()? {
                ProtoSessionTemplateKind::File => Some(SessionTemplateKind::File),
                ProtoSessionTemplateKind::Tool => Some(SessionTemplateKind::Tool),
                ProtoSessionTemplateKind::Prompt => Some(SessionTemplateKind::Prompt),
                ProtoSessionTemplateKind::Resource => Some(SessionTemplateKind::Resource),
                ProtoSessionTemplateKind::Unspecified => None,
            },
        )
        .collect()
}

fn template_option_to_proto(option: SessionTemplateOption) -> ProtoSessionTemplateOption {
    let kind = match option.kind {
        SessionTemplateKind::File => ProtoSessionTemplateKind::File,
        SessionTemplateKind::Tool => ProtoSessionTemplateKind::Tool,
        SessionTemplateKind::Prompt => ProtoSessionTemplateKind::Prompt,
        SessionTemplateKind::Resource => ProtoSessionTemplateKind::Resource,
    };
    ProtoSessionTemplateOption {
        kind: kind as i32,
        label: option.label,
        detail: option.detail,
        template: option.template,
        metadata_json: option.metadata.to_string(),
    }
}

fn push_history(
    output: &mut Vec<Result<WatchTimelineResponse, Status>>,
    highest_cursor: &AtomicU64,
    events: Vec<waltr_agent::AgentTimelineEvent>,
) {
    for event in events {
        let cursor = event.cursor.get();
        if cursor <= highest_cursor.load(Ordering::Relaxed) {
            continue;
        }
        highest_cursor.fetch_max(cursor, Ordering::Relaxed);
        output.push(
            timeline_event_to_proto(event).map(|event| WatchTimelineResponse {
                sequence: 0,
                update: Some(watch_timeline_response::Update::Event(event)),
            }),
        );
    }
}

fn default_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[allow(
    clippy::too_many_arguments,
    reason = "runtime factory policy is supplied by independent server-owned capabilities"
)]
fn runtime_factory(
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    default_agent_definition: ResolvedAgent,
    agent_catalog: &AgentCatalogSource,
    config: ConfigHandle,
) -> AgentSessionRuntimeFactory {
    let factory =
        AgentSessionRuntimeFactory::new(default_root(), credential_store, default_agent_definition);
    let factory = match agent_catalog {
        AgentCatalogSource::Static(catalog) => factory.with_agent_catalog(catalog.clone()),
        AgentCatalogSource::Dynamic(state) => {
            let state = state.clone();
            factory.with_agent_catalog_provider(move || state.snapshot().catalog().clone())
        }
    };
    factory
        .with_trusted_mcp_oauth_origins(trusted_mcp_oauth_origins)
        // TODO: This feels like it should be a similar catalog like the agents catalog
        .with_mcp_connection_resolver(move |request| {
            let config = config.clone();
            Box::pin(async move { resolve_mcp_connections(&config.load().await, request) })
        })
}

fn resolve_mcp_connections(
    config: &Configuration,
    request: McpConnectionResolveRequest,
) -> Result<Vec<McpResolvedConnection>, AgentSessionRuntimeError> {
    use waltr::grpc::waltr::config::v1::McpServerType;
    use waltr_agent::definition::McpServerSelection;

    if let McpServerSelection::Include(keys) = &request.servers {
        let missing = keys
            .iter()
            .filter(|key| !config.mcp_servers.contains_key(*key))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
                "configured MCP servers are missing: {}",
                missing.join(", ")
            )));
        }
    }

    let mut keys = match request.servers {
        McpServerSelection::Include(keys) => keys.into_iter().collect::<Vec<_>>(),
        McpServerSelection::Exclude(excluded) => config
            .mcp_servers
            .keys()
            .filter(|key| !excluded.contains(*key))
            .cloned()
            .collect::<Vec<_>>(),
    };
    keys.sort();
    keys.into_iter()
        .map(|key| {
            let server = &config.mcp_servers[&key];
            let kind = match McpServerType::try_from(server.r#type)
                .unwrap_or(McpServerType::Unspecified)
            {
                McpServerType::Stdio => McpConnectionKind::Stdio,
                McpServerType::StreamableHttp => McpConnectionKind::StreamableHttp,
                McpServerType::Unspecified => {
                    return Err(AgentSessionRuntimeError::FailedPrecondition(format!(
                        "mcp server '{key}' type is unspecified"
                    )));
                }
            };
            Ok(McpResolvedConnection {
                key,
                kind,
                command: server.command.clone(),
                url: server.url.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap},
        fs,
        time::Duration,
    };

    use waltr::grpc::waltr::config::v1::{Configuration, McpServer, McpServerType, Provider};
    use waltr::grpc::waltr::session::v1::{
        AgentSelectionRequest, CreateSessionRequest, ListAgentsRequest, UpdateSessionAgentRequest,
    };
    use waltr_agent::{
        AgentSessionRuntimeError, CredentialStore, FileCredentialStore, McpConnectionKind,
        McpConnectionResolveRequest, MemorySessionStore, SessionCoordinator, SessionId,
        definition::McpServerSelection,
    };
    use tonic::Request;

    use crate::{
        agent_definition::{AgentDefinitionInspection, AgentDefinitionState},
        config::ConfigHandle,
    };

    use super::{GeneratedSessionService, SessionService, resolve_mcp_connections};

    fn agent_source(name: &str, description: &str) -> String {
        format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata: {{ name: {name} }}\nspec: {{ description: {description} }}\n"
        )
    }

    fn config_handle() -> ConfigHandle {
        ConfigHandle::from_resources(crate::test_support::resource_state_with_overrides(&[]))
    }

    fn credential_store(root: &std::path::Path) -> CredentialStore {
        FileCredentialStore::new(root.join("credentials.json")).into()
    }

    fn persisted_etag(state: &AgentDefinitionState, path: &str) -> String {
        match state.inspect(Some(path)).expect("resource inspects") {
            AgentDefinitionInspection::Resource(detail) => detail.persisted_etag,
            AgentDefinitionInspection::Catalog { .. } => unreachable!(),
        }
    }

    fn inventory() -> Configuration {
        Configuration {
            mcp_servers: HashMap::from([
                (
                    "zeta".to_owned(),
                    McpServer {
                        r#type: McpServerType::StreamableHttp as i32,
                        url: "https://zeta.example/mcp".to_owned(),
                        ..McpServer::default()
                    },
                ),
                (
                    "alpha".to_owned(),
                    McpServer {
                        r#type: McpServerType::Stdio as i32,
                        command: "alpha-mcp --stdio".to_owned(),
                        ..McpServer::default()
                    },
                ),
            ]),
            ..Configuration::default()
        }
    }

    #[test]
    fn resolver_selects_all_by_default_in_key_order() {
        let resolved = resolve_mcp_connections(
            &inventory(),
            McpConnectionResolveRequest {
                servers: McpServerSelection::Exclude(BTreeSet::new()),
            },
        )
        .expect("inventory resolves");

        assert_eq!(
            resolved
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        assert_eq!(resolved[0].kind, McpConnectionKind::Stdio);
        assert_eq!(resolved[0].command, "alpha-mcp --stdio");
        assert_eq!(resolved[1].kind, McpConnectionKind::StreamableHttp);
        assert_eq!(resolved[1].url, "https://zeta.example/mcp");
    }

    #[test]
    fn resolver_include_empty_selects_none_and_missing_exclude_is_ignored() {
        let none = resolve_mcp_connections(
            &inventory(),
            McpConnectionResolveRequest {
                servers: McpServerSelection::Include(BTreeSet::new()),
            },
        )
        .expect("empty include resolves");
        assert!(none.is_empty());

        let all = resolve_mcp_connections(
            &inventory(),
            McpConnectionResolveRequest {
                servers: McpServerSelection::Exclude(BTreeSet::from(["missing".to_owned()])),
            },
        )
        .expect("missing exclude is ignored");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn resolver_reports_all_missing_includes_deterministically() {
        let error = resolve_mcp_connections(
            &inventory(),
            McpConnectionResolveRequest {
                servers: McpServerSelection::Include(BTreeSet::from([
                    "missing-z".to_owned(),
                    "alpha".to_owned(),
                    "missing-a".to_owned(),
                ])),
            },
        )
        .expect_err("missing includes fail");
        let AgentSessionRuntimeError::FailedPrecondition(message) = error else {
            panic!("expected failed precondition");
        };
        assert_eq!(
            message,
            "configured MCP servers are missing: missing-a, missing-z"
        );
    }

    #[test]
    fn resolver_uses_current_endpoint_inventory_independent_of_providers() {
        let selection = McpConnectionResolveRequest {
            servers: McpServerSelection::Include(BTreeSet::from(["zeta".to_owned()])),
        };
        let initial = resolve_mcp_connections(&inventory(), selection.clone())
            .expect("initial inventory resolves");

        let mut changed = inventory();
        changed
            .providers
            .insert("unrelated".to_owned(), Provider::default());
        changed.mcp_servers.get_mut("zeta").expect("server").url =
            "https://changed.example/mcp".to_owned();
        let current =
            resolve_mcp_connections(&changed, selection).expect("changed inventory resolves");

        assert_eq!(initial[0].url, "https://zeta.example/mcp");
        assert_eq!(current[0].url, "https://changed.example/mcp");
        assert_eq!(initial[0].key, current[0].key);
        assert_eq!(initial[0].kind, current[0].kind);
    }

    #[test]
    fn resolver_treats_waltr_as_an_ordinary_configured_server() {
        let mut inventory = inventory();
        inventory.mcp_servers.insert(
            "waltr".to_owned(),
            McpServer {
                r#type: McpServerType::StreamableHttp as i32,
                url: "https://waltr.example/mcp".to_owned(),
                ..McpServer::default()
            },
        );
        let resolved = resolve_mcp_connections(
            &inventory,
            McpConnectionResolveRequest {
                servers: McpServerSelection::Exclude(BTreeSet::new()),
            },
        )
        .unwrap();
        assert_eq!(
            resolved
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "waltr", "zeta"]
        );
        let selected = resolve_mcp_connections(
            &inventory,
            McpConnectionResolveRequest {
                servers: McpServerSelection::Include(BTreeSet::from(["waltr".to_owned()])),
            },
        )
        .unwrap();
        assert_eq!(selected[0].key, "waltr");
        assert_eq!(selected[0].url, "https://waltr.example/mcp");
    }

    #[tokio::test]
    async fn dynamic_list_agents_tracks_valid_state_and_keeps_last_known_good() {
        let directory = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&directory, &[], "coding").unwrap();
        let service = SessionService::new_with_config_and_agent_state(
            MemorySessionStore::new(),
            config_handle(),
            credential_store(&directory),
            None,
            state.clone(),
        );
        let initial = service
            .list_agents(Request::new(ListAgentsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(initial.default_agent_name, "coding");
        assert_eq!(
            initial
                .agents
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>(),
            ["coding", "configuration", "default"]
        );

        let source = agent_source("later", "later agent");
        state
            .replace(
                "agents/later.yaml",
                source,
                &persisted_etag(&state, "agents/later.yaml"),
            )
            .await
            .unwrap();
        let current = service
            .list_agents(Request::new(ListAgentsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(current.default_agent_name, "coding");
        assert!(current.agents.iter().any(|agent| agent.name == "later"));

        let watcher = state.spawn_watcher().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::write(
            directory.join(".waltr/resources/agents/later.yaml"),
            "kind: [",
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.reload_error().is_none() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("invalid edit is observed");
        let last_known_good = service
            .list_agents(Request::new(ListAgentsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(last_known_good, current);
        watcher.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dynamic_new_sessions_capture_current_sources_without_rebasing_existing_sessions() {
        let directory = crate::test_support::temp_root();
        let external_source = agent_source("external", "external agent");
        let external_path = directory.join("external.yaml");
        fs::write(&external_path, &external_source).unwrap();
        let state = AgentDefinitionState::load(&directory, &[external_path], "coding").unwrap();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let service = SessionService::from_sessions_with_config_and_agent_state(
            sessions.clone(),
            config_handle(),
            credential_store(&directory),
            None,
            state.clone(),
        );
        let before = super::super::SessionHost::create(service.as_ref(), None)
            .await
            .unwrap();
        let before_bundle = before.record.agent_definition.clone().unwrap();

        let replacement = agent_source("later", "exact replacement bytes");
        state
            .replace(
                "agents/later.yaml",
                replacement.clone(),
                &persisted_etag(&state, "agents/later.yaml"),
            )
            .await
            .unwrap();
        let after = super::super::SessionHost::create(service.as_ref(), Some("later".to_owned()))
            .await
            .unwrap();
        let after_bundle = after.record.agent_definition.unwrap();
        let reloaded_before = sessions.load(before.id()).await.unwrap();

        assert_eq!(
            after_bundle.sources,
            state.snapshot().sources(),
            "new bundles use one complete current snapshot"
        );
        assert!(after_bundle.sources.contains(&replacement));
        assert_eq!(after_bundle.sources.last(), Some(&external_source));
        assert_eq!(
            reloaded_before.record.agent_definition.unwrap(),
            before_bundle,
            "passive replacement must not rebase an existing record"
        );
    }

    #[tokio::test]
    async fn dynamic_update_selects_new_agent_through_normal_switch_audit() {
        let directory = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&directory, &[], "coding").unwrap();
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let service = SessionService::from_sessions_with_config_and_agent_state(
            sessions.clone(),
            config_handle(),
            credential_store(&directory),
            None,
            state.clone(),
        );
        let created = service
            .create_session(Request::new(CreateSessionRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .session
            .unwrap();
        let source = agent_source("later", "newly selectable");
        state
            .replace(
                "agents/later.yaml",
                source.clone(),
                &persisted_etag(&state, "agents/later.yaml"),
            )
            .await
            .unwrap();

        let response = service
            .update_session_agent(Request::new(UpdateSessionAgentRequest {
                id: created.id.clone(),
                agent_selection: Some(AgentSelectionRequest {
                    agent_name: "later".to_owned(),
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        let stored = sessions.load(&SessionId::new(created.id)).await.unwrap();
        let bundle = stored.record.agent_definition.unwrap();
        let audit = bundle
            .pending_switch_audit
            .expect("switch audit is retained");

        assert_eq!(response.session.unwrap().agent_name, "later");
        assert_eq!(bundle.agent_name, "later");
        assert!(bundle.sources.contains(&source));
        assert_eq!(audit.old_name, "coding");
        assert_eq!(audit.new_name, "later");
    }

    #[tokio::test]
    async fn static_service_constructor_preserves_default_agent_labeling() {
        let directory = crate::test_support::temp_root();
        let catalog = crate::test_support::default_agent_catalog();
        let service = SessionService::new_with_config(
            MemorySessionStore::new(),
            config_handle(),
            credential_store(&directory),
            None,
            catalog,
        );

        let response = service
            .list_agents(Request::new(ListAgentsRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.default_agent_name, "coding");
        assert!(response.agents.iter().any(|agent| agent.name == "coding"));
    }
}
