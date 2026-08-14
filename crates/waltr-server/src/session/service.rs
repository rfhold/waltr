//! Implementation of the generated `SessionService`.

use std::{collections::BTreeMap, path::PathBuf, pin::Pin, sync::Arc};

use crate::config::ConfigHandle;

use super::{
    convert::session_summary_to_proto,
    error::{agent_error_to_status, session_run_error_to_status},
    inference_settings::{resolve_inference_settings, resolve_inference_settings_for_create},
    timeline_convert::{
        catchup_query, channel_matches, initial_query, query_from_proto, timeline_event_to_proto,
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
        SubmitUserMessageResponse, TimelineTruncation, UpdateSessionAgentRequest,
        UpdateSessionAgentResponse, UpdateSessionInferenceSettingsRequest,
        UpdateSessionInferenceSettingsResponse, WatchSessionsRequest, WatchSessionsResponse,
        WatchTimelineRequest, WatchTimelineResponse, WipeSessionsRequest, WipeSessionsResponse,
        rollback_target, search_session_templates_response,
        session_service_server::SessionService as GeneratedSessionService, watch_sessions_response,
        watch_timeline_response,
    },
};
use waltr_agent::{
    AgentDefinitionCatalog, AgentSession, AgentSessionActiveRunState, AgentSessionRunError,
    AgentSessionRunSubmission, AgentSessionRuntimeError, AgentSessionRuntimeFactory,
    CredentialStore, ExternalRequestId, McpConnectionKind, McpConnectionResolveRequest,
    McpResolvedConnection, ResolvedAgent, RunConflictPolicy, SessionCoordinator, SessionId,
    SessionListEvent, SessionListUpdate, SessionStore, SessionSummary, SessionTemplateKind,
    SessionTemplateOption, SessionTemplateSearchRequest, SessionTemplateSearchTarget,
    SessionTimelineEvent, TimelineCursor,
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
    agent_catalog: AgentDefinitionCatalog,
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
        Self::from_sessions_with_oauth_policy(
            SessionCoordinator::new(store),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_catalog,
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
        Self::from_sessions_with_oauth_policy(
            sessions,
            config,
            credential_store,
            trusted_mcp_oauth_origin.into_iter().collect(),
            agent_catalog,
        )
    }

    pub(crate) fn from_sessions_with_oauth_policy(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: CredentialStore,
        trusted_mcp_oauth_origins: Vec<reqwest::Url>,
        agent_catalog: AgentDefinitionCatalog,
    ) -> Arc<Self> {
        let default_agent_definition = agent_catalog
            .resolved(agent_catalog.default_agent_name())
            .expect("catalog default agent must be present")
            .clone();
        let runtime_factory = runtime_factory(
            credential_store,
            trusted_mcp_oauth_origins,
            default_agent_definition,
            agent_catalog.clone(),
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
        &self,
        agent_name: &str,
    ) -> Result<waltr_agent::AgentDefinitionBundle, Status> {
        if agent_name.trim().is_empty() {
            return Err(Status::invalid_argument("agent name must not be empty"));
        }
        self.agent_catalog.bundle(agent_name).ok_or_else(|| {
            Status::invalid_argument(format!("agent '{agent_name}' is not selectable"))
        })
    }

    fn summary_to_proto(
        &self,
        summary: SessionSummary,
        config: &Configuration,
    ) -> waltr::grpc::waltr::session::v1::SessionSummary {
        session_summary_to_proto(summary, config, self.agent_catalog.default_agent_name())
    }
}

#[tonic::async_trait]
impl<S> super::SessionHost for SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
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
        Ok(Response::new(ListAgentsResponse {
            agents: self
                .agent_catalog
                .entries()
                .into_iter()
                .map(|agent| SelectableAgent {
                    name: agent.name,
                    description: agent.description,
                })
                .collect(),
            default_agent_name: self.agent_catalog.default_agent_name().to_owned(),
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
        let config = self.load_config().await;
        let inference_settings =
            resolve_inference_settings_for_create(request.inference_settings, &config)?;
        let agent_bundle = match request.agent_selection {
            Some(selection) => self.selected_agent_bundle(&selection.agent_name)?,
            None => self.agent_catalog.default_bundle(),
        };
        let session = self
            .sessions
            .create_with_inference_settings(
                inference_settings,
                Some(agent_bundle),
                BTreeMap::default(),
            )
            .await
            .map_err(agent_error_to_status)?;
        tracing::Span::current().record("session_id", session.record.id.as_str());
        tracing::info!(session_id = %session.record.id.as_str(), "session create completed");

        Ok(Response::new(CreateSessionResponse {
            session: Some(self.summary_to_proto(session.record.summary(), &config)),
        }))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        tracing::trace!("list sessions requested");
        let config = self.load_config().await;
        let sessions = self
            .sessions
            .list()
            .await
            .map_err(agent_error_to_status)?
            .into_iter()
            .map(|summary| self.summary_to_proto(summary, &config))
            .collect();

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

        Ok(Response::new(UpdateSessionInferenceSettingsResponse {
            session: Some(self.summary_to_proto(session.record.summary(), &config)),
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
        self.selected_agent_bundle(&selection.agent_name)?;
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
            session: Some(self.summary_to_proto(session.record.summary(), &config)),
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
        let sessions = self
            .sessions
            .list()
            .await
            .map_err(agent_error_to_status)?
            .into_iter()
            .map(|summary| self.summary_to_proto(summary, &config))
            .collect();
        let snapshot = Ok(WatchSessionsResponse {
            sequence,
            event: Some(watch_sessions_response::Event::Snapshot(
                SessionListSnapshot { sessions },
            )),
        });
        let config = self.config.clone();
        let default_agent_name = self.agent_catalog.default_agent_name().to_owned();
        let stream = tokio_stream::once(snapshot).chain(subscription.then(move |update| {
            let config = config.clone();
            let default_agent_name = default_agent_name.clone();
            async move {
                let current_config = config.load().await;
                let event = match update.event {
                    SessionListEvent::Upsert(session) => watch_sessions_response::Event::Session(
                        session_summary_to_proto(session, &current_config, &default_agent_name),
                    ),
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
        self.sessions
            .load(&session_id)
            .await
            .map_err(agent_error_to_status)?;
        let events = self
            .sessions
            .timeline(&session_id, query_from_proto(request.query))
            .await
            .map_err(agent_error_to_status)?
            .into_iter()
            .map(timeline_event_to_proto)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Response::new(GetTimelineResponse { events }))
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
        tracing::Span::current().record("session_id", session_id.as_str());
        self.sessions
            .load(&session_id)
            .await
            .map_err(agent_error_to_status)?;
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
        tracing::info!("session rollback completed");
        Ok(Response::new(RollbackSessionResponse {
            session: Some(self.summary_to_proto(session.record.summary(), &config)),
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

fn runtime_factory(
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    default_agent_definition: ResolvedAgent,
    agent_catalog: AgentDefinitionCatalog,
    config: ConfigHandle,
) -> AgentSessionRuntimeFactory {
    AgentSessionRuntimeFactory::new(default_root(), credential_store, default_agent_definition)
        .with_agent_catalog(agent_catalog)
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
                McpServerType::Sse => McpConnectionKind::StreamableHttp,
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
    use std::collections::{BTreeSet, HashMap};

    use waltr::grpc::waltr::config::v1::{Configuration, McpServer, McpServerType, Provider};
    use waltr_agent::{
        AgentSessionRuntimeError, McpConnectionKind, McpConnectionResolveRequest,
        definition::McpServerSelection,
    };

    use super::resolve_mcp_connections;

    fn inventory() -> Configuration {
        Configuration {
            mcp_servers: HashMap::from([
                (
                    "zeta".to_owned(),
                    McpServer {
                        r#type: McpServerType::Sse as i32,
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
}
