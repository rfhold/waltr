use waltr::grpc::waltr::catalog::v1::{
    GetModelCatalogDriftRequest, GetModelCatalogDriftResponse, SearchCatalogModelsRequest,
    SearchCatalogModelsResponse, SuggestCatalogMatchesRequest, SuggestCatalogMatchesResponse,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::ClientError;

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn search_catalog_models<T>(
    transport: &T,
    request: SearchCatalogModelsRequest,
) -> Result<SearchCatalogModelsResponse, ClientError>
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
        .catalog_client(&credentials)
        .await?
        .search_catalog_models(request)
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(response.into_inner())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn suggest_catalog_matches<T>(
    transport: &T,
    request: SuggestCatalogMatchesRequest,
) -> Result<SuggestCatalogMatchesResponse, ClientError>
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
        .catalog_client(&credentials)
        .await?
        .suggest_catalog_matches(request)
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(response.into_inner())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn get_model_catalog_drift<T>(
    transport: &T,
    request: GetModelCatalogDriftRequest,
) -> Result<GetModelCatalogDriftResponse, ClientError>
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
        .catalog_client(&credentials)
        .await?
        .get_model_catalog_drift(request)
        .await
        .map_err(|error| status_error(&error, &credentials))?;

    Ok(response.into_inner())
}
