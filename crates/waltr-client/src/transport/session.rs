use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, DeleteSessionRequest, GetTimelineRequest, InterruptSessionRequest,
    ListSessionsRequest, RespondExternalRequestRequest, SessionRuntimeConfigRequest,
    SessionSummary, SubmitUserMessageRequest, TimelineEvent, TimelineQuery,
    UpdateSessionRuntimeConfigRequest, WatchSessionsRequest, WatchSessionsResponse,
    WatchTimelineRequest, WatchTimelineResponse, watch_sessions_response,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::ClientError;

/// Stream returned by session watch transports.
pub type SessionWatchStream = Pin<Box<dyn Stream<Item = Result<SessionWatchUpdate, ClientError>>>>;

/// Stream returned by timeline watch transports.
pub type SessionTimelineWatchStream =
    Pin<Box<dyn Stream<Item = Result<SessionTimelineWatchUpdate, ClientError>>>>;

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
#[allow(clippy::large_enum_variant)]
pub enum SessionWatchEvent {
    /// Full session list snapshot sent when the watch opens.
    Snapshot(Vec<SessionSummary>),
    /// Full created or changed session summary.
    Upsert(SessionSummary),
    /// Deleted session id.
    Deleted(String),
}

/// Internal timeline watch update from the transport layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTimelineWatchUpdate {
    /// Monotonic server-side watch sequence.
    pub sequence: u64,
    /// Persisted timeline event.
    pub event: TimelineEvent,
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

    response.session.ok_or_else(|| {
        status_error(&tonic::Status::data_loss(
            "create session response missing session",
        ))
    })
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

pub(super) async fn update_session_runtime_config<T>(
    transport: &T,
    id: String,
    runtime_config: SessionRuntimeConfigRequest,
) -> Result<SessionSummary, ClientError>
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
        .update_session_runtime_config(UpdateSessionRuntimeConfigRequest {
            id,
            runtime_config: Some(runtime_config),
        })
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    response.session.ok_or_else(|| {
        status_error(&tonic::Status::data_loss(
            "update session runtime config response missing session",
        ))
    })
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

pub(super) async fn get_timeline<T>(
    transport: &T,
    session_id: String,
    query: Option<TimelineQuery>,
) -> Result<Vec<TimelineEvent>, ClientError>
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
        .get_timeline(GetTimelineRequest { session_id, query })
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.events)
}

pub(super) async fn watch_timeline<T>(
    transport: &T,
    session_id: String,
    after_cursor: Option<u64>,
    channels: Vec<String>,
    initial_limit: Option<u32>,
) -> Result<SessionTimelineWatchStream, ClientError>
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
        .watch_timeline(WatchTimelineRequest {
            session_id,
            after_cursor,
            channels,
            initial_limit,
        })
        .await
        .map_err(|error| status_error(&error))?
        .into_inner()
        .filter_map(|response| async {
            match response {
                Ok(response) if response.event.is_none() => None,
                Ok(response) => {
                    Some(timeline_watch_update(response).map_err(|error| status_error(&error)))
                }
                Err(error) => Some(Err(status_error(&error))),
            }
        });

    Ok(Box::pin(stream))
}

pub(super) async fn submit_user_message<T>(
    transport: &T,
    session_id: String,
    text: String,
) -> Result<(), ClientError>
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
        .submit_user_message(SubmitUserMessageRequest { session_id, text })
        .await
        .map_err(|error| status_error(&error))?;

    Ok(())
}

pub(super) async fn respond_external_request<T>(
    transport: &T,
    session_id: String,
    request_id: String,
    response_json: String,
) -> Result<(), ClientError>
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
        .respond_external_request(RespondExternalRequestRequest {
            session_id,
            request_id,
            response_json,
        })
        .await
        .map_err(|error| status_error(&error))?;

    Ok(())
}

pub(super) async fn interrupt_session<T>(
    transport: &T,
    session_id: String,
) -> Result<(), ClientError>
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
        .interrupt_session(InterruptSessionRequest { session_id })
        .await
        .map_err(|error| status_error(&error))?;

    Ok(())
}

fn session_watch_update(
    response: WatchSessionsResponse,
) -> Result<SessionWatchUpdate, tonic::Status> {
    let event = match response.event {
        Some(watch_sessions_response::Event::Snapshot(snapshot)) => {
            SessionWatchEvent::Snapshot(snapshot.sessions)
        }
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

fn timeline_watch_update(
    response: WatchTimelineResponse,
) -> Result<SessionTimelineWatchUpdate, tonic::Status> {
    let event = response
        .event
        .ok_or_else(|| tonic::Status::data_loss("timeline watch response missing event"))?;

    Ok(SessionTimelineWatchUpdate {
        sequence: response.sequence,
        event,
    })
}
