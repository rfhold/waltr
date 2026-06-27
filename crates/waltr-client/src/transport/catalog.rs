use waltr::grpc::waltr::catalog::v1::{
    ApplyCatalogModelDefaultsRequest, GetModelCatalogDriftRequest, GetModelCatalogDriftResponse,
    SearchCatalogModelsRequest, SearchCatalogModelsResponse, SetModelCatalogMatchRequest,
    SuggestCatalogMatchesRequest, SuggestCatalogMatchesResponse,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::{ClientError, Configuration};

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
    let response = transport
        .catalog_client()
        .await?
        .search_catalog_models(request)
        .await
        .map_err(|error| status_error(&error))?;

    Ok(response.into_inner())
}

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
    let response = transport
        .catalog_client()
        .await?
        .suggest_catalog_matches(request)
        .await
        .map_err(|error| status_error(&error))?;

    Ok(response.into_inner())
}

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
    let response = transport
        .catalog_client()
        .await?
        .get_model_catalog_drift(request)
        .await
        .map_err(|error| status_error(&error))?;

    Ok(response.into_inner())
}

pub(super) async fn set_model_catalog_match<T>(
    transport: &T,
    request: SetModelCatalogMatchRequest,
) -> Result<Configuration, ClientError>
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
        .catalog_client()
        .await?
        .set_model_catalog_match(request)
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.config.unwrap_or_default())
}

pub(super) async fn apply_catalog_model_defaults<T>(
    transport: &T,
    request: ApplyCatalogModelDefaultsRequest,
) -> Result<Configuration, ClientError>
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
        .catalog_client()
        .await?
        .apply_catalog_model_defaults(request)
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.config.unwrap_or_default())
}
