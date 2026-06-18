use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, DeleteSessionRequest, ListSessionsRequest, SessionSummary,
    WatchSessionsRequest, WatchSessionsResponse, watch_sessions_response,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::ClientError;

/// Stream returned by session watch transports.
pub type SessionWatchStream = Pin<Box<dyn Stream<Item = Result<SessionWatchUpdate, ClientError>>>>;

/// Internal session watch update from the transport layer.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionWatchUpdate {
    /// Monotonic server-side watch sequence.
    pub sequence: u64,
    /// Changed session payload.
    pub event: SessionWatchEvent,
}

/// Internal session watch event from the transport layer.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionWatchEvent {
    /// Full created or changed session summary.
    Upsert(SessionSummary),
    /// Deleted session id.
    Deleted(String),
}

pub(super) async fn create_session<T>(transport: &T) -> Result<SessionSummary, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let response = transport
        .session_client()
        .await?
        .create_session(CreateSessionRequest::default())
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.session.unwrap_or_default())
}

pub(super) async fn list_sessions<T>(transport: &T) -> Result<Vec<SessionSummary>, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let response = transport
        .session_client()
        .await?
        .list_sessions(ListSessionsRequest::default())
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.sessions)
}

pub(super) async fn delete_session<T>(transport: &T, id: String) -> Result<(), ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    transport
        .session_client()
        .await?
        .delete_session(DeleteSessionRequest { id })
        .await
        .map_err(|error| status_error(&error))?;

    Ok(())
}

pub(super) async fn watch_sessions<T>(transport: &T) -> Result<SessionWatchStream, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let stream = transport
        .session_client()
        .await?
        .watch_sessions(WatchSessionsRequest::default())
        .await
        .map_err(|error| status_error(&error))?
        .into_inner()
        .map(|response| {
            response
                .and_then(session_watch_update)
                .map_err(|error| status_error(&error))
        });

    Ok(Box::pin(stream))
}

fn session_watch_update(
    response: WatchSessionsResponse,
) -> Result<SessionWatchUpdate, tonic::Status> {
    let event = match response.event {
        Some(watch_sessions_response::Event::Session(session)) => {
            SessionWatchEvent::Upsert(session)
        }
        Some(watch_sessions_response::Event::DeletedId(id)) => SessionWatchEvent::Deleted(id),
        None => {
            return Err(tonic::Status::data_loss(
                "session watch response missing event",
            ));
        }
    };

    Ok(SessionWatchUpdate {
        sequence: response.sequence,
        event,
    })
}
