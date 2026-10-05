//! Transport boundary for HTTP, browser, embedded, and test clients.

mod auth;
mod catalog;
mod config;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-tracing"))]
mod native_trace;
mod routine;
mod session;
mod status;

pub use config::ConfigWatchStream;
use waltr::grpc::waltr::catalog::v1::{
    GetModelCatalogDriftRequest, GetModelCatalogDriftResponse, SearchCatalogModelsRequest,
    SearchCatalogModelsResponse, SuggestCatalogMatchesRequest, SuggestCatalogMatchesResponse,
    catalog_service_client::CatalogServiceClient,
};
pub use waltr::grpc::waltr::resource::v1::ResourceCatalog as ConfigWatchUpdate;
use waltr::grpc::waltr::resource::v1::{
    ResourceCatalog, resource_service_client::ResourceServiceClient,
};
use waltr::grpc::waltr::routine::v1::routine_service_client::RoutineServiceClient;
use waltr::grpc::waltr::session::v1::session_service_client::SessionServiceClient;
use waltr::grpc::waltr::status::v1::status_service_client::StatusServiceClient;
use prost::Message;
pub use routine::{RoutineDefinitionWatchStream, RoutineInvocationWatchStream};
pub use session::{
    SessionTimelinePage, SessionTimelineProjectionMode, SessionTimelineWatchEvent,
    SessionTimelineWatchStream, SessionTimelineWatchUpdate, SessionWatchEvent, SessionWatchStream,
    SessionWatchUpdate,
};
pub use status::StatusWatchStream;
use tonic::Code;
use tonic::codegen::{Body, Bytes, StdError};

#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, RwLock};
#[cfg(not(target_arch = "wasm32"))]
use tonic::metadata::AsciiMetadataValue;
#[cfg(not(target_arch = "wasm32"))]
use tonic::service::{Interceptor, interceptor::InterceptedService};
#[cfg(all(not(target_arch = "wasm32"), feature = "native-tracing"))]
use tower::Layer as _;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-tracing"))]
use tracing::Instrument as _;

use crate::{
    ClientError, ClientRoutineInvocationDetail, ClientRoutineInvocationPage,
    ClientRoutineStartOutcome, InferenceSettingsRequest, ListAgentsResponse, ResourceWrite,
    RollbackTarget, SearchSessionTemplatesRequest, SearchSessionTemplatesResponse, SessionSummary,
    StartManualRoutineArgs, TimelineQuery,
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
    fn get_config(&self) -> impl Future<Output = Result<ResourceCatalog, ClientError>>;

    /// Apply one prepared canonical resource-slot write.
    fn write_resource(&self, write: ResourceWrite)
    -> impl Future<Output = Result<(), ClientError>>;

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

    /// Read sanitized durable callback completion state.
    fn get_auth_attempt_status(
        &self,
        attempt_id: String,
    ) -> impl Future<Output = Result<GenericAuthAttemptStatus, ClientError>>;

    /// Cancel an auth attempt.
    fn cancel_auth(&self, attempt_id: String) -> impl Future<Output = Result<(), ClientError>>;

    /// Logout any supported auth target by deleting its stored credential.
    fn logout(&self, target: AuthTarget) -> impl Future<Output = Result<AuthStatus, ClientError>>;

    /// Force-refresh any supported OAuth target.
    fn refresh_auth(
        &self,
        target: AuthTarget,
    ) -> impl Future<Output = Result<AuthStatus, ClientError>>;

    /// Create a new server-side session.
    fn create_session(
        &self,
        agent_name: String,
    ) -> impl Future<Output = Result<SessionSummary, ClientError>>;

    /// List selectable session agents.
    fn list_agents(&self) -> impl Future<Output = Result<ListAgentsResponse, ClientError>>;

    /// List current server-side sessions.
    fn list_sessions(&self) -> impl Future<Output = Result<Vec<SessionSummary>, ClientError>>;

    /// Delete a server-side session by id.
    fn delete_session(&self, id: String) -> impl Future<Output = Result<(), ClientError>>;

    /// Delete every server-side session.
    fn wipe_sessions(&self) -> impl Future<Output = Result<(), ClientError>>;

    /// Replace inference settings for a server-side session.
    fn update_session_inference_settings(
        &self,
        id: String,
        inference_settings: InferenceSettingsRequest,
    ) -> impl Future<Output = Result<SessionSummary, ClientError>>;

    /// Switch a server-side session to an exact selectable agent.
    fn update_session_agent(
        &self,
        id: String,
        agent_name: String,
    ) -> impl Future<Output = Result<SessionSummary, ClientError>>;

    /// Subscribe to future session changes.
    fn watch_sessions(&self) -> impl Future<Output = Result<SessionWatchStream, ClientError>>;

    /// Load a timeline page for one session.
    fn get_timeline(
        &self,
        session_id: String,
        query: Option<TimelineQuery>,
        projection_mode: SessionTimelineProjectionMode,
    ) -> impl Future<Output = Result<SessionTimelinePage, ClientError>>;

    /// Subscribe to timeline changes for one session.
    fn watch_timeline(
        &self,
        session_id: String,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
        projection_mode: SessionTimelineProjectionMode,
    ) -> impl Future<Output = Result<SessionTimelineWatchStream, ClientError>>;

    /// Open an owner-bound app for a durable tool call.
    fn open_session_mcp_app(
        &self,
        _request: waltr::grpc::waltr::session::v1::OpenSessionMcpAppRequest,
    ) -> impl Future<Output = Result<crate::McpAppView, ClientError>> {
        async { Err(ClientError::new("MCP Apps transport unavailable")) }
    }
    /// Invoke a tool through the server's owner-bound app lane.
    fn call_session_mcp_app_tool(
        &self,
        _request: waltr::grpc::waltr::session::v1::CallSessionMcpAppToolRequest,
    ) -> impl Future<Output = Result<String, ClientError>> {
        async { Err(ClientError::new("MCP Apps transport unavailable")) }
    }
    /// Close an owner-bound server lease.
    fn close_session_mcp_app(
        &self,
        _request: waltr::grpc::waltr::session::v1::CloseSessionMcpAppRequest,
    ) -> impl Future<Output = Result<(), ClientError>> {
        async { Err(ClientError::new("MCP Apps transport unavailable")) }
    }
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

    /// Subscribe to complete global manual Routine definition snapshots.
    fn watch_manual_routines(
        &self,
        request_id: String,
    ) -> impl Future<Output = Result<RoutineDefinitionWatchStream, ClientError>>;

    /// Start one manual Routine. Callers must not automatically retry this operation.
    fn start_manual_routine(
        &self,
        args: StartManualRoutineArgs,
    ) -> impl Future<Output = Result<ClientRoutineStartOutcome, ClientError>>;

    /// Load one older invocation-history page.
    fn list_routine_invocations(
        &self,
        request_id: String,
        page_size: u32,
        before_admission_sequence: Option<u64>,
        metadata_filters: std::collections::BTreeMap<String, String>,
    ) -> impl Future<Output = Result<ClientRoutineInvocationPage, ClientError>>;

    /// Load full task-level detail for one invocation.
    fn get_routine_invocation(
        &self,
        request_id: String,
        invocation_id: String,
    ) -> impl Future<Output = Result<ClientRoutineInvocationDetail, ClientError>>;

    /// Subscribe to a bounded snapshot or exact event replay.
    fn watch_routine_invocations(
        &self,
        request_id: String,
        after_sequence: Option<u64>,
        snapshot_size: u32,
        metadata_filters: std::collections::BTreeMap<String, String>,
    ) -> impl Future<Output = Result<RoutineInvocationWatchStream, ClientError>>;
}

trait RemoteTransport {
    type Inner;

    fn request_credentials(&self) -> RequestCredentials;

    fn resource_client(
        &self,
        credentials: &RequestCredentials,
    ) -> impl Future<Output = Result<ResourceServiceClient<Self::Inner>, ClientError>>;

    fn auth_client(
        &self,
        credentials: &RequestCredentials,
    ) -> impl Future<Output = Result<AuthServiceClient<Self::Inner>, ClientError>>;

    fn catalog_client(
        &self,
        credentials: &RequestCredentials,
    ) -> impl Future<Output = Result<CatalogServiceClient<Self::Inner>, ClientError>>;

    fn status_client(
        &self,
        credentials: &RequestCredentials,
    ) -> impl Future<Output = Result<StatusServiceClient<Self::Inner>, ClientError>>;

    fn session_client(
        &self,
        credentials: &RequestCredentials,
    ) -> impl Future<Output = Result<SessionServiceClient<Self::Inner>, ClientError>>;

    fn routine_client(
        &self,
        credentials: &RequestCredentials,
    ) -> impl Future<Output = Result<RoutineServiceClient<Self::Inner>, ClientError>>;
}

#[derive(Clone, Default)]
struct RequestCredentials {
    #[cfg(not(target_arch = "wasm32"))]
    provider: NativeCredentialProvider,
    #[cfg(not(target_arch = "wasm32"))]
    lease_generation: u64,
    #[cfg(not(target_arch = "wasm32"))]
    credential_revision: u64,
    #[cfg(not(target_arch = "wasm32"))]
    authorization: Option<AsciiMetadataValue>,
}

#[allow(
    clippy::future_not_send,
    reason = "the transport implementation also supports browser-local RPC futures"
)]
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

    async fn get_config(&self) -> Result<ResourceCatalog, ClientError> {
        config::get_config(self).await
    }

    async fn write_resource(&self, write: ResourceWrite) -> Result<(), ClientError> {
        config::write_resource(self, write).await
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

    async fn get_auth_attempt_status(
        &self,
        attempt_id: String,
    ) -> Result<GenericAuthAttemptStatus, ClientError> {
        auth::get_auth_attempt_status(self, attempt_id).await
    }

    async fn cancel_auth(&self, attempt_id: String) -> Result<(), ClientError> {
        auth::cancel_auth(self, attempt_id).await
    }

    async fn logout(&self, target: AuthTarget) -> Result<AuthStatus, ClientError> {
        auth::logout(self, target).await
    }

    async fn refresh_auth(&self, target: AuthTarget) -> Result<AuthStatus, ClientError> {
        auth::refresh_auth(self, target).await
    }

    async fn create_session(&self, agent_name: String) -> Result<SessionSummary, ClientError> {
        session::create_session(self, agent_name).await
    }

    async fn list_agents(&self) -> Result<ListAgentsResponse, ClientError> {
        session::list_agents(self).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        session::list_sessions(self).await
    }

    async fn delete_session(&self, id: String) -> Result<(), ClientError> {
        session::delete_session(self, id).await
    }

    async fn wipe_sessions(&self) -> Result<(), ClientError> {
        session::wipe_sessions(self).await
    }

    async fn update_session_inference_settings(
        &self,
        id: String,
        inference_settings: InferenceSettingsRequest,
    ) -> Result<SessionSummary, ClientError> {
        session::update_session_inference_settings(self, id, inference_settings).await
    }

    async fn update_session_agent(
        &self,
        id: String,
        agent_name: String,
    ) -> Result<SessionSummary, ClientError> {
        session::update_session_agent(self, id, agent_name).await
    }

    async fn watch_sessions(&self) -> Result<SessionWatchStream, ClientError> {
        session::watch_sessions(self).await
    }

    async fn get_timeline(
        &self,
        session_id: String,
        query: Option<TimelineQuery>,
        projection_mode: SessionTimelineProjectionMode,
    ) -> Result<SessionTimelinePage, ClientError> {
        session::get_timeline(self, session_id, query, projection_mode).await
    }

    async fn watch_timeline(
        &self,
        session_id: String,
        after_cursor: Option<u64>,
        channels: Vec<String>,
        initial_limit: Option<u32>,
        projection_mode: SessionTimelineProjectionMode,
    ) -> Result<SessionTimelineWatchStream, ClientError> {
        session::watch_timeline(
            self,
            session_id,
            after_cursor,
            channels,
            initial_limit,
            projection_mode,
        )
        .await
    }

    async fn search_session_templates(
        &self,
        request: SearchSessionTemplatesRequest,
    ) -> Result<SearchSessionTemplatesResponse, ClientError> {
        session::search_session_templates(self, request).await
    }
    async fn open_session_mcp_app(
        &self,
        request: waltr::grpc::waltr::session::v1::OpenSessionMcpAppRequest,
    ) -> Result<crate::McpAppView, ClientError> {
        let response = session::open_session_mcp_app(self, request).await?;
        Ok(crate::McpAppView {
            app_id: response.app_id,
            html: response.html,
            tool_json: response.tool_json,
            arguments_json: response.arguments_json,
            result_json: response.result_json,
            resource_meta_json: response.resource_meta_json,
        })
    }
    async fn call_session_mcp_app_tool(
        &self,
        request: waltr::grpc::waltr::session::v1::CallSessionMcpAppToolRequest,
    ) -> Result<String, ClientError> {
        Ok(session::call_session_mcp_app_tool(self, request)
            .await?
            .result_json)
    }
    async fn close_session_mcp_app(
        &self,
        request: waltr::grpc::waltr::session::v1::CloseSessionMcpAppRequest,
    ) -> Result<(), ClientError> {
        session::close_session_mcp_app(self, request)
            .await
            .map(|_| ())
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

    async fn watch_manual_routines(
        &self,
        request_id: String,
    ) -> Result<RoutineDefinitionWatchStream, ClientError> {
        routine::watch_manual_routines(self, request_id).await
    }

    async fn start_manual_routine(
        &self,
        args: StartManualRoutineArgs,
    ) -> Result<ClientRoutineStartOutcome, ClientError> {
        routine::start_manual_routine(self, args).await
    }

    async fn list_routine_invocations(
        &self,
        request_id: String,
        page_size: u32,
        before_admission_sequence: Option<u64>,
        metadata_filters: std::collections::BTreeMap<String, String>,
    ) -> Result<ClientRoutineInvocationPage, ClientError> {
        routine::list_routine_invocations(
            self,
            request_id,
            page_size,
            before_admission_sequence,
            metadata_filters,
        )
        .await
    }

    async fn get_routine_invocation(
        &self,
        request_id: String,
        invocation_id: String,
    ) -> Result<ClientRoutineInvocationDetail, ClientError> {
        routine::get_routine_invocation(self, request_id, invocation_id).await
    }

    async fn watch_routine_invocations(
        &self,
        request_id: String,
        after_sequence: Option<u64>,
        snapshot_size: u32,
        metadata_filters: std::collections::BTreeMap<String, String>,
    ) -> Result<RoutineInvocationWatchStream, ClientError> {
        routine::watch_routine_invocations(
            self,
            request_id,
            after_sequence,
            snapshot_size,
            metadata_filters,
        )
        .await
    }
}

/// Native gRPC transport backed by tonic channels.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct NativeGrpcTransport {
    endpoint: String,
    credentials: NativeCredentialProvider,
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-tracing"))]
type NativeChannel = native_trace::NativeTraceService<tonic::transport::Channel>;
#[cfg(all(not(target_arch = "wasm32"), not(feature = "native-tracing")))]
type NativeChannel = tonic::transport::Channel;

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::large_enum_variant)]
enum NativeEndpoint {
    Plaintext(tonic::transport::Endpoint),
    WebPkiRootsTls {
        endpoint: tonic::transport::Endpoint,
        tls: tonic::transport::ClientTlsConfig,
    },
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeEndpoint {
    fn into_tonic_endpoint(self) -> Result<tonic::transport::Endpoint, ClientError> {
        match self {
            Self::Plaintext(endpoint) => Ok(endpoint),
            Self::WebPkiRootsTls { endpoint, tls } => endpoint
                .tls_config(tls)
                .map_err(|error| ClientError::new(error.to_string())),
        }
    }
}

/// Cloneable in-memory bearer credential provider for native runtimes.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Default)]
pub struct NativeCredentialProvider {
    state: Arc<RwLock<NativeCredentialState>>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct NativeCredentialState {
    lease_generation: u64,
    credential_revision: u64,
    authorization: Option<AsciiMetadataValue>,
}

/// Generation lease used to reject stale native credential updates.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeCredentialLease {
    lease_generation: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeCredentialLease {
    /// Return the monotonic credential generation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.lease_generation
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for NativeCredentialProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeCredentialProvider")
            .field("has_bearer_token", &self.has_bearer_token())
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeCredentialProvider {
    /// Create an empty in-memory credential provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the bearer token used by subsequently sent requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be represented as gRPC metadata.
    pub fn replace_bearer_token(
        &self,
        token: impl AsRef<str>,
    ) -> Result<NativeCredentialLease, ClientError> {
        let authorization = format!("Bearer {}", token.as_ref())
            .parse::<AsciiMetadataValue>()
            .map_err(|_| ClientError::new("invalid bearer token metadata"))?;
        let mut state = self.write_state();
        state.lease_generation = state.lease_generation.saturating_add(1);
        state.credential_revision = state.credential_revision.saturating_add(1);
        state.authorization = Some(authorization);
        Ok(NativeCredentialLease {
            lease_generation: state.lease_generation,
        })
    }

    /// Begin a credential generation, clearing any previous token.
    #[must_use]
    pub fn begin_credential_lease(&self) -> NativeCredentialLease {
        self.clear_bearer_token()
    }

    /// Return a lease for the current credential generation without changing it.
    #[must_use]
    pub fn credential_lease(&self) -> NativeCredentialLease {
        NativeCredentialLease {
            lease_generation: self.read_state().lease_generation,
        }
    }

    /// Return the current token revision for atomic compare-and-clear operations.
    #[must_use]
    pub fn credential_revision(&self) -> u64 {
        self.read_state().credential_revision
    }

    /// Replace the token only when the lease is still current.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be represented as gRPC metadata.
    pub fn replace_bearer_token_if_current(
        &self,
        lease: NativeCredentialLease,
        token: impl AsRef<str>,
    ) -> Result<bool, ClientError> {
        let authorization = format!("Bearer {}", token.as_ref())
            .parse::<AsciiMetadataValue>()
            .map_err(|_| ClientError::new("invalid bearer token metadata"))?;
        let mut state = self.write_state();
        if state.lease_generation != lease.lease_generation {
            return Ok(false);
        }
        state.credential_revision = state.credential_revision.saturating_add(1);
        state.authorization = Some(authorization);
        drop(state);
        Ok(true)
    }

    /// Clear the bearer token used by subsequently sent requests.
    #[must_use]
    pub fn clear_bearer_token(&self) -> NativeCredentialLease {
        let mut state = self.write_state();
        state.lease_generation = state.lease_generation.saturating_add(1);
        state.credential_revision = state.credential_revision.saturating_add(1);
        state.authorization = None;
        NativeCredentialLease {
            lease_generation: state.lease_generation,
        }
    }

    /// Clear credentials only when both the lease and token revision are current.
    #[must_use]
    pub fn clear_bearer_token_if_current(
        &self,
        lease: NativeCredentialLease,
        credential_revision: u64,
    ) -> bool {
        let mut state = self.write_state();
        if state.lease_generation != lease.lease_generation
            || state.credential_revision != credential_revision
        {
            return false;
        }
        state.lease_generation = state.lease_generation.saturating_add(1);
        state.credential_revision = state.credential_revision.saturating_add(1);
        state.authorization = None;
        true
    }

    fn request_credentials(&self) -> RequestCredentials {
        let state = self.read_state();
        let credentials = RequestCredentials {
            provider: self.clone(),
            lease_generation: state.lease_generation,
            credential_revision: state.credential_revision,
            authorization: state.authorization.clone(),
        };
        drop(state);
        credentials
    }

    /// Return whether a bearer token is currently installed.
    #[must_use]
    pub fn has_bearer_token(&self) -> bool {
        self.read_state().authorization.is_some()
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, NativeCredentialState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, NativeCredentialState> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct NativeBearerInterceptor {
    authorization: Option<AsciiMetadataValue>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for NativeBearerInterceptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeBearerInterceptor")
            .field("has_bearer_token", &self.authorization.is_some())
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Interceptor for NativeBearerInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(authorization) = self.authorization.clone() {
            request
                .metadata_mut()
                .insert("authorization", authorization);
        }
        Ok(request)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for NativeGrpcTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeGrpcTransport")
            .field("endpoint", &self.endpoint)
            .field("credentials", &self.credentials)
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeGrpcTransport {
    /// Create a native gRPC transport for an endpoint base URL.
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            credentials: NativeCredentialProvider::new(),
        }
    }

    /// Create a transport using a runtime-owned in-memory credential provider.
    #[must_use]
    pub fn with_credential_provider(
        endpoint: impl Into<String>,
        credentials: NativeCredentialProvider,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            credentials,
        }
    }

    /// Return a clone of the shared in-memory credential provider.
    #[must_use]
    pub fn credential_provider(&self) -> NativeCredentialProvider {
        self.credentials.clone()
    }

    /// Replace the bearer token used by subsequently sent requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be represented as gRPC metadata.
    pub fn replace_bearer_token(
        &self,
        token: impl AsRef<str>,
    ) -> Result<NativeCredentialLease, ClientError> {
        self.credentials.replace_bearer_token(token)
    }

    /// Begin a credential generation, clearing any previous token.
    #[must_use]
    pub fn begin_credential_lease(&self) -> NativeCredentialLease {
        self.credentials.begin_credential_lease()
    }

    /// Replace the token only when the lease is still current.
    ///
    /// # Errors
    ///
    /// Returns an error when the token cannot be represented as gRPC metadata.
    pub fn replace_bearer_token_if_current(
        &self,
        lease: NativeCredentialLease,
        token: impl AsRef<str>,
    ) -> Result<bool, ClientError> {
        self.credentials
            .replace_bearer_token_if_current(lease, token)
    }

    /// Clear the bearer token used by subsequently sent requests.
    #[must_use]
    pub fn clear_bearer_token(&self) -> NativeCredentialLease {
        self.credentials.clear_bearer_token()
    }

    /// Clear credentials only when the supplied lease and revision are current.
    #[must_use]
    pub fn clear_bearer_token_if_current(
        &self,
        lease: NativeCredentialLease,
        credential_revision: u64,
    ) -> bool {
        self.credentials
            .clear_bearer_token_if_current(lease, credential_revision)
    }

    async fn channel(&self) -> Result<NativeChannel, ClientError> {
        let endpoint = self.native_endpoint()?.into_tonic_endpoint()?;
        #[cfg(feature = "native-tracing")]
        {
            let span = native_trace::connection_span();
            let started = std::time::Instant::now();
            let channel = endpoint.connect().instrument(span.clone()).await;
            native_trace::finish_connection(&span, channel.is_ok(), started);
            channel
                .map(|channel| native_trace::NativeTraceLayer.layer(channel))
                .map_err(|error| ClientError::new(error.to_string()))
        }
        #[cfg(not(feature = "native-tracing"))]
        endpoint
            .connect()
            .await
            .map_err(|error| ClientError::new(error.to_string()))
    }

    fn native_endpoint(&self) -> Result<NativeEndpoint, ClientError> {
        let endpoint = tonic::transport::Endpoint::from_shared(self.endpoint.clone())
            .map_err(|error| ClientError::new(error.to_string()))?;
        if self.endpoint.starts_with("https://") {
            drop(rustls::crypto::ring::default_provider().install_default());
            Ok(NativeEndpoint::WebPkiRootsTls {
                endpoint,
                tls: tonic::transport::ClientTlsConfig::new().with_webpki_roots(),
            })
        } else {
            Ok(NativeEndpoint::Plaintext(endpoint))
        }
    }

    #[cfg(test)]
    fn interceptor(&self) -> NativeBearerInterceptor {
        NativeBearerInterceptor {
            authorization: self.credentials.read_state().authorization.clone(),
        }
    }

    fn interceptor_for(credentials: &RequestCredentials) -> NativeBearerInterceptor {
        NativeBearerInterceptor {
            authorization: credentials.authorization.clone(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl RemoteTransport for NativeGrpcTransport {
    type Inner = InterceptedService<NativeChannel, NativeBearerInterceptor>;

    fn request_credentials(&self) -> RequestCredentials {
        self.credentials.request_credentials()
    }

    async fn resource_client(
        &self,
        credentials: &RequestCredentials,
    ) -> Result<ResourceServiceClient<Self::Inner>, ClientError> {
        Ok(ResourceServiceClient::with_interceptor(
            self.channel().await?,
            Self::interceptor_for(credentials),
        ))
    }

    async fn auth_client(
        &self,
        credentials: &RequestCredentials,
    ) -> Result<AuthServiceClient<Self::Inner>, ClientError> {
        Ok(AuthServiceClient::with_interceptor(
            self.channel().await?,
            Self::interceptor_for(credentials),
        ))
    }

    async fn catalog_client(
        &self,
        credentials: &RequestCredentials,
    ) -> Result<CatalogServiceClient<Self::Inner>, ClientError> {
        Ok(CatalogServiceClient::with_interceptor(
            self.channel().await?,
            Self::interceptor_for(credentials),
        ))
    }

    async fn status_client(
        &self,
        credentials: &RequestCredentials,
    ) -> Result<StatusServiceClient<Self::Inner>, ClientError> {
        Ok(StatusServiceClient::with_interceptor(
            self.channel().await?,
            Self::interceptor_for(credentials),
        ))
    }

    async fn session_client(
        &self,
        credentials: &RequestCredentials,
    ) -> Result<SessionServiceClient<Self::Inner>, ClientError> {
        Ok(SessionServiceClient::with_interceptor(
            self.channel().await?,
            Self::interceptor_for(credentials),
        ))
    }

    async fn routine_client(
        &self,
        credentials: &RequestCredentials,
    ) -> Result<RoutineServiceClient<Self::Inner>, ClientError> {
        Ok(RoutineServiceClient::with_interceptor(
            self.channel().await?,
            Self::interceptor_for(credentials),
        ))
    }
}

/// Browser gRPC-web transport backed by `fetch`.
/// Same-origin requests retain the browser transport's cookie credential behavior;
/// bearer injection is intentionally native-only.
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

    fn request_credentials(&self) -> RequestCredentials {
        RequestCredentials::default()
    }

    async fn resource_client(
        &self,
        _: &RequestCredentials,
    ) -> Result<ResourceServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(ResourceServiceClient::new(transport))
    }

    async fn auth_client(
        &self,
        _: &RequestCredentials,
    ) -> Result<AuthServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(AuthServiceClient::new(transport))
    }

    async fn catalog_client(
        &self,
        _: &RequestCredentials,
    ) -> Result<CatalogServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(CatalogServiceClient::new(transport))
    }

    async fn status_client(
        &self,
        _: &RequestCredentials,
    ) -> Result<StatusServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(StatusServiceClient::new(transport))
    }

    async fn session_client(
        &self,
        _: &RequestCredentials,
    ) -> Result<SessionServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(SessionServiceClient::new(transport))
    }

    async fn routine_client(
        &self,
        _: &RequestCredentials,
    ) -> Result<RoutineServiceClient<Self::Inner>, ClientError> {
        let transport = tonic_web_wasm_client::Client::new(self.api_base.clone());
        Ok(RoutineServiceClient::new(transport))
    }
}

fn status_error(error: &tonic::Status, credentials: &RequestCredentials) -> ClientError {
    #[cfg(target_arch = "wasm32")]
    let _ = credentials;

    let failure = waltr::grpc::waltr::auth::v1::AuthenticationFailure::decode(error.details()).ok();
    let domain = failure
        .as_ref()
        .filter(|failure| failure.version == 1)
        .and_then(|failure| {
            waltr::grpc::waltr::auth::v1::AuthenticationDomain::try_from(failure.domain).ok()
        });
    if let Some(failure) = failure.and_then(crate::OutboundAuthenticationFailure::from_proto) {
        return ClientError::outbound_authentication(
            format!("{}: {}", status_code_name(error.code()), error.message()),
            failure,
        );
    }

    if error.code() == Code::Unauthenticated
        && domain == Some(waltr::grpc::waltr::auth::v1::AuthenticationDomain::WaltrAccess)
    {
        #[cfg(not(target_arch = "wasm32"))]
        if !credentials.provider.clear_bearer_token_if_current(
            NativeCredentialLease {
                lease_generation: credentials.lease_generation,
            },
            credentials.credential_revision,
        ) {
            return ClientError::new(format!(
                "{}: stale Waltr credentials were rejected",
                status_code_name(error.code())
            ));
        }

        return ClientError::waltr_login_required(format!(
            "{}: {}",
            status_code_name(error.code()),
            error.message()
        ));
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
pub use auth::{AuthStatus, GenericAuthAttempt, GenericAuthAttemptStatus};
pub use waltr::grpc::waltr::auth::v1::AuthTarget;
use waltr::grpc::waltr::auth::v1::auth_service_client::AuthServiceClient;

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::Mutex;

    use futures_core::Stream;
    use futures_util::StreamExt;
    use waltr::grpc::waltr::auth::v1::auth_target;
    use waltr::grpc::waltr::resource::v1::{
        GetResourceCatalogRequest, GetResourceCatalogResponse, InspectResourceRequest,
        InspectResourceResponse, ReplaceResourceRequest, ReplaceResourceResponse,
        ResetResourceRequest, ResetResourceResponse, WatchResourceCatalogRequest,
        WatchResourceCatalogResponse,
        resource_service_server::{ResourceService, ResourceServiceServer},
    };

    #[derive(Clone, Default)]
    struct MetadataResourceService {
        authorizations: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl MetadataResourceService {
        fn record<T>(&self, request: &tonic::Request<T>) {
            let authorization = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            self.authorizations.lock().unwrap().push(authorization);
        }
    }

    #[tonic::async_trait]
    impl ResourceService for MetadataResourceService {
        async fn replace_resource(
            &self,
            request: tonic::Request<ReplaceResourceRequest>,
        ) -> Result<tonic::Response<ReplaceResourceResponse>, tonic::Status> {
            self.record(&request);
            Ok(tonic::Response::new(ReplaceResourceResponse::default()))
        }

        async fn get_resource_catalog(
            &self,
            request: tonic::Request<GetResourceCatalogRequest>,
        ) -> Result<tonic::Response<GetResourceCatalogResponse>, tonic::Status> {
            self.record(&request);
            Ok(tonic::Response::new(GetResourceCatalogResponse {
                catalog: Some(ResourceCatalog::default()),
            }))
        }

        async fn inspect_resource(
            &self,
            request: tonic::Request<InspectResourceRequest>,
        ) -> Result<tonic::Response<InspectResourceResponse>, tonic::Status> {
            self.record(&request);
            Ok(tonic::Response::new(InspectResourceResponse::default()))
        }

        async fn reset_resource(
            &self,
            request: tonic::Request<ResetResourceRequest>,
        ) -> Result<tonic::Response<ResetResourceResponse>, tonic::Status> {
            self.record(&request);
            Ok(tonic::Response::new(ResetResourceResponse::default()))
        }

        type WatchResourceCatalogStream =
            Pin<Box<dyn Stream<Item = Result<WatchResourceCatalogResponse, tonic::Status>> + Send>>;

        async fn watch_resource_catalog(
            &self,
            request: tonic::Request<WatchResourceCatalogRequest>,
        ) -> Result<tonic::Response<Self::WatchResourceCatalogStream>, tonic::Status> {
            self.record(&request);
            Ok(tonic::Response::new(Box::pin(futures_util::stream::once(
                async {
                    Ok(WatchResourceCatalogResponse {
                        catalog: Some(ResourceCatalog::default()),
                    })
                },
            ))))
        }
    }

    async fn metadata_transport_server() -> (
        NativeGrpcTransport,
        MetadataResourceService,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("metadata test listener should bind");
        let address = listener.local_addr().unwrap();
        let incoming = futures_util::stream::unfold(listener, |listener| async move {
            let item = listener.accept().await.map(|(stream, _)| stream);
            Some((item, listener))
        });
        let service = MetadataResourceService::default();
        let server_service = service.clone();
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ResourceServiceServer::new(server_service))
                .serve_with_incoming(incoming)
                .await
                .expect("metadata test server should run");
        });
        (
            NativeGrpcTransport::new(format!("http://{address}")),
            service,
            task,
        )
    }

    fn waltr_access_status(message: &str) -> tonic::Status {
        let failure = waltr::grpc::waltr::auth::v1::AuthenticationFailure {
            version: 1,
            domain: waltr::grpc::waltr::auth::v1::AuthenticationDomain::WaltrAccess as i32,
            reason: waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::CredentialRequired
                as i32,
            target: None,
            mcp_authorization: None,
        };
        tonic::Status::with_details(
            Code::Unauthenticated,
            message,
            Bytes::from(failure.encode_to_vec()),
        )
    }

    #[test]
    fn unauthenticated_status_without_explicit_domain_is_not_waltr_login() {
        for status in [
            tonic::Status::unauthenticated("login required"),
            tonic::Status::with_details(
                Code::Unauthenticated,
                "login required",
                Bytes::from_static(&[0]),
            ),
            tonic::Status::with_details(
                Code::Unauthenticated,
                "login required",
                Bytes::from(AuthTarget::default().encode_to_vec()),
            ),
            tonic::Status::with_details(
                Code::Unauthenticated,
                "login required",
                Bytes::from(
                    AuthTarget {
                        target: Some(auth_target::Target::ProviderKey(String::new())),
                    }
                    .encode_to_vec(),
                ),
            ),
        ] {
            let error = status_error(&status, &RequestCredentials::default());
            assert_eq!(error.kind(), crate::ClientErrorKind::Other);
            assert!(error.authentication_failure().is_none());
        }
    }

    #[test]
    fn explicit_waltr_access_failure_requires_waltr_login() {
        let status = waltr_access_status("login required");

        let error = status_error(&status, &RequestCredentials::default());

        assert!(error.is_waltr_login_required());
    }

    #[test]
    fn every_valid_outbound_reason_retains_exact_provider_failure_and_server_message() {
        let target = AuthTarget {
            target: Some(auth_target::Target::ProviderKey("anthropic".to_owned())),
        };
        for reason in [
            waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::CredentialRequired,
            waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::CredentialRejected,
            waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::NetworkFailed,
            waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::TokenExchangeFailed,
            waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::ResponseInvalid,
            waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::ConfigurationRequired,
        ] {
            let failure = waltr::grpc::waltr::auth::v1::AuthenticationFailure {
                version: 1,
                domain: waltr::grpc::waltr::auth::v1::AuthenticationDomain::OutboundProvider as i32,
                reason: reason as i32,
                target: Some(target.clone()),
                mcp_authorization: None,
            };
            let status = tonic::Status::with_details(
                Code::FailedPrecondition,
                "safe provider failure",
                Bytes::from(failure.encode_to_vec()),
            );
            let error = status_error(&status, &RequestCredentials::default());
            let actual = error.authentication_failure().expect("outbound failure");
            assert_eq!(actual.version, 1);
            assert_eq!(actual.domain, failure.domain);
            assert_eq!(actual.reason, reason as i32);
            assert_eq!(actual.target, target);
            assert_eq!(
                error.message(),
                "failed_precondition: safe provider failure"
            );
            assert!(!error.is_waltr_login_required());
        }
    }

    #[test]
    fn unauthenticated_status_with_mcp_target_remains_credential_authorization() {
        let target = AuthTarget {
            target: Some(auth_target::Target::McpServerKey("github".to_owned())),
        };
        let failure = waltr::grpc::waltr::auth::v1::AuthenticationFailure {
            version: 1,
            domain: waltr::grpc::waltr::auth::v1::AuthenticationDomain::OutboundMcp as i32,
            reason: waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::CredentialRequired
                as i32,
            target: Some(target.clone()),
            mcp_authorization: Some(waltr::grpc::waltr::auth::v1::McpAuthorizationContext {
                endpoint: "https://mcp.example/rpc".to_owned(),
                http_status: 401,
                resource_metadata_url: Some("javascript:alert(1)".to_owned()),
                scope: Some("repo:read".to_owned()),
                error: Some("invalid_token".to_owned()),
                error_description: None,
                error_uri: None,
                realm: None,
            }),
        };
        let status = tonic::Status::with_details(
            Code::Unauthenticated,
            "MCP login required",
            Bytes::from(failure.encode_to_vec()),
        );

        let credentials = RequestCredentials::default();
        let error = status_error(&status, &credentials);

        let failure = error.authentication_failure().expect("outbound failure");
        assert_eq!(failure.target, target);
        let context = failure.mcp_context.as_ref().expect("MCP challenge context");
        assert_eq!(context.endpoint.as_deref(), Some("https://mcp.example/rpc"));
        assert_eq!(context.scope.as_deref(), Some("repo:read"));
        assert!(context.resource_metadata_url.is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn outbound_failure_never_clears_waltr_bearer_or_requires_waltr_login() {
        let credentials = NativeCredentialProvider::new();
        credentials
            .replace_bearer_token("waltr-token")
            .expect("token should install");
        let request = credentials.request_credentials();
        let target = AuthTarget {
            target: Some(auth_target::Target::McpServerKey("self".to_owned())),
        };
        let failure = waltr::grpc::waltr::auth::v1::AuthenticationFailure {
            version: 1,
            domain: waltr::grpc::waltr::auth::v1::AuthenticationDomain::OutboundMcp as i32,
            reason: waltr::grpc::waltr::auth::v1::AuthenticationFailureReason::NetworkFailed as i32,
            target: Some(target),
            mcp_authorization: None,
        };
        let status = tonic::Status::with_details(
            Code::Unavailable,
            "token service unavailable",
            Bytes::from(failure.encode_to_vec()),
        );

        let error = status_error(&status, &request);

        assert_eq!(error.kind(), crate::ClientErrorKind::Other);
        assert!(error.authentication_failure().is_some());
        assert!(!error.is_waltr_login_required());
        assert!(credentials.has_bearer_token());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_bearer_interceptor_covers_unary_and_streaming_requests() {
        let credentials = NativeCredentialProvider::new();
        credentials
            .replace_bearer_token("stream-token")
            .expect("token should be valid metadata");
        let mut interceptor = NativeBearerInterceptor {
            authorization: credentials.request_credentials().authorization,
        };

        let unary = interceptor
            .call(tonic::Request::new(()))
            .expect("unary request should be authorized");
        let streaming = interceptor
            .call(tonic::Request::new(()))
            .expect("streaming request should be authorized");

        assert_eq!(
            unary.metadata().get("authorization").unwrap(),
            "Bearer stream-token"
        );
        assert_eq!(
            streaming.metadata().get("authorization").unwrap(),
            "Bearer stream-token"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn stale_waltr_401_does_not_clear_replacement_credentials() {
        let credentials = NativeCredentialProvider::new();
        credentials
            .replace_bearer_token("token-a")
            .expect("first token should install");
        let request = credentials.request_credentials();
        credentials
            .replace_bearer_token_if_current(credentials.credential_lease(), "token-b")
            .expect("replacement token should be valid");

        let error = status_error(&waltr_access_status("expired"), &request);

        assert_eq!(error.kind(), crate::ClientErrorKind::Other);
        assert!(credentials.has_bearer_token());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn current_waltr_401_atomically_clears_credentials() {
        let credentials = NativeCredentialProvider::new();
        credentials
            .replace_bearer_token("current-token")
            .expect("token should install");
        let request = credentials.request_credentials();

        let error = status_error(&waltr_access_status("expired"), &request);

        assert!(error.is_waltr_login_required());
        assert!(!credentials.has_bearer_token());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn token_refresh_advances_revision_without_replacing_lease() {
        let credentials = NativeCredentialProvider::new();
        let lease = credentials.begin_credential_lease();
        credentials
            .replace_bearer_token_if_current(lease, "first-token")
            .expect("first token should be valid");
        let stale_revision = credentials.credential_revision();
        credentials
            .replace_bearer_token_if_current(lease, "refreshed-token")
            .expect("refresh token should be valid");

        assert_eq!(credentials.credential_lease(), lease);
        assert!(credentials.credential_revision() > stale_revision);
        assert!(!credentials.clear_bearer_token_if_current(lease, stale_revision));
        assert!(credentials.has_bearer_token());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    #[allow(clippy::similar_names)]
    async fn native_endpoint_connection_uses_webpki_root_tls_only_for_https() {
        let http = NativeGrpcTransport::new("http://127.0.0.1:8080")
            .native_endpoint()
            .expect("HTTP endpoint should construct");
        let https = NativeGrpcTransport::new("https://waltr.example.test")
            .native_endpoint()
            .expect("HTTPS endpoint should construct");

        let NativeEndpoint::Plaintext(http_endpoint) = &http else {
            panic!("HTTP endpoint must not carry TLS configuration");
        };
        assert_eq!(http_endpoint.uri().scheme_str(), Some("http"));

        let NativeEndpoint::WebPkiRootsTls { endpoint, tls } = &https else {
            panic!("HTTPS endpoint must carry a WebPKI-root TLS configuration");
        };
        assert_eq!(endpoint.uri().scheme_str(), Some("https"));
        let _ = tls;

        let http_bytes = initial_connection_bytes(http).await;
        assert!(http_bytes.starts_with(b"PRI * HTTP/2.0"));

        let https_bytes = initial_connection_bytes(https).await;
        assert_eq!(
            https_bytes.first(),
            Some(&22),
            "HTTPS must emit a TLS handshake"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_https_ignores_controlled_native_root() {
        const CHILD_ENV: &str = "WALTR_CLIENT_NATIVE_TLS_CHILD_ENDPOINT";
        if let Ok(endpoint) = std::env::var(CHILD_ENV) {
            assert!(rustls::crypto::CryptoProvider::get_default().is_none());
            let transport = NativeGrpcTransport::new(endpoint.clone());
            transport
                .native_endpoint()
                .expect("HTTPS endpoint should initialize its crypto provider");
            assert!(rustls::crypto::CryptoProvider::get_default().is_some());
            let error = transport
                .get_config()
                .await
                .expect_err("a certificate trusted only through SSL_CERT_FILE must be rejected");
            assert!(
                error.to_string().contains("certificate")
                    || error.to_string().contains("tls")
                    || error.to_string().contains("transport"),
                "unexpected TLS rejection: {error}"
            );
            #[cfg(feature = "native-access")]
            crate::native_access::test_http_client()
                .get(endpoint)
                .send()
                .await
                .expect_err("reqwest must reject a certificate absent from bundled WebPKI roots");
            return;
        }

        drop(rustls::crypto::ring::default_provider().install_default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TLS fixture listener should bind");
        let address = listener.local_addr().unwrap();
        let incoming = futures_util::stream::unfold(listener, |listener| async move {
            let item = listener.accept().await.map(|(stream, _)| stream);
            Some((item, listener))
        });
        let identity = tonic::transport::Identity::from_pem(
            include_str!("../../tests/fixtures/native-tls-server.pem"),
            include_str!("../../tests/fixtures/native-tls-server-key.pem"),
        );
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
                .expect("TLS fixture identity should configure")
                .add_service(ResourceServiceServer::new(
                    MetadataResourceService::default(),
                ))
                .serve_with_incoming(incoming)
                .await
                .expect("TLS fixture server should run");
        });

        let ca_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/native-tls-ca.pem");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("transport::tests::native_https_ignores_controlled_native_root")
            .arg("--nocapture")
            .env(CHILD_ENV, format!("https://localhost:{}", address.port()))
            .env("SSL_CERT_FILE", ca_path)
            .env_remove("SSL_CERT_DIR")
            .output()
            .expect("TLS child test should start");
        server.abort();

        assert!(
            output.status.success(),
            "TLS child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[derive(Clone)]
    struct DuplexConnector {
        stream: Arc<Mutex<Option<tokio::io::DuplexStream>>>,
    }

    impl tonic::codegen::Service<tonic::codegen::http::Uri> for DuplexConnector {
        type Response = hyper_util::rt::TokioIo<tokio::io::DuplexStream>;
        type Error = std::io::Error;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: tonic::codegen::http::Uri) -> Self::Future {
            std::future::ready(
                self.stream
                    .lock()
                    .unwrap()
                    .take()
                    .map(hyper_util::rt::TokioIo::new)
                    .ok_or_else(|| std::io::Error::other("connector called more than once")),
            )
        }
    }

    async fn initial_connection_bytes(endpoint: NativeEndpoint) -> Vec<u8> {
        use tokio::io::AsyncReadExt;

        let endpoint = endpoint
            .into_tonic_endpoint()
            .expect("endpoint security should configure");
        let (client, mut server) = tokio::io::duplex(4_096);
        let connector = DuplexConnector {
            stream: Arc::new(Mutex::new(Some(client))),
        };
        let connection = endpoint.connect_with_connector(connector);
        let mut bytes = vec![0; 24];
        tokio::pin!(connection);
        tokio::select! {
            result = server.read_exact(&mut bytes) => {
                result.expect("client should write its connection preface");
            }
            result = &mut connection => {
                let _channel = result.expect("plaintext connector should create a channel");
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    server.read_exact(&mut bytes),
                )
                .await
                .expect("channel should write its connection preface")
                .expect("connection preface should be readable");
            }
        }
        bytes
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn native_transport_applies_current_metadata_to_unary_and_reopened_streams() {
        let (transport, service, server) = metadata_transport_server().await;
        transport
            .replace_bearer_token("first-token")
            .expect("first token should install");

        transport
            .get_config()
            .await
            .expect("unary call should succeed");
        let mut first_stream = transport
            .watch_config()
            .await
            .expect("first stream should open");
        first_stream.next().await.unwrap().unwrap();

        transport
            .replace_bearer_token("second-token")
            .expect("replacement token should install");
        transport
            .get_config()
            .await
            .expect("replaced unary should succeed");
        let _ = transport.clear_bearer_token();
        transport
            .get_config()
            .await
            .expect("cleared unary should succeed");

        let lease = transport.begin_credential_lease();
        assert!(
            transport
                .replace_bearer_token_if_current(lease, "stream-token")
                .expect("stream token should be valid")
        );
        let mut reopened = transport
            .watch_config()
            .await
            .expect("stream should reopen");
        reopened.next().await.unwrap().unwrap();

        assert_eq!(
            *service.authorizations.lock().unwrap(),
            vec![
                Some("Bearer first-token".to_owned()),
                Some("Bearer first-token".to_owned()),
                Some("Bearer second-token".to_owned()),
                None,
                Some("Bearer stream-token".to_owned()),
            ]
        );
        server.abort();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_request_credentials_are_fixed_per_rpc_and_refresh_between_rpcs() {
        let transport = NativeGrpcTransport::new("http://127.0.0.1:1");
        let credentials = transport.credential_provider();
        transport
            .replace_bearer_token("first-secret")
            .expect("first token should be valid");
        let mut interceptor = transport.interceptor();
        assert_eq!(
            interceptor
                .call(tonic::Request::new(()))
                .unwrap()
                .metadata()
                .get("authorization")
                .unwrap(),
            "Bearer first-secret"
        );

        credentials
            .replace_bearer_token("second-secret")
            .expect("replacement token should be valid");
        assert_eq!(
            transport
                .interceptor()
                .call(tonic::Request::new(()))
                .unwrap()
                .metadata()
                .get("authorization")
                .unwrap(),
            "Bearer second-secret"
        );

        let _ = transport.clear_bearer_token();
        assert_eq!(
            interceptor
                .call(tonic::Request::new(()))
                .unwrap()
                .metadata()
                .get("authorization")
                .unwrap(),
            "Bearer first-secret"
        );
        assert!(
            transport
                .interceptor()
                .call(tonic::Request::new(()))
                .unwrap()
                .metadata()
                .get("authorization")
                .is_none()
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_credentials_do_not_expose_tokens_in_debug_or_errors() {
        let transport = NativeGrpcTransport::new("http://127.0.0.1:1");
        transport
            .replace_bearer_token("debug-secret")
            .expect("token should be valid");

        let debug = format!("{transport:?}");
        assert!(!debug.contains("debug-secret"));
        assert!(debug.contains("has_bearer_token: true"));

        let invalid = "error-secret\ninvalid";
        let error = transport
            .replace_bearer_token(invalid)
            .expect_err("invalid metadata should fail");
        assert!(!error.to_string().contains("error-secret"));
        assert!(!format!("{error:?}").contains("error-secret"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_credential_leases_reject_stale_login_and_refresh_tasks() {
        let credentials = NativeCredentialProvider::new();
        let stale_login = credentials.begin_credential_lease();
        let current_login = credentials.begin_credential_lease();

        assert!(
            !credentials
                .replace_bearer_token_if_current(stale_login, "stale-login-secret")
                .unwrap()
        );
        assert!(
            credentials
                .replace_bearer_token_if_current(current_login, "current-secret")
                .unwrap()
        );
        let refresh = credentials.credential_lease();
        let cleared = credentials.clear_bearer_token();
        assert!(cleared.generation() > refresh.generation());
        assert!(
            !credentials
                .replace_bearer_token_if_current(refresh, "stale-refresh-secret")
                .unwrap()
        );
        assert!(!credentials.has_bearer_token());

        let debug = format!("{credentials:?}");
        assert!(!debug.contains("stale-login-secret"));
        assert!(!debug.contains("stale-refresh-secret"));
        assert!(!debug.contains("current-secret"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_credential_clear_recovers_from_lock_poisoning() {
        let credentials = NativeCredentialProvider::new();
        credentials
            .replace_bearer_token("poison-secret")
            .expect("token should install");
        let poisoned = credentials.clone();
        drop(
            std::thread::spawn(move || {
                let _guard = poisoned.state.write().unwrap();
                panic!("poison credential lock");
            })
            .join(),
        );

        let lease = credentials.clear_bearer_token();

        assert!(!credentials.has_bearer_token());
        assert_eq!(credentials.credential_lease(), lease);
        assert!(!format!("{credentials:?}").contains("poison-secret"));
    }
}
