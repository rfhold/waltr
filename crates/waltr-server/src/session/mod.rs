//! Session service coordination.

use crate::{agent_definition::AgentDefinitionState, config::ConfigHandle};
use futures_util::Stream;
use waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer;
use waltr_agent::{
    AgentDefinitionCatalog, AgentRunLimits, AgentSession, AgentSessionActiveRunState,
    AgentSessionRunError, AgentSessionRunSubmission, CompletedSessionCapture, CredentialStore,
    FilesystemSessionStore, MemorySessionStore, MetadataSetCondition, MetadataSetOutcome,
    SessionCaptureError, SessionCompletionCoordinationHandler, SessionCoordinationReason,
    SessionCoordinator, SessionCreateRequest, SessionEventSink, SessionId, SessionListUpdate,
    SessionMaskMutation, SessionMaskMutationOutcome, SessionPauseError, SessionPauseLease,
    SessionStore, SessionSummary, TimelineCursor,
};
use std::{pin::Pin, sync::Arc};
use tonic::service::RoutesBuilder;

pub mod service;
pub mod storage;

mod convert;
mod error;
mod inference_settings;
mod timeline_convert;

pub use service::SessionService;
pub use storage::SessionStorageConfigError;

use storage::SessionStorageConfig;

/// Object-safe access to the coordinator owned by the registered gRPC session service.
#[tonic::async_trait]
pub trait SessionHost: Send + Sync {
    /// Install or remove synchronous terminal-run admission.
    fn set_completion_admission_handler(
        &self,
        handler: Option<Arc<dyn SessionCompletionCoordinationHandler>>,
    );
    /// Create a durable session using server inference defaults and an optional agent selection.
    async fn create(&self, agent_name: Option<String>) -> Result<AgentSession, tonic::Status>;
    /// Create a durable session from an exact host-owned request.
    async fn create_exact(
        &self,
        request: SessionCreateRequest,
    ) -> Result<AgentSession, waltr_agent::LlmError>;
    /// Capture one immutable completed source history seed.
    async fn capture_latest_completed(
        &self,
        id: &SessionId,
    ) -> Result<CompletedSessionCapture, SessionCaptureError>;
    /// Acquire an exclusive process-local pause without waiting for active work.
    async fn acquire_pause_reject_busy(
        &self,
        id: &SessionId,
        reason: SessionCoordinationReason,
    ) -> Result<SessionPauseLease, SessionPauseError>;
    /// Capture mask-effective context through the boundary owned by `lease`.
    async fn capture_paused_effective(
        &self,
        lease: &SessionPauseLease,
    ) -> Result<CompletedSessionCapture, SessionPauseError>;
    /// Apply one atomic mask mutation through the pause's pinned boundary.
    async fn apply_paused_mask_mutation(
        &self,
        lease: &SessionPauseLease,
        mutation: SessionMaskMutation,
    ) -> Result<SessionMaskMutationOutcome, SessionPauseError>;
    /// Arm a continuation from the mutation cursor committed by this pause.
    async fn arm_paused_continuation(
        &self,
        lease: &SessionPauseLease,
        mutation_cursor: TimelineCursor,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, SessionPauseError>;
    /// Release an owned pause and drain its privileged and ordinary queues.
    async fn release_pause(&self, lease: &SessionPauseLease) -> Result<(), SessionPauseError>;
    /// Start an already durable prompt with ordinary replay semantics.
    async fn start_durable_prompt(
        &self,
        id: SessionId,
        limits: AgentRunLimits,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError>;
    /// List durable sessions.
    async fn list(&self) -> Result<Vec<SessionSummary>, waltr_agent::LlmError>;
    /// Load one durable session.
    async fn load(&self, id: &SessionId) -> Result<AgentSession, waltr_agent::LlmError>;
    /// Delete every durable session and clear cached MCP runtimes.
    async fn wipe(&self) -> Result<(), waltr_agent::LlmError>;
    /// Interrupt one session through its coordinator.
    async fn interrupt(&self, id: &SessionId) -> Result<AgentSession, waltr_agent::LlmError>;
    /// Read coordinator-local active-run and queue state.
    async fn active_state(&self, id: &SessionId) -> Option<AgentSessionActiveRunState>;
    /// Submit a prompt using queue-if-active admission.
    async fn submit_prompt(
        &self,
        id: SessionId,
        prompt: String,
    ) -> Result<AgentSessionRunSubmission, AgentSessionRunError>;
    /// Atomically set one metadata value.
    async fn set_metadata_value_conditionally(
        &self,
        id: &SessionId,
        key: String,
        value: serde_json::Value,
        condition: MetadataSetCondition,
    ) -> Result<MetadataSetOutcome, waltr_agent::LlmError>;
    /// Subscribe to coordinator-local session updates.
    fn watch_sessions(&self) -> Pin<Box<dyn Stream<Item = SessionListUpdate> + Send>>;
}

/// Add the configured session service to the gRPC routes builder.
///
/// # Errors
///
/// Returns an error when session storage environment configuration is invalid.
pub fn add_service_from_env(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origin: Option<reqwest::Url>,
    agent_catalog: AgentDefinitionCatalog,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    add_service_from_env_with_oauth_policy(
        routes,
        config,
        credential_store,
        trusted_mcp_oauth_origin.into_iter().collect(),
        agent_catalog,
    )
}

pub(crate) fn add_service_from_env_with_oauth_policy(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_catalog: AgentDefinitionCatalog,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    let host = match SessionStorageConfig::from_env()? {
        SessionStorageConfig::Memory => add_store_service(
            routes,
            MemorySessionStore::new(),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_catalog,
        ),
        SessionStorageConfig::Filesystem { root } => add_store_service(
            routes,
            FilesystemSessionStore::new(root),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_catalog,
        ),
        SessionStorageConfig::S3(s3_config) => add_store_service(
            routes,
            s3_config.into_store(),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_catalog,
        ),
    };

    Ok(host)
}

/// Add the configured session service backed by live agent-definition state.
///
/// # Errors
///
/// Returns an error when session storage environment configuration is invalid.
pub fn add_service_from_env_with_agent_state(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origin: Option<reqwest::Url>,
    agent_state: AgentDefinitionState,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    add_service_from_env_with_oauth_policy_and_agent_state(
        routes,
        config,
        credential_store,
        trusted_mcp_oauth_origin.into_iter().collect(),
        agent_state,
    )
}

pub(crate) fn add_service_from_env_with_oauth_policy_and_agent_state(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_state: AgentDefinitionState,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    let host = match SessionStorageConfig::from_env()? {
        SessionStorageConfig::Memory => add_store_service_with_agent_state(
            routes,
            MemorySessionStore::new(),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_state,
        ),
        SessionStorageConfig::Filesystem { root } => add_store_service_with_agent_state(
            routes,
            FilesystemSessionStore::new(root),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_state,
        ),
        SessionStorageConfig::S3(s3_config) => add_store_service_with_agent_state(
            routes,
            s3_config.into_store(),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_state,
        ),
    };

    Ok(host)
}

#[allow(
    clippy::too_many_arguments,
    reason = "session construction receives independent host dependencies"
)]
pub(crate) fn add_service_from_env_with_oauth_policy_agent_state_and_event_sink(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_state: AgentDefinitionState,
    event_sink: Arc<dyn SessionEventSink>,
) -> Result<Arc<dyn SessionHost>, SessionStorageConfigError> {
    let host = match SessionStorageConfig::from_env()? {
        SessionStorageConfig::Memory => add_store_service_with_agent_state_and_event_sink(
            routes,
            MemorySessionStore::new(),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_state,
            event_sink,
        ),
        SessionStorageConfig::Filesystem { root } => {
            add_store_service_with_agent_state_and_event_sink(
                routes,
                FilesystemSessionStore::new(root),
                config,
                credential_store,
                trusted_mcp_oauth_origins,
                agent_state,
                event_sink,
            )
        }
        SessionStorageConfig::S3(s3_config) => add_store_service_with_agent_state_and_event_sink(
            routes,
            s3_config.into_store(),
            config,
            credential_store,
            trusted_mcp_oauth_origins,
            agent_state,
            event_sink,
        ),
    };
    Ok(host)
}

/// Add an in-memory session service using an explicit config store.
pub fn add_memory_service(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    agent_catalog: AgentDefinitionCatalog,
) -> Arc<dyn SessionHost> {
    add_store_service(
        routes,
        MemorySessionStore::new(),
        config,
        credential_store,
        Vec::new(),
        agent_catalog,
    )
}

/// Add an in-memory session service backed by live agent-definition state.
pub fn add_memory_service_with_agent_state(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
    agent_state: AgentDefinitionState,
) -> Arc<dyn SessionHost> {
    add_store_service_with_agent_state(
        routes,
        MemorySessionStore::new(),
        config,
        credential_store,
        Vec::new(),
        agent_state,
    )
}

/// Add an in-memory session service using an existing session coordinator.
#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
pub fn add_sessions_service<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origin: Option<reqwest::Url>,
    agent_catalog: AgentDefinitionCatalog,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    add_sessions_service_with_oauth_policy(
        routes,
        sessions,
        config,
        credential_store,
        trusted_mcp_oauth_origin.into_iter().collect(),
        agent_catalog,
    )
}

/// Add a session service using an existing coordinator and live agent-definition state.
#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
pub fn add_sessions_service_with_agent_state<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origin: Option<reqwest::Url>,
    agent_state: AgentDefinitionState,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    add_sessions_service_with_oauth_policy_and_agent_state(
        routes,
        sessions,
        config,
        credential_store,
        trusted_mcp_oauth_origin.into_iter().collect(),
        agent_state,
    )
}

/// Add a session service using an existing coordinator, live definitions, and semantic event sink.
#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
pub(crate) fn add_sessions_service_with_agent_state_and_event_sink<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_state: AgentDefinitionState,
    event_sink: Arc<dyn SessionEventSink>,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    add_sessions_service_with_oauth_policy_and_agent_state(
        routes,
        sessions.with_session_event_sink(event_sink),
        config,
        credential_store,
        trusted_mcp_oauth_origins,
        agent_state,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
pub(crate) fn add_sessions_service_with_oauth_policy<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_catalog: AgentDefinitionCatalog,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let service = SessionService::from_sessions_with_oauth_policy(
        sessions,
        config,
        credential_store,
        trusted_mcp_oauth_origins,
        agent_catalog,
    );
    routes.add_service(SessionServiceServer::from_arc(Arc::clone(&service)));
    let host: Arc<dyn SessionHost> = service;
    host
}

#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
pub(crate) fn add_sessions_service_with_oauth_policy_and_agent_state<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_state: AgentDefinitionState,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let service = SessionService::from_sessions_with_oauth_policy_and_agent_state(
        sessions,
        config,
        credential_store,
        trusted_mcp_oauth_origins,
        agent_state,
    );
    routes.add_service(SessionServiceServer::from_arc(Arc::clone(&service)));
    let host: Arc<dyn SessionHost> = service;
    host
}

#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
fn add_store_service<S>(
    routes: &mut RoutesBuilder,
    store: S,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_catalog: AgentDefinitionCatalog,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let service = SessionService::new_with_oauth_policy(
        store,
        config,
        credential_store,
        trusted_mcp_oauth_origins,
        agent_catalog,
    );
    routes.add_service(SessionServiceServer::from_arc(Arc::clone(&service)));
    let host: Arc<dyn SessionHost> = service;
    host
}

#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
fn add_store_service_with_agent_state<S>(
    routes: &mut RoutesBuilder,
    store: S,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_state: AgentDefinitionState,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let service = SessionService::new_with_oauth_policy_and_agent_state(
        store,
        config,
        credential_store,
        trusted_mcp_oauth_origins,
        agent_state,
    );
    routes.add_service(SessionServiceServer::from_arc(Arc::clone(&service)));
    let host: Arc<dyn SessionHost> = service;
    host
}

#[allow(
    clippy::too_many_arguments,
    reason = "service registration receives independent coordinator dependencies"
)]
pub(crate) fn add_store_service_with_agent_state_and_event_sink<S>(
    routes: &mut RoutesBuilder,
    store: S,
    config: ConfigHandle,
    credential_store: CredentialStore,
    trusted_mcp_oauth_origins: Vec<reqwest::Url>,
    agent_state: AgentDefinitionState,
    event_sink: Arc<dyn SessionEventSink>,
) -> Arc<dyn SessionHost>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    add_sessions_service_with_agent_state_and_event_sink(
        routes,
        SessionCoordinator::new(store),
        config,
        credential_store,
        trusted_mcp_oauth_origins,
        agent_state,
        event_sink,
    )
}
