//! Session service coordination.

use crate::config::ConfigHandle;
use futures_util::Stream;
use waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer;
use waltr_agent::{
    AgentDefinitionCatalog, AgentSession, AgentSessionActiveRunState, AgentSessionRunError,
    AgentSessionRunSubmission, CredentialStore, FilesystemSessionStore, MemorySessionStore,
    SessionCoordinator, SessionId, SessionListUpdate, SessionStore, SessionSummary,
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
