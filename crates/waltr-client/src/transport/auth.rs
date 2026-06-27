use waltr::grpc::waltr::auth::v1::{
    BeginProviderAuthRequest, CancelProviderAuthRequest, CompleteProviderAuthRequest,
    ListProviderAuthStatusesRequest, LogoutProviderRequest, ProviderAuthMethod,
};
use tonic::codegen::{Body, Bytes, StdError};

use crate::{ClientError, transport::RemoteTransport};

pub use waltr::grpc::waltr::auth::v1::{
    BeginProviderAuthResponse as AuthAttempt, ProviderAuthStatus,
};

pub(super) async fn list_provider_auth_statuses<T>(
    transport: &T,
) -> Result<Vec<ProviderAuthStatus>, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let mut client = transport.auth_client().await?;
    let response = client
        .list_provider_auth_statuses(ListProviderAuthStatusesRequest {})
        .await
        .map_err(|error| super::status_error(&error))?;
    Ok(response.into_inner().statuses)
}

pub(super) async fn begin_provider_auth<T>(
    transport: &T,
    provider_key: String,
) -> Result<AuthAttempt, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let mut client = transport.auth_client().await?;
    let response = client
        .begin_provider_auth(BeginProviderAuthRequest {
            provider_key,
            method: ProviderAuthMethod::OauthManual as i32,
        })
        .await
        .map_err(|error| super::status_error(&error))?;
    Ok(response.into_inner())
}

pub(super) async fn complete_provider_auth<T>(
    transport: &T,
    attempt_id: String,
    input: String,
) -> Result<ProviderAuthStatus, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let mut client = transport.auth_client().await?;
    let response = client
        .complete_provider_auth(CompleteProviderAuthRequest { attempt_id, input })
        .await
        .map_err(|error| super::status_error(&error))?;
    response
        .into_inner()
        .status
        .ok_or_else(|| ClientError::new("auth response did not include status"))
}

pub(super) async fn cancel_provider_auth<T>(
    transport: &T,
    attempt_id: String,
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
    let mut client = transport.auth_client().await?;
    client
        .cancel_provider_auth(CancelProviderAuthRequest { attempt_id })
        .await
        .map_err(|error| super::status_error(&error))?;
    Ok(())
}

pub(super) async fn logout_provider<T>(
    transport: &T,
    provider_key: String,
) -> Result<ProviderAuthStatus, ClientError>
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let mut client = transport.auth_client().await?;
    let response = client
        .logout_provider(LogoutProviderRequest { provider_key })
        .await
        .map_err(|error| super::status_error(&error))?;
    response
        .into_inner()
        .status
        .ok_or_else(|| ClientError::new("auth response did not include status"))
}
