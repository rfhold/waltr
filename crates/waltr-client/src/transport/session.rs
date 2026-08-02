use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::session::v1::{
    ContinueSessionRequest, CreateSessionRequest, DeleteSessionRequest, GetTimelineRequest,
    InterruptSessionRequest, ListSessionsRequest, RespondExternalRequestRequest,
    RollbackSessionRequest, RollbackTarget, SearchSessionTemplatesRequest,
    SearchSessionTemplatesResponse, SessionRuntimeConfigRequest, SessionSummary,
    SubmitUserMessageRequest, TimelineEvent, TimelineQuery, TimelineTruncation,
    UpdateSessionRuntimeConfigRequest, WatchSessionsRequest, WatchSessionsResponse,
    WatchTimelineRequest, WatchTimelineResponse, WipeSessionsRequest, watch_sessions_response,
    watch_timeline_response,
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
    /// Every session was deleted.
    Cleared,
}

/// Internal timeline watch update from the transport layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTimelineWatchUpdate {
    /// Monotonic server-side watch sequence.
    pub sequence: u64,
    /// Timeline watch update payload.
    pub event: SessionTimelineWatchEvent,
}

/// Internal timeline watch event from the transport layer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum SessionTimelineWatchEvent {
    /// Persisted timeline event.
    Event(TimelineEvent),
    /// Visible timeline events after this cursor were removed.
    Truncated(TimelineTruncation),
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    let response = transport
        .session_client(&credentials)
        .await?
        .create_session(CreateSessionRequest::default())
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();

    response.session.ok_or_else(|| {
        status_error(
            &tonic::Status::data_loss("create session response missing session"),
            &credentials,
        )
    })
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    let response = transport
        .session_client(&credentials)
        .await?
        .list_sessions(ListSessionsRequest::default())
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();

    Ok(response.sessions)
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    transport
        .session_client(&credentials)
        .await?
        .delete_session(DeleteSessionRequest { id })
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn wipe_sessions<T>(transport: &T) -> Result<(), ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    transport
        .session_client(&credentials)
        .await?
        .wipe_sessions(WipeSessionsRequest { confirm: true })
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    let response = transport
        .session_client(&credentials)
        .await?
        .update_session_runtime_config(UpdateSessionRuntimeConfigRequest {
            id,
            runtime_config: Some(runtime_config),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();

    response.session.ok_or_else(|| {
        status_error(
            &tonic::Status::data_loss("update session runtime config response missing session"),
            &credentials,
        )
    })
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    let stream_credentials = credentials.clone();
    let stream = transport
        .session_client(&credentials)
        .await?
        .watch_sessions(WatchSessionsRequest::default())
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .map(move |response| {
            response
                .and_then(session_watch_update)
                .map_err(|error| status_error(&error, &stream_credentials))
        });

    Ok(Box::pin(stream))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    let response = transport
        .session_client(&credentials)
        .await?
        .get_timeline(GetTimelineRequest { session_id, query })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();

    Ok(response.events)
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    let stream_credentials = credentials.clone();
    let stream = transport
        .session_client(&credentials)
        .await?
        .watch_timeline(WatchTimelineRequest {
            session_id,
            after_cursor,
            channels,
            initial_limit,
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .filter_map(move |response| {
            let stream_credentials = stream_credentials.clone();
            async move {
                match response {
                    Ok(response) if response.update.is_none() => None,
                    Ok(response) => Some(
                        timeline_watch_update(response)
                            .map_err(|error| status_error(&error, &stream_credentials)),
                    ),
                    Err(error) => Some(Err(status_error(&error, &stream_credentials))),
                }
            }
        });

    Ok(Box::pin(stream))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn search_session_templates<T>(
    transport: &T,
    request: SearchSessionTemplatesRequest,
) -> Result<SearchSessionTemplatesResponse, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    Ok(transport
        .session_client(&credentials)
        .await?
        .search_session_templates(request)
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn rollback_session<T>(
    transport: &T,
    session_id: String,
    target: RollbackTarget,
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
    let credentials = transport.request_credentials();
    let response = transport
        .session_client(&credentials)
        .await?
        .rollback_session(RollbackSessionRequest {
            session_id,
            target: Some(target),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner();

    response.session.ok_or_else(|| {
        status_error(
            &tonic::Status::data_loss("rollback session response missing session"),
            &credentials,
        )
    })
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn continue_session<T>(
    transport: &T,
    session_id: String,
    target: RollbackTarget,
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
    let credentials = transport.request_credentials();
    transport
        .session_client(&credentials)
        .await?
        .continue_session(ContinueSessionRequest {
            session_id,
            target: Some(target),
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    transport
        .session_client(&credentials)
        .await?
        .submit_user_message(SubmitUserMessageRequest { session_id, text })
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    transport
        .session_client(&credentials)
        .await?
        .respond_external_request(RespondExternalRequestRequest {
            session_id,
            request_id,
            response_json,
        })
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
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
    let credentials = transport.request_credentials();
    transport
        .session_client(&credentials)
        .await?
        .interrupt_session(InterruptSessionRequest { session_id })
        .await
        .map_err(|error| status_error(&error, &credentials))?;

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
        Some(watch_sessions_response::Event::Cleared(_)) => SessionWatchEvent::Cleared,
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
    let event = match response.update {
        Some(watch_timeline_response::Update::Event(event)) => {
            SessionTimelineWatchEvent::Event(event)
        }
        Some(watch_timeline_response::Update::Truncation(truncation)) => {
            SessionTimelineWatchEvent::Truncated(truncation)
        }
        None => {
            return Err(tonic::Status::data_loss(
                "timeline watch response missing update",
            ));
        }
    };

    Ok(SessionTimelineWatchUpdate {
        sequence: response.sequence,
        event,
    })
}
