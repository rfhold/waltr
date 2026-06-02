//! Transport boundary for HTTP, browser, embedded, and test clients.

use std::pin::Pin;

use futures_core::Stream;
use futures_util::StreamExt;
use waltr::grpc::waltr::config::v1::{
    GetConfigRequest, McpServerMapPatch, PatchConfigRequest, ProviderMapPatch, WatchConfigRequest,
    WatchConfigResponse, config_service_client::ConfigServiceClient,
};
use waltr::grpc::waltr::status::v1::{
    SystemStatus, WatchStatusRequest, WatchStatusResponse,
    status_service_client::StatusServiceClient,
};
use tonic::Code;
use tonic::codegen::{Body, Bytes, StdError};

use crate::{ClientError, ConfigPatch, Configuration};

/// Stream returned by configuration watch transports.
pub type ConfigWatchStream = Pin<Box<dyn Stream<Item = Result<ConfigWatchUpdate, ClientError>>>>;

/// Internal configuration watch update from the transport layer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigWatchUpdate {
    /// Previous entity tag, if supplied by the server.
    pub previous_etag: String,
    /// New entity tag for the configuration.
    pub etag: String,
    /// Changed configuration keys reported by the server.
    pub changed_keys: Vec<String>,
    /// Optional watch error message.
    pub error: String,
    /// Full configuration snapshot, if supplied by the server.
    pub config: Option<Configuration>,
}

/// Stream returned by status watch transports.
pub type StatusWatchStream = Pin<Box<dyn Stream<Item = Result<SystemStatus, ClientError>>>>;

#[cfg(not(target_arch = "wasm32"))]
pub type DefaultGrpcTransport = NativeGrpcTransport;

#[cfg(target_arch = "wasm32")]
pub type DefaultGrpcTransport = WasmGrpcTransport;

/// Runtime-neutral transport adapter used by the shared effect executor.
pub trait WaltrTransport {
    /// Stream type returned by configuration watch subscriptions.
    type ConfigWatchStream: Stream<Item = Result<ConfigWatchUpdate, ClientError>>;

    /// Load the current configuration.
    fn get_config(&self) -> impl Future<Output = Result<Configuration, ClientError>>;

    /// Patch the current configuration.
    fn patch_config(
        &self,
        patch: ConfigPatch,
        expected_etag: String,
    ) -> impl Future<Output = Result<Configuration, ClientError>>;

    /// Subscribe to future configuration changes.
    fn watch_config(&self) -> impl Future<Output = Result<Self::ConfigWatchStream, ClientError>>;

    /// Subscribe to future system status changes.
    fn watch_status(&self) -> impl Future<Output = Result<StatusWatchStream, ClientError>>;
}

trait RemoteTransport {
    type Inner;

    fn config_client(
        &self,
    ) -> impl Future<Output = Result<ConfigServiceClient<Self::Inner>, ClientError>>;

    fn status_client(
        &self,
    ) -> impl Future<Output = Result<StatusServiceClient<Self::Inner>, ClientError>>;
}

impl<T> WaltrTransport for T
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    type ConfigWatchStream = ConfigWatchStream;

    async fn get_config(&self) -> Result<Configuration, ClientError> {
        let response = self
            .config_client()
            .await?
            .get_config(GetConfigRequest::default())
            .await
            .map_err(|error| status_error(&error))?
            .into_inner();

        Ok(response.config.unwrap_or_default())
    }

    async fn patch_config(
        &self,
        patch: ConfigPatch,
        expected_etag: String,
    ) -> Result<Configuration, ClientError> {
        let response = self
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
            })
            .await
            .map_err(|error| status_error(&error))?
            .into_inner();

        Ok(response.config.unwrap_or_default())
    }

    async fn watch_config(&self) -> Result<Self::ConfigWatchStream, ClientError> {
        let stream = self
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

    async fn watch_status(&self) -> Result<StatusWatchStream, ClientError> {
        let stream = self
            .status_client()
            .await?
            .watch_status(WatchStatusRequest::default())
            .await
            .map_err(|error| status_error(&error))?
            .into_inner()
            .map(|response| {
                response
                    .map(status_watch_update)
                    .map_err(|error| status_error(&error))
            });

        Ok(Box::pin(stream))
    }
}

/// Native gRPC transport backed by tonic channels.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct NativeGrpcTransport {
    endpoint: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeGrpcTransport {
    /// Create a native gRPC transport for an endpoint base URL.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl RemoteTransport for NativeGrpcTransport {
    type Inner = tonic::transport::Channel;

    async fn config_client(&self) -> Result<ConfigServiceClient<Self::Inner>, ClientError> {
        let channel = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|error| ClientError::new(error.to_string()))?
            .connect()
            .await
            .map_err(|error| ClientError::new(error.to_string()))?;
        Ok(ConfigServiceClient::new(channel))
    }

    async fn status_client(&self) -> Result<StatusServiceClient<Self::Inner>, ClientError> {
        let channel = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|error| ClientError::new(error.to_string()))?
            .connect()
            .await
            .map_err(|error| ClientError::new(error.to_string()))?;
        Ok(StatusServiceClient::new(channel))
    }
}

/// Browser gRPC-web transport backed by `fetch`.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug, Default)]
pub struct WasmGrpcTransport {
    api_base: String,
}

#[cfg(target_arch = "wasm32")]
impl WasmGrpcTransport {
    /// Create a browser gRPC-web transport.
    #[must_use]
    pub fn new(api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl RemoteTransport for WasmGrpcTransport {
    type Inner = tonic_web_wasm_client::Client;

    async fn config_client(&self) -> Result<ConfigServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(ConfigServiceClient::new(transport))
    }

    async fn status_client(&self) -> Result<StatusServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(StatusServiceClient::new(transport))
    }
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

fn status_watch_update(response: WatchStatusResponse) -> SystemStatus {
    response.status.unwrap_or_default()
}

fn status_error(error: &tonic::Status) -> ClientError {
    ClientError::new(format!(
        "{}: {}",
        status_code_name(error.code()),
        error.message()
    ))
}

const fn status_code_name(code: Code) -> &'static str {
    match code {
        Code::Ok => "ok",
        Code::Cancelled => "cancelled",
        Code::Unknown => "unknown",
        Code::InvalidArgument => "invalid_argument",
        Code::DeadlineExceeded => "deadline_exceeded",
        Code::NotFound => "not_found",
        Code::AlreadyExists => "already_exists",
        Code::PermissionDenied => "permission_denied",
        Code::ResourceExhausted => "resource_exhausted",
        Code::FailedPrecondition => "failed_precondition",
        Code::Aborted => "aborted",
        Code::OutOfRange => "out_of_range",
        Code::Unimplemented => "unimplemented",
        Code::Internal => "internal",
        Code::Unavailable => "unavailable",
        Code::DataLoss => "data_loss",
        Code::Unauthenticated => "unauthenticated",
    }
}
