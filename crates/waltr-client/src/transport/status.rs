use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::status::v1::{SystemStatus, WatchStatusRequest, WatchStatusResponse};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::ClientError;

/// Stream returned by status watch transports.
pub type StatusWatchStream = Pin<Box<dyn Stream<Item = Result<SystemStatus, ClientError>>>>;

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn watch_status<T>(transport: &T) -> Result<StatusWatchStream, ClientError>
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
        .status_client(&credentials)
        .await?
        .watch_status(WatchStatusRequest::default())
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .map(move |response| {
            response
                .map(status_watch_update)
                .map_err(|error| status_error(&error, &stream_credentials))
        });

    Ok(Box::pin(stream))
}

fn status_watch_update(response: WatchStatusResponse) -> SystemStatus {
    response.status.unwrap_or_default()
}
