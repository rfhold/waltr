//! Implementation of the generated `SessionService`.

use std::{collections::BTreeMap, pin::Pin, sync::Arc};

use crate::config::ConfigHandle;

use super::{
    convert::session_summary_to_proto,
    error::agent_error_to_status,
    runtime_config::{
        SESSION_RUNTIME_CONFIG_METADATA_KEY, resolve_runtime_config,
        resolve_runtime_config_for_create, runtime_config_to_metadata_value,
        runtime_metadata_values,
    },
};
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, CreateSessionResponse, DeleteSessionRequest, DeleteSessionResponse,
    ListSessionsRequest, ListSessionsResponse, SessionListSnapshot,
    UpdateSessionRuntimeConfigRequest, UpdateSessionRuntimeConfigResponse, WatchSessionsRequest,
    WatchSessionsResponse, session_service_server::SessionService as GeneratedSessionService,
    watch_sessions_response,
};
use waltr_agent::{AgentSessions, SessionId, SessionListEvent, SessionStore};
use tokio_stream::{Stream, StreamExt as _};
use tonic::{Request, Response, Status};

/// gRPC session service backed by durable agent session storage.
#[derive(Debug)]
pub struct SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    sessions: AgentSessions<S>,
    config: ConfigHandle,
}

impl<S> SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a session service from a store.
    #[must_use]
    pub fn new(store: S) -> Arc<Self> {
        Self::from_sessions(AgentSessions::new(store))
    }

    /// Create a session service from an existing session coordinator.
    #[must_use]
    pub fn from_sessions(sessions: AgentSessions<S>) -> Arc<Self> {
        Arc::new(Self {
            sessions,
            config: ConfigHandle::default(),
        })
    }
}

impl<S> SessionService<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a session service from a store and config handle.
    #[must_use]
    pub fn new_with_config(store: S, config: ConfigHandle) -> Arc<Self> {
        Self::from_sessions_with_config(AgentSessions::new(store), config)
    }

    /// Create a session service from an existing session coordinator and config handle.
    #[must_use]
    pub fn from_sessions_with_config(
        sessions: AgentSessions<S>,
        config: ConfigHandle,
    ) -> Arc<Self> {
        Arc::new(Self { sessions, config })
    }

    async fn load_config(&self) -> waltr::grpc::waltr::config::v1::Configuration {
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

    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        let request = request.into_inner();
        tracing::info!("create session requested");
        let config = self.load_config().await;
        let runtime_config = resolve_runtime_config_for_create(request.runtime_config, &config)?;
        let metadata_values = runtime_config
            .as_ref()
            .map(runtime_metadata_values)
            .transpose()?
            .unwrap_or_else(BTreeMap::new);
        let session = self
            .sessions
            .create_with_metadata_values(metadata_values)
            .await
            .map_err(agent_error_to_status)?;

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

    async fn delete_session(
        &self,
        request: Request<DeleteSessionRequest>,
    ) -> Result<Response<DeleteSessionResponse>, Status> {
        let id = request.into_inner().id;
        if id.trim().is_empty() {
            return Err(Status::invalid_argument("session id must not be empty"));
        }

        tracing::info!(session_id = %id, "delete session requested");
        let id = SessionId::new(id);
        self.sessions
            .delete(&id)
            .await
            .map_err(agent_error_to_status)?;

        Ok(Response::new(DeleteSessionResponse {}))
    }

    async fn update_session_runtime_config(
        &self,
        request: Request<UpdateSessionRuntimeConfigRequest>,
    ) -> Result<Response<UpdateSessionRuntimeConfigResponse>, Status> {
        let request = request.into_inner();
        if request.id.trim().is_empty() {
            return Err(Status::invalid_argument("session id must not be empty"));
        }
        let runtime_config = request
            .runtime_config
            .ok_or_else(|| Status::invalid_argument("runtime config is required"))?;
        let config = self.load_config().await;
        let runtime_config = resolve_runtime_config(runtime_config, &config)?;
        let metadata_value = runtime_config_to_metadata_value(&runtime_config)?;
        let session = self
            .sessions
            .set_metadata_value(
                &SessionId::new(request.id),
                SESSION_RUNTIME_CONFIG_METADATA_KEY,
                metadata_value,
            )
            .await
            .map_err(agent_error_to_status)?;

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
}
