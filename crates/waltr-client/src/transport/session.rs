use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, ListSessionsRequest, SessionSummary, WatchSessionsRequest,
    WatchSessionsResponse,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::ClientError;

/// Stream returned by session watch transports.
pub type SessionWatchStream = Pin<Box<dyn Stream<Item = Result<SessionWatchUpdate, ClientError>>>>;

/// Internal session watch update from the transport layer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionWatchUpdate {
    /// Monotonic server-side watch sequence.
    pub sequence: u64,
    /// Full changed session summary.
    pub session: SessionSummary,
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
                .map(session_watch_update)
                .map_err(|error| status_error(&error))
        });

    Ok(Box::pin(stream))
}

fn session_watch_update(response: WatchSessionsResponse) -> SessionWatchUpdate {
    SessionWatchUpdate {
        sequence: response.sequence,
        session: response.session.unwrap_or_default(),
    }
}
