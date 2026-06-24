//! Transport boundary for HTTP, browser, embedded, and test clients.

mod config;
mod session;
mod status;

pub use config::ConfigWatchStream;
pub use waltr::config::ConfigWatchUpdate;
use waltr::grpc::waltr::config::v1::config_service_client::ConfigServiceClient;
use waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient;
use waltr::grpc::waltr::status::v1::status_service_client::StatusServiceClient;
pub use session::{
    SessionTimelineWatchStream, SessionTimelineWatchUpdate, SessionWatchEvent, SessionWatchStream,
    SessionWatchUpdate,
};
pub use status::StatusWatchStream;
use tonic::Code;
use tonic::codegen::{Body, Bytes, StdError};

use crate::{
    ClientError, ConfigPatch, Configuration, SessionRuntimeConfigRequest, SessionSummary,
    TimelineEvent, TimelineQuery,
};

#[cfg(not(target_arch = "wasm32"))]
pub type DefaultGrpcTransport = NativeGrpcTransport;

#[cfg(target_arch = "wasm32")]
pub type DefaultGrpcTransport = WasmGrpcTransport;

/// Runtime-neutral transport adapter used by the shared effect executor.
pub trait WaltrTransport {
    /// Stream type returned by configuration watch subscriptions.
    type ConfigWatchStream: futures_core::Stream<Item = Result<ConfigWatchUpdate, ClientError>>;

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

    /// Create a new server-side session.
    fn create_session(&self) -> impl Future<Output = Result<SessionSummary, ClientError>>;

    /// List current server-side sessions.
    fn list_sessions(&self) -> impl Future<Output = Result<Vec<SessionSummary>, ClientError>>;

    /// Delete a server-side session by id.
    fn delete_session(&self, id: String) -> impl Future<Output = Result<(), ClientError>>;

    /// Replace runtime config metadata for a server-side session.
    fn update_session_runtime_config(
        &self,
        id: String,
        runtime_config: SessionRuntimeConfigRequest,
    ) -> impl Future<Output = Result<SessionSummary, ClientError>>;

    /// Subscribe to future session changes.
    fn watch_sessions(&self) -> impl Future<Output = Result<SessionWatchStream, ClientError>>;

    /// Load a timeline page for one session.
    fn get_timeline(
        &self,
        session_id: String,
        query: Option<TimelineQuery>,
    ) -> impl Future<Output = Result<Vec<TimelineEvent>, ClientError>>;

    /// Subscribe to timeline changes for one session.
    fn watch_timeline(
        &self,
        session_id: String,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
    ) -> impl Future<Output = Result<SessionTimelineWatchStream, ClientError>>;

    /// Submit a user message to a session.
    fn submit_user_message(
        &self,
        session_id: String,
        text: String,
    ) -> impl Future<Output = Result<(), ClientError>>;

    /// Respond to an external request.
    fn respond_external_request(
        &self,
        session_id: String,
        request_id: String,
        response_json: String,
    ) -> impl Future<Output = Result<(), ClientError>>;

    /// Interrupt an active session run.
    fn interrupt_session(
        &self,
        session_id: String,
    ) -> impl Future<Output = Result<(), ClientError>>;
}

trait RemoteTransport {
    type Inner;

    fn config_client(
        &self,
    ) -> impl Future<Output = Result<ConfigServiceClient<Self::Inner>, ClientError>>;

    fn status_client(
        &self,
    ) -> impl Future<Output = Result<StatusServiceClient<Self::Inner>, ClientError>>;

    fn session_client(
        &self,
    ) -> impl Future<Output = Result<SessionServiceClient<Self::Inner>, ClientError>>;
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
        config::get_config(self).await
    }

    async fn patch_config(
        &self,
        patch: ConfigPatch,
        expected_etag: String,
    ) -> Result<Configuration, ClientError> {
        config::patch_config(self, patch, expected_etag).await
    }

    async fn watch_config(&self) -> Result<Self::ConfigWatchStream, ClientError> {
        config::watch_config(self).await
    }

    async fn watch_status(&self) -> Result<StatusWatchStream, ClientError> {
        status::watch_status(self).await
    }

    async fn create_session(&self) -> Result<SessionSummary, ClientError> {
        session::create_session(self).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        session::list_sessions(self).await
    }

    async fn delete_session(&self, id: String) -> Result<(), ClientError> {
        session::delete_session(self, id).await
    }

    async fn update_session_runtime_config(
        &self,
        id: String,
        runtime_config: SessionRuntimeConfigRequest,
    ) -> Result<SessionSummary, ClientError> {
        session::update_session_runtime_config(self, id, runtime_config).await
    }

    async fn watch_sessions(&self) -> Result<SessionWatchStream, ClientError> {
        session::watch_sessions(self).await
    }

    async fn get_timeline(
        &self,
        session_id: String,
        query: Option<TimelineQuery>,
    ) -> Result<Vec<TimelineEvent>, ClientError> {
        session::get_timeline(self, session_id, query).await
    }

    async fn watch_timeline(
        &self,
        session_id: String,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
    ) -> Result<SessionTimelineWatchStream, ClientError> {
        session::watch_timeline(self, session_id, after_cursor, channels, initial_limit).await
    }

    async fn submit_user_message(
        &self,
        session_id: String,
        text: String,
    ) -> Result<(), ClientError> {
        session::submit_user_message(self, session_id, text).await
    }

    async fn respond_external_request(
        &self,
        session_id: String,
        request_id: String,
        response_json: String,
    ) -> Result<(), ClientError> {
        session::respond_external_request(self, session_id, request_id, response_json).await
    }

    async fn interrupt_session(&self, session_id: String) -> Result<(), ClientError> {
        session::interrupt_session(self, session_id).await
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

    async fn session_client(&self) -> Result<SessionServiceClient<Self::Inner>, ClientError> {
        let channel = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|error| ClientError::new(error.to_string()))?
            .connect()
            .await
            .map_err(|error| ClientError::new(error.to_string()))?;
        Ok(SessionServiceClient::new(channel))
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

    async fn session_client(&self) -> Result<SessionServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(SessionServiceClient::new(transport))
    }
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
