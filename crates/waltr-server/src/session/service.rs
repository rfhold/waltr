//! Implementation of the generated `SessionService`.

use std::{pin::Pin, sync::Arc};

use super::{convert::session_summary_to_proto, error::agent_error_to_status};
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, CreateSessionResponse, DeleteSessionRequest, DeleteSessionResponse,
    ListSessionsRequest, ListSessionsResponse, WatchSessionsRequest, WatchSessionsResponse,
    session_service_server::SessionService as GeneratedSessionService, watch_sessions_response,
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
        Arc::new(Self { sessions })
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
        _request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        tracing::info!("create session requested");
        let session = self
            .sessions
            .create()
            .await
            .map_err(agent_error_to_status)?;

        Ok(Response::new(CreateSessionResponse {
            session: Some(session_summary_to_proto(session.record.summary())),
        }))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        tracing::trace!("list sessions requested");
        let sessions = self
            .sessions
            .list()
            .await
            .map_err(agent_error_to_status)?
            .into_iter()
            .map(session_summary_to_proto)
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

    async fn watch_sessions(
        &self,
        _request: Request<WatchSessionsRequest>,
    ) -> Result<Response<Self::WatchSessionsStream>, Status> {
        tracing::trace!("watch sessions requested");
        let stream = self.sessions.watch_sessions().map(|update| {
            let event = match update.event {
                SessionListEvent::Upsert(session) => {
                    watch_sessions_response::Event::Session(session_summary_to_proto(session))
                }
                SessionListEvent::Deleted(id) => {
                    watch_sessions_response::Event::DeletedId(id.as_str().to_owned())
                }
            };

            Ok(WatchSessionsResponse {
                sequence: update.sequence,
                event: Some(event),
            })
        });

        Ok(Response::new(Box::pin(stream)))
    }
}
