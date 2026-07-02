//! Implementation of the generated `SessionService`.

use std::{path::PathBuf, pin::Pin, sync::Arc};

use crate::config::ConfigHandle;

use super::{
    convert::session_summary_to_proto,
    error::{agent_error_to_status, session_run_error_to_status},
    runtime_config::{resolve_runtime_config, resolve_runtime_config_for_create},
    timeline_convert::{
        catchup_query, channel_matches, initial_query, query_from_proto, timeline_event_to_proto,
    },
};
use waltr::grpc::waltr::{
    config::v1::Configuration,
    session::v1::{
        CreateSessionRequest, CreateSessionResponse, DeleteSessionRequest, DeleteSessionResponse,
        GetTimelineRequest, GetTimelineResponse, InterruptSessionRequest, InterruptSessionResponse,
        ListSessionsRequest, ListSessionsResponse, RespondExternalRequestRequest,
        RespondExternalRequestResponse, SessionListSnapshot, SubmitUserMessageRequest,
        SubmitUserMessageResponse, UpdateSessionRuntimeConfigRequest,
        UpdateSessionRuntimeConfigResponse, WatchSessionsRequest, WatchSessionsResponse,
        WatchTimelineRequest, WatchTimelineResponse,
        session_service_server::SessionService as GeneratedSessionService, watch_sessions_response,
    },
};
use waltr_agent::{
    AgentSessionRuntimeFactory, ExternalRequestId, FileCredentialStore, RunConflictPolicy,
    SessionCoordinator, SessionId, SessionListEvent, SessionStore,
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
}

impl<S> SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a session service from a store.
    #[must_use]
    pub fn new(store: S) -> Arc<Self> {
        Self::from_sessions(SessionCoordinator::new(store))
    }

    /// Create a session service from an existing session coordinator.
    #[must_use]
    pub fn from_sessions(sessions: SessionCoordinator<S>) -> Arc<Self> {
        let credential_store = FileCredentialStore::from_env();
        let config = ConfigHandle::default();
        let sessions = sessions.with_runtime_factory(AgentSessionRuntimeFactory::new(
            default_root(),
            credential_store,
        ));
        Arc::new(Self { sessions, config })
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
        credential_store: FileCredentialStore,
    ) -> Arc<Self> {
        Self::from_sessions_with_config(SessionCoordinator::new(store), config, credential_store)
    }

    /// Create a session service from an existing session coordinator and config handle.
    #[must_use]
    pub fn from_sessions_with_config(
        sessions: SessionCoordinator<S>,
        config: ConfigHandle,
        credential_store: FileCredentialStore,
    ) -> Arc<Self> {
        let sessions = sessions.with_runtime_factory(AgentSessionRuntimeFactory::new(
            default_root(),
            credential_store,
        ));
        Arc::new(Self { sessions, config })
    }

    async fn load_config(&self) -> Configuration {
        self.config.load().await
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
        let runtime_config = resolve_runtime_config_for_create(request.runtime_config, &config)?;
        let session = self
            .sessions
            .create_with_runtime_settings(runtime_config, Default::default())
            .await
            .map_err(agent_error_to_status)?;
        tracing::Span::current().record("session_id", session.record.id.as_str());
        tracing::info!(session_id = %session.record.id.as_str(), "session create completed");

        Ok(Response::new(CreateSessionResponse {
            session: Some(session_summary_to_proto(session.record.summary(), &config)),
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
            .map(|summary| session_summary_to_proto(summary, &config))
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
        fields(operation = "update_session_runtime_config", session_id)
    )]
    async fn update_session_runtime_config(
        &self,
        request: Request<UpdateSessionRuntimeConfigRequest>,
    ) -> Result<Response<UpdateSessionRuntimeConfigResponse>, Status> {
        let request = request.into_inner();
        if request.id.trim().is_empty() {
            return Err(Status::invalid_argument("session id must not be empty"));
        }
        tracing::Span::current().record("session_id", request.id.as_str());
        tracing::info!("session runtime config update accepted");
        let runtime_config = request
            .runtime_config
            .ok_or_else(|| Status::invalid_argument("runtime config is required"))?;
        let config = self.load_config().await;
        let runtime_config = resolve_runtime_config(runtime_config, &config)?;
        let session = self
            .sessions
            .set_runtime_settings(&SessionId::new(request.id), Some(runtime_config))
            .await
            .map_err(agent_error_to_status)?;
        tracing::info!("session runtime config update completed");

        Ok(Response::new(UpdateSessionRuntimeConfigResponse {
            session: Some(session_summary_to_proto(session.record.summary(), &config)),
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
            .map(|summary| session_summary_to_proto(summary, &config))
            .collect();
        let snapshot = Ok(WatchSessionsResponse {
            sequence,
            event: Some(watch_sessions_response::Event::Snapshot(
                SessionListSnapshot { sessions },
            )),
        });
        let config = self.config.clone();
        let stream = tokio_stream::once(snapshot).chain(subscription.then(move |update| {
            let config = config.clone();
            async move {
                let current_config = config.load().await;
                let event = match update.event {
                    SessionListEvent::Upsert(session) => watch_sessions_response::Event::Session(
                        session_summary_to_proto(session, &current_config),
                    ),
                    SessionListEvent::Deleted(id) => {
                        watch_sessions_response::Event::DeletedId(id.as_str().to_owned())
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
            event: None,
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
            let cursor = update.event.cursor.get();
            if cursor <= highest_cursor.load(Ordering::Relaxed)
                || !channel_matches(&update.event, &channels)
            {
                return None;
            }
            highest_cursor.fetch_max(cursor, Ordering::Relaxed);
            tracing::trace!(
                sequence = update.sequence,
                cursor,
                kind = ?update.event.kind,
                "watch timeline live event delivered"
            );
            Some(
                timeline_event_to_proto(update.event).map(|event| WatchTimelineResponse {
                    sequence: update.sequence,
                    event: Some(event),
                }),
            )
        });
        let stream = tokio_stream::iter(historical).chain(live);

        tracing::trace!(session_id = %session_id.as_str(), sequence = initial_sequence, "watch timeline requested");
        Ok(Response::new(Box::pin(stream)))
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
}

fn required_session_id(value: &str) -> Result<SessionId, Status> {
    if value.trim().is_empty() {
        Err(Status::invalid_argument("session id must not be empty"))
    } else {
        Ok(SessionId::new(value.to_owned()))
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
                event: Some(event),
            }),
        );
    }
}

fn default_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
