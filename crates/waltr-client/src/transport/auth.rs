use waltr::grpc::waltr::auth::v1::{
    AuthMethod, AuthTarget, BeginAuthRequest, CancelAuthRequest, CompleteAuthRequest,
    GetAuthAttemptStatusRequest, ListAuthStatusesRequest, LogoutRequest, RefreshAuthRequest,
};
use tonic::codegen::{Body, Bytes, StdError};

use crate::{ClientError, transport::RemoteTransport};

pub use waltr::grpc::waltr::auth::v1::{
    AuthStatus, BeginAuthResponse as GenericAuthAttempt,
    GetAuthAttemptStatusResponse as GenericAuthAttemptStatus,
};

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn list_auth_statuses<T>(transport: &T) -> Result<Vec<AuthStatus>, ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    let response = client
        .list_auth_statuses(ListAuthStatusesRequest {})
        .await
        .map_err(|error| super::status_error(&error, &credentials))?;
    Ok(response.into_inner().statuses)
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn get_auth_attempt_status<T>(
    transport: &T,
    attempt_id: String,
) -> Result<GenericAuthAttemptStatus, ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    client
        .get_auth_attempt_status(GetAuthAttemptStatusRequest { attempt_id })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|error| super::status_error(&error, &credentials))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn begin_auth<T>(
    transport: &T,
    target: AuthTarget,
) -> Result<GenericAuthAttempt, ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    let response = client
        .begin_auth(BeginAuthRequest {
            target: Some(target),
            method: AuthMethod::OauthManual as i32,
        })
        .await
        .map_err(|error| super::status_error(&error, &credentials))?;
    Ok(response.into_inner())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn complete_auth<T>(
    transport: &T,
    attempt_id: String,
    input: String,
) -> Result<AuthStatus, ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    let response = client
        .complete_auth(CompleteAuthRequest { attempt_id, input })
        .await
        .map_err(|error| super::status_error(&error, &credentials))?;
    response
        .into_inner()
        .status
        .ok_or_else(|| ClientError::new("auth response did not include status"))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn cancel_auth<T>(transport: &T, attempt_id: String) -> Result<(), ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    client
        .cancel_auth(CancelAuthRequest { attempt_id })
        .await
        .map_err(|error| super::status_error(&error, &credentials))?;
    Ok(())
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn logout<T>(transport: &T, target: AuthTarget) -> Result<AuthStatus, ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    let response = client
        .logout(LogoutRequest {
            target: Some(target),
        })
        .await
        .map_err(|error| super::status_error(&error, &credentials))?;
    response
        .into_inner()
        .status
        .ok_or_else(|| ClientError::new("auth response did not include status"))
}

#[allow(
    clippy::future_not_send,
    reason = "browser RPC futures are locally polled"
)]
pub(super) async fn refresh_auth<T>(
    transport: &T,
    target: AuthTarget,
) -> Result<AuthStatus, ClientError>
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
    let mut client = transport.auth_client(&credentials).await?;
    let response = client
        .refresh_auth(RefreshAuthRequest {
            target: Some(target),
        })
        .await
        .map_err(|error| super::status_error(&error, &credentials))?;
    response
        .into_inner()
        .status
        .ok_or_else(|| ClientError::new("auth response did not include status"))
}
