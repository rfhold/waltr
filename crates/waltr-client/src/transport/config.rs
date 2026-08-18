use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::resource::v1::{
    GetResourceCatalogRequest, ReplaceResourceRequest, ResetResourceRequest, ResourceCatalog,
    WatchResourceCatalogRequest,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::{ClientError, ResourceWrite};

/// Stream returned by resource catalog watch transports.
pub type ConfigWatchStream = Pin<Box<dyn Stream<Item = Result<ResourceCatalog, ClientError>>>>;

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn get_config<T>(transport: &T) -> Result<ResourceCatalog, ClientError>
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
        .resource_client(&credentials)
        .await?
        .get_resource_catalog(GetResourceCatalogRequest {})
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .catalog
        .ok_or_else(|| ClientError::new("resource catalog response missing catalog"))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn write_resource<T>(
    transport: &T,
    write: ResourceWrite,
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
    match write {
        ResourceWrite::Replace {
            path,
            source,
            expected_etag,
        } => {
            let credentials = transport.request_credentials();
            let mut client = transport.resource_client(&credentials).await?;
            client
                .replace_resource(ReplaceResourceRequest {
                    path,
                    source,
                    expected_etag,
                })
                .await
                .map_err(|error| status_error(&error, &credentials))?;
            Ok(())
        }
        ResourceWrite::Reset {
            path,
            expected_etag,
        } => {
            let credentials = transport.request_credentials();
            let mut client = transport.resource_client(&credentials).await?;
            client
                .reset_resource(ResetResourceRequest {
                    path,
                    expected_etag,
                })
                .await
                .map_err(|error| status_error(&error, &credentials))?;
            Ok(())
        }
    }
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn watch_config<T>(transport: &T) -> Result<ConfigWatchStream, ClientError>
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
        .resource_client(&credentials)
        .await?
        .watch_resource_catalog(WatchResourceCatalogRequest {})
        .await
        .map_err(|error| status_error(&error, &credentials))?
        .into_inner()
        .map(move |response| {
            response
                .map_err(|error| status_error(&error, &stream_credentials))
                .and_then(|response| {
                    response.catalog.ok_or_else(|| {
                        ClientError::new("resource catalog watch response missing catalog")
                    })
                })
        });
    Ok(Box::pin(stream))
}
