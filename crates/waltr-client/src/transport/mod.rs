//! Transport boundary for HTTP, browser, embedded, and test clients.

mod auth;
mod catalog;
mod config;
mod session;
mod status;

pub use config::ConfigWatchStream;
pub use waltr::config::ConfigWatchUpdate;
use waltr::grpc::waltr::catalog::v1::{
    ApplyCatalogModelDefaultsRequest, GetModelCatalogDriftRequest, GetModelCatalogDriftResponse,
    SearchCatalogModelsRequest, SearchCatalogModelsResponse, SetModelCatalogMatchRequest,
    SuggestCatalogMatchesRequest, SuggestCatalogMatchesResponse,
    catalog_service_client::CatalogServiceClient,
};
use waltr::grpc::waltr::config::v1::config_service_client::ConfigServiceClient;
use waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient;
use waltr::grpc::waltr::status::v1::status_service_client::StatusServiceClient;
use prost::Message;
pub use session::{
    SessionTimelineWatchEvent, SessionTimelineWatchStream, SessionTimelineWatchUpdate,
    SessionWatchEvent, SessionWatchStream, SessionWatchUpdate,
};
pub use status::StatusWatchStream;
use tonic::Code;
use tonic::codegen::{Body, Bytes, StdError};

use crate::{
    ClientError, ConfigPatch, Configuration, RollbackTarget, SearchSessionTemplatesRequest,
    SearchSessionTemplatesResponse, SessionRuntimeConfigRequest, SessionSummary, TimelineEvent,
    TimelineQuery,
};

#[cfg(not(target_arch = "wasm32"))]
/// Default gRPC transport for native clients.
pub type DefaultGrpcTransport = NativeGrpcTransport;

#[cfg(target_arch = "wasm32")]
/// Default gRPC transport for browser WASM clients.
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

    /// Search the models.dev catalog.
    fn search_catalog_models(
        &self,
        request: SearchCatalogModelsRequest,
    ) -> impl Future<Output = Result<SearchCatalogModelsResponse, ClientError>>;

    /// Suggest catalog matches for a configured model.
    fn suggest_catalog_matches(
        &self,
        request: SuggestCatalogMatchesRequest,
    ) -> impl Future<Output = Result<SuggestCatalogMatchesResponse, ClientError>>;

    /// Compare a configured model against its catalog match.
    fn get_model_catalog_drift(
        &self,
        request: GetModelCatalogDriftRequest,
    ) -> impl Future<Output = Result<GetModelCatalogDriftResponse, ClientError>>;

    /// Set or clear catalog linkage for a configured model.
    fn set_model_catalog_match(
        &self,
        request: SetModelCatalogMatchRequest,
    ) -> impl Future<Output = Result<Configuration, ClientError>>;

    /// Apply catalog-derived model defaults to configuration.
    fn apply_catalog_model_defaults(
        &self,
        request: ApplyCatalogModelDefaultsRequest,
    ) -> impl Future<Output = Result<Configuration, ClientError>>;

    /// Subscribe to future system status changes.
    fn watch_status(&self) -> impl Future<Output = Result<StatusWatchStream, ClientError>>;

    /// List secret-safe provider auth statuses.
    fn list_auth_statuses(&self) -> impl Future<Output = Result<Vec<AuthStatus>, ClientError>>;

    /// Begin a manual OAuth attempt for any supported auth target.
    fn begin_auth(
        &self,
        target: AuthTarget,
    ) -> impl Future<Output = Result<GenericAuthAttempt, ClientError>>;

    /// Complete a manual OAuth attempt for any supported auth target.
    fn complete_auth(
        &self,
        attempt_id: String,
        input: String,
    ) -> impl Future<Output = Result<AuthStatus, ClientError>>;

    /// Cancel an auth attempt.
    fn cancel_auth(&self, attempt_id: String) -> impl Future<Output = Result<(), ClientError>>;

    /// Logout any supported auth target by deleting its stored credential.
    fn logout(&self, target: AuthTarget) -> impl Future<Output = Result<AuthStatus, ClientError>>;

    /// List secret-safe provider auth statuses.
    fn list_provider_auth_statuses(
        &self,
    ) -> impl Future<Output = Result<Vec<ProviderAuthStatus>, ClientError>>;

    /// Begin a manual provider OAuth attempt.
    fn begin_provider_auth(
        &self,
        provider_key: String,
    ) -> impl Future<Output = Result<AuthAttempt, ClientError>>;

    /// Complete a manual provider OAuth attempt.
    fn complete_provider_auth(
        &self,
        attempt_id: String,
        input: String,
    ) -> impl Future<Output = Result<ProviderAuthStatus, ClientError>>;

    /// Cancel a provider OAuth attempt.
    fn cancel_provider_auth(
        &self,
        attempt_id: String,
    ) -> impl Future<Output = Result<(), ClientError>>;

    /// Logout a provider by deleting its stored credential.
    fn logout_provider(
        &self,
        provider_key: String,
    ) -> impl Future<Output = Result<ProviderAuthStatus, ClientError>>;

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

    /// Search session templates visible to one session.
    fn search_session_templates(
        &self,
        request: SearchSessionTemplatesRequest,
    ) -> impl Future<Output = Result<SearchSessionTemplatesResponse, ClientError>>;

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

    /// Roll back a session to a turn or cursor target.
    fn rollback_session(
        &self,
        session_id: String,
        target: RollbackTarget,
    ) -> impl Future<Output = Result<SessionSummary, ClientError>>;

    /// Roll back to a target and continue from that checkpoint.
    fn continue_session(
        &self,
        session_id: String,
        target: RollbackTarget,
    ) -> impl Future<Output = Result<(), ClientError>>;
}

trait RemoteTransport {
    type Inner;

    fn config_client(
        &self,
    ) -> impl Future<Output = Result<ConfigServiceClient<Self::Inner>, ClientError>>;

    fn auth_client(
        &self,
    ) -> impl Future<Output = Result<AuthServiceClient<Self::Inner>, ClientError>>;

    fn catalog_client(
        &self,
    ) -> impl Future<Output = Result<CatalogServiceClient<Self::Inner>, ClientError>>;

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

    async fn search_catalog_models(
        &self,
        request: SearchCatalogModelsRequest,
    ) -> Result<SearchCatalogModelsResponse, ClientError> {
        catalog::search_catalog_models(self, request).await
    }

    async fn suggest_catalog_matches(
        &self,
        request: SuggestCatalogMatchesRequest,
    ) -> Result<SuggestCatalogMatchesResponse, ClientError> {
        catalog::suggest_catalog_matches(self, request).await
    }

    async fn get_model_catalog_drift(
        &self,
        request: GetModelCatalogDriftRequest,
    ) -> Result<GetModelCatalogDriftResponse, ClientError> {
        catalog::get_model_catalog_drift(self, request).await
    }

    async fn set_model_catalog_match(
        &self,
        request: SetModelCatalogMatchRequest,
    ) -> Result<Configuration, ClientError> {
        catalog::set_model_catalog_match(self, request).await
    }

    async fn apply_catalog_model_defaults(
        &self,
        request: ApplyCatalogModelDefaultsRequest,
    ) -> Result<Configuration, ClientError> {
        catalog::apply_catalog_model_defaults(self, request).await
    }

    async fn watch_status(&self) -> Result<StatusWatchStream, ClientError> {
        status::watch_status(self).await
    }

    async fn list_auth_statuses(&self) -> Result<Vec<AuthStatus>, ClientError> {
        auth::list_auth_statuses(self).await
    }

    async fn begin_auth(&self, target: AuthTarget) -> Result<GenericAuthAttempt, ClientError> {
        auth::begin_auth(self, target).await
    }

    async fn complete_auth(
        &self,
        attempt_id: String,
        input: String,
    ) -> Result<AuthStatus, ClientError> {
        auth::complete_auth(self, attempt_id, input).await
    }

    async fn cancel_auth(&self, attempt_id: String) -> Result<(), ClientError> {
        auth::cancel_auth(self, attempt_id).await
    }

    async fn logout(&self, target: AuthTarget) -> Result<AuthStatus, ClientError> {
        auth::logout(self, target).await
    }

    async fn list_provider_auth_statuses(&self) -> Result<Vec<ProviderAuthStatus>, ClientError> {
        auth::list_provider_auth_statuses(self).await
    }

    async fn begin_provider_auth(&self, provider_key: String) -> Result<AuthAttempt, ClientError> {
        auth::begin_provider_auth(self, provider_key).await
    }

    async fn complete_provider_auth(
        &self,
        attempt_id: String,
        input: String,
    ) -> Result<ProviderAuthStatus, ClientError> {
        auth::complete_provider_auth(self, attempt_id, input).await
    }

    async fn cancel_provider_auth(&self, attempt_id: String) -> Result<(), ClientError> {
        auth::cancel_provider_auth(self, attempt_id).await
    }

    async fn logout_provider(
        &self,
        provider_key: String,
    ) -> Result<ProviderAuthStatus, ClientError> {
        auth::logout_provider(self, provider_key).await
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

    async fn search_session_templates(
        &self,
        request: SearchSessionTemplatesRequest,
    ) -> Result<SearchSessionTemplatesResponse, ClientError> {
        session::search_session_templates(self, request).await
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

    async fn rollback_session(
        &self,
        session_id: String,
        target: RollbackTarget,
    ) -> Result<SessionSummary, ClientError> {
        session::rollback_session(self, session_id, target).await
    }

    async fn continue_session(
        &self,
        session_id: String,
        target: RollbackTarget,
    ) -> Result<(), ClientError> {
        session::continue_session(self, session_id, target).await
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

    async fn auth_client(&self) -> Result<AuthServiceClient<Self::Inner>, ClientError> {
        let channel = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|error| ClientError::new(error.to_string()))?
            .connect()
            .await
            .map_err(|error| ClientError::new(error.to_string()))?;
        Ok(AuthServiceClient::new(channel))
    }

    async fn catalog_client(&self) -> Result<CatalogServiceClient<Self::Inner>, ClientError> {
        let channel = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|error| ClientError::new(error.to_string()))?
            .connect()
            .await
            .map_err(|error| ClientError::new(error.to_string()))?;
        Ok(CatalogServiceClient::new(channel))
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

    async fn auth_client(&self) -> Result<AuthServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(AuthServiceClient::new(transport))
    }

    async fn catalog_client(&self) -> Result<CatalogServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(CatalogServiceClient::new(transport))
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
    if error.code() == Code::Unauthenticated
        && let Ok(target) = AuthTarget::decode(error.details())
        && target.target.is_some()
    {
        return ClientError::authorization_required(
            format!("{}: {}", status_code_name(error.code()), error.message()),
            target,
        );
    }

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
pub use auth::{AuthAttempt, AuthStatus, GenericAuthAttempt, ProviderAuthStatus};
pub use waltr::grpc::waltr::auth::v1::AuthTarget;
use waltr::grpc::waltr::auth::v1::auth_service_client::AuthServiceClient;
