use std::pin::Pin;
use futures_core::Stream;
use futures_util::StreamExt;
use tonic::codegen::{Body, Bytes, StdError};
use waltr::grpc::waltr::session::v1 as wire;
use super::{RemoteTransport, status_error};
use crate::ClientError;

pub type InteractionWatchStream = Pin<Box<dyn Stream<Item = Result<wire::InteractionSnapshot, ClientError>>>>;

pub(super) async fn get<T>(transport: &T, session_id: String) -> Result<wire::InteractionSnapshot, ClientError>
where T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    let response = transport.interaction_client(&credentials).await?.get_interactions(wire::GetInteractionsRequest { session_id }).await.map_err(|e| status_error(&e, &credentials))?.into_inner();
    if !credentials.is_current() { return Err(ClientError::new("Retired interaction credentials")); }
    response.snapshot.ok_or_else(|| ClientError::new("Missing interaction snapshot"))
}

pub(super) async fn watch<T>(transport: &T, session_id: String) -> Result<InteractionWatchStream, ClientError>
where T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    let stream = transport.interaction_client(&credentials).await?.watch_interactions(wire::WatchInteractionsRequest { session_id }).await.map_err(|e| status_error(&e, &credentials))?.into_inner();
    Ok(Box::pin(stream.map(move |result| {
        if !credentials.is_current() { return Err(ClientError::new("Retired interaction credentials")); }
        result.map_err(|e| status_error(&e, &credentials))?.snapshot.ok_or_else(|| ClientError::new("Missing interaction snapshot"))
    })))
}

pub(super) async fn respond<T>(transport: &T, request: wire::RespondInteractionRequest) -> Result<(), ClientError>
where T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    let credentials = transport.request_credentials();
    transport.interaction_client(&credentials).await?.respond_interaction(request).await.map_err(|e| status_error(&e, &credentials))?;
    if !credentials.is_current() { return Err(ClientError::new("Retired interaction credentials")); }
    Ok(())
}
