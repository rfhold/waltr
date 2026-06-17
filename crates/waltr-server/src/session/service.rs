//! Implementation of the generated `SessionService`.

use std::{pin::Pin, sync::Arc};

use super::{convert::session_summary_to_proto, error::agent_error_to_status};
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, CreateSessionResponse, ListSessionsRequest, ListSessionsResponse,
    WatchSessionsRequest, WatchSessionsResponse,
    session_service_server::SessionService as GeneratedSessionService,
};
use waltr_agent::{AgentSessions, SessionStore};
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

    async fn watch_sessions(
        &self,
        _request: Request<WatchSessionsRequest>,
    ) -> Result<Response<Self::WatchSessionsStream>, Status> {
        tracing::trace!("watch sessions requested");
        let stream = self.sessions.watch_sessions().map(|update| {
            Ok(WatchSessionsResponse {
                sequence: update.sequence,
                session: Some(session_summary_to_proto(update.session)),
            })
        });

        Ok(Response::new(Box::pin(stream)))
    }
}
