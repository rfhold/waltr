use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::config::ConfigWatchUpdate;
use waltr::grpc::waltr::config::v1::{
    GetConfigRequest, McpServerMapPatch, PatchConfigRequest, ProviderMapPatch, WatchConfigRequest,
    WatchConfigResponse,
};
use tonic::codegen::{Body, Bytes, StdError};

use super::{RemoteTransport, status_error};
use crate::{ClientError, ConfigPatch, Configuration};

/// Stream returned by configuration watch transports.
pub type ConfigWatchStream = Pin<Box<dyn Stream<Item = Result<ConfigWatchUpdate, ClientError>>>>;

pub(super) async fn get_config<T>(transport: &T) -> Result<Configuration, ClientError>
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
        .config_client()
        .await?
        .get_config(GetConfigRequest::default())
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.config.unwrap_or_default())
}

pub(super) async fn patch_config<T>(
    transport: &T,
    patch: ConfigPatch,
    expected_etag: String,
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
        .config_client()
        .await?
        .patch_config(PatchConfigRequest {
            expected_etag,
            providers: patch
                .providers
                .map(|providers| ProviderMapPatch { providers }),
            mcp_servers: patch
                .mcp_servers
                .map(|mcp_servers| McpServerMapPatch { mcp_servers }),
            runtime_defaults: patch.runtime_defaults,
        })
        .await
        .map_err(|error| status_error(&error))?
        .into_inner();

    Ok(response.config.unwrap_or_default())
}

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
    let stream = transport
        .config_client()
        .await?
        .watch_config(WatchConfigRequest::default())
        .await
        .map_err(|error| status_error(&error))?
        .into_inner()
        .map(|response| {
            response
                .map(config_watch_update)
                .map_err(|error| status_error(&error))
        });

    Ok(Box::pin(stream))
}

fn config_watch_update(response: WatchConfigResponse) -> ConfigWatchUpdate {
    ConfigWatchUpdate {
        previous_etag: response.previous_etag,
        etag: response.etag,
        changed_keys: response.changed_keys,
        error: response.error,
        config: response.config,
    }
}
