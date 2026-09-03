//! HTTP and gRPC router construction.

use crate::{
    AgentDefinitionLoadError,
    access_auth::{
        AccessAuthConfig, AccessAuthConfigError, AccessAuthError, AccessAuthHttpClient,
        AccessAuthenticator, GrpcAccessAuthLayer, GrpcTransportLayer,
    },
    agent_definition::{AgentDefinitionState, AgentDefinitionWatcher, DefinitionWatcherHealth},
    auth::{
        AuthService, CredentialStore, OutboundOAuthConfig, TrustedPrivateOriginsError,
        trusted_private_origins_from_env,
    },
    catalog::{CatalogLoader, CatalogService},
    config::ConfigHandle,
    config_service::ConfigService,
    local_admission::{LocalBearer, LocalCredentialStore, require_local_protocol_bearer},
    mcp_oauth::{McpOAuthConfig, McpOAuthConfigError, McpOAuthError, McpOAuthServer},
    mcp_service::WaltrMcpServer,
    persistence::{Persistence, PersistenceError},
    resource_error::ResourceError,
    routine::{EngineHealth, EventBus, RoutineEngine},
    routine_service::RoutineService,
    session,
    status_service::StatusService,
};
use axum::middleware;
use axum::{
    Json, Router,
    http::Request,
    response::IntoResponse as _,
    routing::{any, get},
};
use waltr::grpc::waltr::auth::v1::auth_service_server::AuthServiceServer;
use waltr::grpc::waltr::catalog::v1::catalog_service_server::CatalogServiceServer;
use waltr::grpc::waltr::resource::v1::resource_service_server::ResourceServiceServer;
use waltr::grpc::waltr::routine::v1::routine_service_server::RoutineServiceServer;
use waltr::grpc::waltr::status::v1::status_service_server::StatusServiceServer;
use serde::Serialize;
use std::{error::Error, fmt, future::Future, io, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tonic::service::{Routes, RoutesBuilder};
use tonic_otel_layer::MetricsLayerBuilder;
use tonic_tracing_opentelemetry::middleware::server::OtelGrpcLayer;
use tonic_web::GrpcWebLayer;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct LocalAdmission {
    pub(crate) bearer: LocalBearer,
    pub(crate) self_mcp_url: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Clone, Debug)]
struct Readiness {
    persistence: Persistence,
    oauth: Option<McpOAuthServer>,
    definitions: tokio::sync::watch::Receiver<DefinitionWatcherHealth>,
    routines: tokio::sync::watch::Receiver<EngineHealth>,
}

impl Readiness {
    async fn is_ready(&self) -> bool {
        let definitions_ready = definition_watcher_ready(&self.definitions);
        definitions_ready
            && routine_engine_ready(&self.routines)
            && self.persistence.ready().await.is_ok()
            && match &self.oauth {
                Some(oauth) => oauth.ready().await.is_ok(),
                None => true,
            }
    }

    async fn response(self) -> axum::response::Response {
        if self.is_ready().await {
            Json(HealthResponse { status: "ready" }).into_response()
        } else {
            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn definition_watcher_ready(
    health: &tokio::sync::watch::Receiver<DefinitionWatcherHealth>,
) -> bool {
    health.has_changed().is_ok() && matches!(&*health.borrow(), DefinitionWatcherHealth::Running)
}

fn routine_engine_ready(health: &tokio::sync::watch::Receiver<EngineHealth>) -> bool {
    health.has_changed().is_ok() && matches!(&*health.borrow(), EngineHealth::Running)
}

/// Waltr router and server-owned background work.
pub struct ServerApplication {
    router: Router,
    definition_watcher: Option<AgentDefinitionWatcher>,
    oauth_maintenance: Option<McpOAuthMaintenanceTask>,
    routine_engine: Option<RoutineEngine>,
    session_host: Option<Arc<dyn session::SessionHost>>,
    readiness: Option<Readiness>,
    test_background: Option<TestBackgroundTask>,
}

impl fmt::Debug for ServerApplication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerApplication")
            .finish_non_exhaustive()
    }
}

impl ServerApplication {
    fn new(
        router: Router,
        definition_watcher: AgentDefinitionWatcher,
        oauth: Option<McpOAuthServer>,
        routine_engine: RoutineEngine,
        session_host: Arc<dyn session::SessionHost>,
        readiness: Readiness,
    ) -> Self {
        Self {
            router,
            definition_watcher: Some(definition_watcher),
            oauth_maintenance: oauth.map(McpOAuthMaintenanceTask::spawn),
            routine_engine: Some(routine_engine),
            session_host: Some(session_host),
            readiness: Some(readiness),
            test_background: None,
        }
    }

    /// Clone the router for in-process request testing.
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Check all server-owned readiness dependencies directly.
    async fn ready(&self) -> bool {
        let Some(readiness) = self.readiness.clone() else {
            return false;
        };
        readiness.is_ready().await
    }

    /// Wait until all server-owned readiness dependencies are ready.
    pub async fn wait_until_ready(&self) {
        loop {
            if self.ready().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Cancel and join all server-owned background work.
    pub async fn shutdown(self) {
        let Self {
            router,
            definition_watcher,
            oauth_maintenance,
            routine_engine,
            session_host,
            readiness: _,
            test_background,
        } = self;
        drop(router);
        shutdown_background_tasks(
            definition_watcher,
            oauth_maintenance,
            routine_engine,
            session_host,
            test_background,
        )
        .await;
    }

    #[cfg(any(debug_assertions, feature = "test"))]
    pub(crate) fn with_background_task_for_test(
        cancellation: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self::with_router_and_background_task_for_test(Router::new(), cancellation, task)
    }

    #[cfg(any(debug_assertions, feature = "test"))]
    pub(crate) fn with_router_and_background_task_for_test(
        router: Router,
        cancellation: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            router,
            definition_watcher: None,
            oauth_maintenance: None,
            routine_engine: None,
            session_host: None,
            readiness: None,
            test_background: Some(TestBackgroundTask { cancellation, task }),
        }
    }

    /// Bind the server listener, shutting down background work if startup fails.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener cannot bind or report its local address.
    pub async fn bind(
        self,
        addr: std::net::SocketAddr,
    ) -> io::Result<(Self, tokio::net::TcpListener, std::net::SocketAddr)> {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(error) => {
                self.shutdown().await;
                return Err(error);
            }
        };
        let bound_addr = match listener.local_addr() {
            Ok(bound_addr) => bound_addr,
            Err(error) => {
                self.shutdown().await;
                return Err(error);
            }
        };
        Ok((self, listener, bound_addr))
    }

    /// Serve until the listener fails or the shutdown signal resolves, then join background work.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP server fails.
    pub async fn serve_with_shutdown<F>(
        self,
        listener: tokio::net::TcpListener,
        shutdown: F,
    ) -> io::Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Self {
            router,
            definition_watcher,
            oauth_maintenance,
            routine_engine,
            session_host,
            readiness: _,
            test_background,
        } = self;
        let result = {
            let (shutdown_started_tx, shutdown_started_rx) = tokio::sync::oneshot::channel();
            let server = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                shutdown.await;
                let _ = shutdown_started_tx.send(());
            })
            .into_future();
            tokio::pin!(server);
            tokio::select! {
                result = &mut server => result,
                () = async {
                    if shutdown_started_rx.await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
                } => Ok(()),
            }
        };
        shutdown_background_tasks(
            definition_watcher,
            oauth_maintenance,
            routine_engine,
            session_host,
            test_background,
        )
        .await;
        result
    }
}

async fn shutdown_background_tasks(
    definition_watcher: Option<AgentDefinitionWatcher>,
    oauth_maintenance: Option<McpOAuthMaintenanceTask>,
    routine_engine: Option<RoutineEngine>,
    session_host: Option<Arc<dyn session::SessionHost>>,
    test_background: Option<TestBackgroundTask>,
) {
    if let Some(routine_engine) = routine_engine {
        routine_engine.shutdown().await;
    }
    if let Some(session_host) = session_host {
        session_host.shutdown().await;
    }
    if let Some(definition_watcher) = definition_watcher
        && let Err(error) = definition_watcher.shutdown().await
    {
        tracing::warn!(%error, "agent-definition watcher failed to join");
    }
    if let Some(task) = &oauth_maintenance {
        task.cancellation.cancel();
    }
    if let Some(oauth_maintenance) = oauth_maintenance {
        oauth_maintenance.shutdown().await;
    }
    if let Some(test_background) = test_background {
        test_background.shutdown().await;
    }
}

struct TestBackgroundTask {
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl TestBackgroundTask {
    async fn shutdown(self) {
        self.cancellation.cancel();
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "test background task failed to join");
        }
    }
}

struct McpOAuthMaintenanceTask {
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl McpOAuthMaintenanceTask {
    fn spawn(server: McpOAuthServer) -> Self {
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let health_server = server.clone();
        let task = tokio::spawn(async move {
            struct HealthGuard(McpOAuthServer);
            impl Drop for HealthGuard {
                fn drop(&mut self) {
                    self.0.mark_maintenance_unhealthy();
                }
            }
            let _health = HealthGuard(health_server);
            let mut cleanup = tokio::time::interval(crate::mcp_oauth::oauth_cleanup_interval());
            cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut rotation = tokio::time::interval(crate::mcp_oauth::signing_rotation_interval());
            rotation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            cleanup.tick().await;
            rotation.tick().await;
            loop {
                let rotate = tokio::select! {
                    biased;
                    () = task_cancellation.cancelled() => return,
                    _ = rotation.tick() => true,
                    _ = cleanup.tick() => false,
                };
                if let Err(error) = server.maintain(rotate).await {
                    tracing::warn!(%error, rotate, "MCP OAuth maintenance failed");
                }
            }
        });
        Self {
            cancellation,
            task: Some(task),
        }
    }

    async fn shutdown(mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "MCP OAuth maintenance task failed to join");
        }
    }
}

impl Drop for McpOAuthMaintenanceTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Failure to construct the environment-owned application.
pub enum BuildAppError {
    /// Agent definitions could not be loaded or resolved.
    AgentDefinition(AgentDefinitionLoadError),
    /// Filesystem-backed configuration could not start.
    Config(ResourceError),
    /// Server persistence could not initialize.
    Persistence(PersistenceError),
    /// Agent session storage configuration is invalid.
    SessionStorage(session::SessionStorageConfigError),
    /// Access-authentication environment configuration is invalid.
    AccessAuthConfig(AccessAuthConfigError),
    /// The bounded OIDC HTTP client could not be constructed.
    AccessAuthClient(reqwest::Error),
    /// OIDC discovery or initial key loading failed.
    AccessAuthDiscovery(AccessAuthError),
    /// Access-authentication persistence could not initialize.
    AccessAuthStore(AccessAuthError),
    /// MCP OAuth environment configuration is invalid.
    McpOAuthConfig(McpOAuthConfigError),
    /// Outbound MCP OAuth private-origin configuration is invalid.
    OutboundOAuthConfig(TrustedPrivateOriginsError),
    /// MCP OAuth persistence, wrapping keys, or signing state could not initialize.
    McpOAuth(McpOAuthError),
}

impl fmt::Debug for BuildAppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for BuildAppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentDefinition(error) => {
                write!(formatter, "failed to initialize agent definitions: {error}")
            }
            Self::Config(error) => write!(formatter, "failed to initialize server config: {error}"),
            Self::Persistence(error) => error.fmt(formatter),
            Self::SessionStorage(error) => error.fmt(formatter),
            Self::AccessAuthConfig(_) => {
                formatter.write_str("access authentication configuration is invalid")
            }
            Self::AccessAuthClient(_) => {
                formatter.write_str("failed to initialize access authentication HTTP client")
            }
            Self::AccessAuthDiscovery(_) => {
                formatter.write_str("failed to initialize OIDC provider discovery")
            }
            Self::AccessAuthStore(_) => {
                formatter.write_str("failed to initialize access authentication persistence")
            }
            Self::McpOAuthConfig(_) => formatter.write_str("MCP OAuth configuration is invalid"),
            Self::OutboundOAuthConfig(_) => {
                formatter.write_str("outbound MCP OAuth configuration is invalid")
            }
            Self::McpOAuth(_) => formatter.write_str("failed to initialize MCP OAuth server"),
        }
    }
}

impl Error for BuildAppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AgentDefinition(error) => Some(error),
            Self::Config(error) => Some(error),
            Self::Persistence(error) => Some(error),
            Self::SessionStorage(error) => Some(error),
            Self::AccessAuthConfig(error) => Some(error),
            Self::AccessAuthClient(error) => Some(error),
            Self::AccessAuthDiscovery(error) | Self::AccessAuthStore(error) => Some(error),
            Self::McpOAuthConfig(error) => Some(error),
            Self::OutboundOAuthConfig(error) => Some(error),
            Self::McpOAuth(error) => Some(error),
        }
    }
}

/// Build the Axum application from environment-owned dependencies.
///
/// # Errors
///
/// Returns an error when persistence, config, authentication, or agent session storage cannot initialize.
///
/// # Panics
///
/// Panics if the application builder does not invoke the session-service callback.
pub async fn build_app() -> Result<ServerApplication, BuildAppError> {
    tracing::debug!("building app with config service from current directory");
    let persistence = Persistence::from_env()
        .await
        .map_err(BuildAppError::Persistence)?;
    let catalog_loader = CatalogLoader::from_env();
    let config_service = ConfigService::from_current_dir(catalog_loader.clone())
        .await
        .map_err(BuildAppError::Config)?;
    let agent_state = config_service.state().clone();
    let access_auth_config =
        AccessAuthConfig::from_env().map_err(BuildAppError::AccessAuthConfig)?;
    let mcp_oauth_config =
        McpOAuthConfig::from_env(&access_auth_config).map_err(BuildAppError::McpOAuthConfig)?;
    let outbound_private_origins =
        trusted_private_origins_from_env().map_err(BuildAppError::OutboundOAuthConfig)?;
    let access_auth_client = build_access_auth_client()?;
    let access_authenticator =
        AccessAuthenticator::initialize(access_auth_config, &persistence, access_auth_client)
            .await
            .map_err(|error| match error {
                AccessAuthError::Persistence(_) => BuildAppError::AccessAuthStore(error),
                _ => BuildAppError::AccessAuthDiscovery(error),
            })?;
    let mcp_oauth =
        initialize_mcp_oauth(mcp_oauth_config, &persistence, access_authenticator.clone()).await?;
    let automatic_self_mcp_resource = mcp_oauth
        .as_ref()
        .and_then(|oauth| reqwest::Url::parse(oauth.resource()).ok());
    let session_private_origins = agent_trusted_origins(
        outbound_private_origins.clone(),
        automatic_self_mcp_resource.as_ref(),
    );
    let mut session_storage = None;
    let router = std::future::ready(
        build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth_policy(
            config_service,
            persistence,
            catalog_loader,
            &access_authenticator,
            mcp_oauth,
            outbound_private_origins.clone(),
            OutboundOAuthConfig::from_env(),
            automatic_self_mcp_resource,
            Some(agent_state),
            None,
            |grpc_routes, config, credential_store, agent_state, event_sink| {
                match session::add_service_from_env_with_oauth_policy_agent_state_and_event_sink(
                    grpc_routes,
                    config.clone(),
                    credential_store.clone(),
                    session_private_origins,
                    agent_state.clone(),
                    event_sink.clone(),
                ) {
                    Ok(host) => {
                        session_storage = Some(Ok(Arc::clone(&host)));
                        host
                    }
                    Err(error) => {
                        session_storage = Some(Err(error));
                        session::add_store_service_with_agent_state_and_event_sink(
                            grpc_routes,
                            waltr_agent::MemorySessionStore::new(),
                            config,
                            credential_store,
                            Vec::new(),
                            agent_state,
                            event_sink,
                        )
                    }
                }
            },
        ),
    )
    .await
    .map_err(BuildAppError::Config)?;
    session_storage
        .expect("session service builder should run")
        .map_err(BuildAppError::SessionStorage)?;
    Ok(router)
}

async fn initialize_mcp_oauth(
    config: McpOAuthConfig,
    persistence: &Persistence,
    access_authenticator: AccessAuthenticator,
) -> Result<Option<McpOAuthServer>, BuildAppError> {
    match config {
        McpOAuthConfig::Disabled => Ok(None),
        McpOAuthConfig::Enabled(config) => {
            McpOAuthServer::initialize(config, persistence, access_authenticator)
                .await
                .map(Some)
                .map_err(BuildAppError::McpOAuth)
        }
    }
}

fn build_access_auth_client() -> Result<AccessAuthHttpClient, BuildAppError> {
    AccessAuthHttpClient::build(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10)),
    )
    .map_err(BuildAppError::AccessAuthClient)
}

pub(crate) fn agent_trusted_origins(
    mut configured: Vec<reqwest::Url>,
    server_resource: Option<&reqwest::Url>,
) -> Vec<reqwest::Url> {
    if let Some(server_resource) = server_resource
        && !configured
            .iter()
            .any(|origin| origin.origin() == server_resource.origin())
    {
        configured.push(server_resource.clone());
    }
    configured
}

/// Build the Axum application using explicit services.
///
/// # Errors
///
/// Returns an error when persisted config cannot be loaded before router construction.
pub async fn build_app_with_services<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    let catalog_loader = config_service.catalog_loader();
    build_app_with_catalog_loader_and_access_authenticator(
        config_service,
        persistence,
        catalog_loader,
        AccessAuthenticator::disabled(),
        add_session_service,
    )
    .await
}

/// Build the Axum application with an initialized access authenticator.
///
/// # Errors
///
/// Returns an error when persisted config cannot be loaded before router construction.
pub async fn build_app_with_access_authenticator<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    access_authenticator: AccessAuthenticator,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    let catalog_loader = config_service.catalog_loader();
    build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        catalog_loader,
        access_authenticator,
        None,
        add_session_service,
    )
    .await
}

/// Build the application with initialized access authentication and hosted MCP OAuth.
///
/// # Errors
///
/// Returns an error when persisted config cannot be loaded before router construction.
pub async fn build_app_with_access_authenticator_and_mcp_oauth<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    access_authenticator: AccessAuthenticator,
    mcp_oauth: Option<McpOAuthServer>,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    let catalog_loader = config_service.catalog_loader();
    build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        catalog_loader,
        access_authenticator,
        mcp_oauth,
        add_session_service,
    )
    .await
}

/// Build the Axum application using explicit services and catalog loader.
///
/// # Errors
///
/// Returns an error when persisted config cannot be loaded before router construction.
pub async fn build_app_with_catalog_loader<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    build_app_with_catalog_loader_and_access_authenticator(
        config_service,
        persistence,
        catalog_loader,
        AccessAuthenticator::disabled(),
        add_session_service,
    )
    .await
}

/// Build the Axum application using explicit services, catalog, and access authentication.
///
/// # Errors
///
/// Returns an error when persisted config cannot be loaded before router construction.
///
/// # Panics
///
/// Panics if enabled access authentication produces invalid MCP authorization metadata.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "router assembly consumes initialized services in dependency order"
)]
pub async fn build_app_with_catalog_loader_and_access_authenticator<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    access_authenticator: AccessAuthenticator,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        catalog_loader,
        access_authenticator,
        None,
        add_session_service,
    )
    .await
}

/// Build the application with explicit catalog, access auth, and hosted MCP OAuth.
///
/// # Errors
///
/// Returns an error when persisted config cannot be loaded before router construction.
///
/// # Panics
///
/// Panics only if initialized hosted OAuth metadata violates the generic MCP contract.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "router assembly consumes initialized services in dependency order"
)]
pub async fn build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    access_authenticator: AccessAuthenticator,
    mcp_oauth: Option<McpOAuthServer>,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    let automatic_self_mcp_resource = mcp_oauth
        .as_ref()
        .and_then(|oauth| reqwest::Url::parse(oauth.resource()).ok());
    std::future::ready(
        build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth_policy(
            config_service,
            persistence,
            catalog_loader,
            &access_authenticator,
            mcp_oauth,
            Vec::new(),
            OutboundOAuthConfig::from_env(),
            automatic_self_mcp_resource,
            None,
            None,
            add_session_service,
        ),
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "router assembly consumes initialized services and OAuth policy in dependency order"
)]
pub(crate) fn build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth_policy<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    access_authenticator: &AccessAuthenticator,
    mcp_oauth: Option<McpOAuthServer>,
    outbound_private_origins: Vec<reqwest::Url>,
    outbound_oauth: OutboundOAuthConfig,
    automatic_self_mcp_resource: Option<reqwest::Url>,
    agent_state: Option<AgentDefinitionState>,
    local_admission: Option<LocalAdmission>,
    add_session_service: F,
) -> Result<ServerApplication, ResourceError>
where
    F: FnOnce(
        &mut RoutesBuilder,
        ConfigHandle,
        CredentialStore,
        AgentDefinitionState,
        Arc<dyn waltr_agent::SessionEventSink>,
    ) -> Arc<dyn session::SessionHost>,
{
    tracing::debug!("building app router");
    let agent_state = agent_state.unwrap_or_else(|| config_service.state().clone());
    let config = ConfigHandle::from_resources(agent_state.clone());
    let outbound_oauth_store = persistence.outbound_oauth_store();
    let credential_store = match &local_admission {
        Some(local) => CredentialStore::new(LocalCredentialStore::new(
            outbound_oauth_store.clone(),
            &local.bearer,
            &local.self_mcp_url,
            outbound_oauth
                .durable_callback_marker()
                .map(ToOwned::to_owned),
        )),
        None => CredentialStore::shared(outbound_oauth_store.clone()),
    };
    let auth_service = AuthService::new(
        agent_state.clone(),
        credential_store.clone(),
        outbound_oauth_store,
        outbound_private_origins,
        automatic_self_mcp_resource,
        outbound_oauth,
    );
    let outbound_oauth_router = auth_service.router();
    let catalog_service = CatalogService::new(agent_state.clone(), catalog_loader);
    // Waltr has no browser MCP client, so the generic empty Origin allowlist stays in force.
    let mut mcp_options =
        mcp::server::StreamableHttpOptions::default().without_root_protected_resource_metadata();
    if let Some(oauth) = &mcp_oauth {
        let required_scopes = oauth.required_scopes().to_vec();
        let metadata = mcp::McpProtectedResourceMetadata::new(oauth.resource(), [oauth.issuer()])
            .with_scopes(required_scopes.clone())
            .with_resource_name("Waltr MCP");
        let hosted = oauth.authorization().clone();
        let authorization =
            mcp::server::StreamableHttpAuthorization::hosted(metadata, move |token, context| {
                hosted.authorize_token(token, context)
            })
            .expect("hosted OAuth should produce valid MCP metadata")
            .with_required_scopes(required_scopes);
        mcp_options = mcp_options.with_authorization(authorization);
    }
    let mut grpc_routes = Routes::builder();
    let event_bus = EventBus::default();
    let session_host = add_session_service(
        &mut grpc_routes,
        config,
        credential_store,
        agent_state.clone(),
        Arc::new(event_bus.clone()),
    );
    let waltr_mcp = Arc::new(WaltrMcpServer::new(
        session_host.clone(),
        agent_state.clone(),
    ));
    let definition_watcher = agent_state
        .spawn_watcher()
        .map_err(|_| ResourceError::unavailable("resource watcher could not start"))?;
    let routine_store = persistence.routine_invocation_store();
    let routine_engine = RoutineEngine::start(
        &event_bus,
        agent_state.clone(),
        session_host.clone(),
        routine_store.clone(),
    );
    let routine_service = RoutineService::new(
        agent_state,
        routine_engine.manual_handle(),
        routine_store,
        routine_engine.lifecycle_wakeups(),
    );
    let readiness = Readiness {
        persistence,
        oauth: mcp_oauth.clone(),
        definitions: definition_watcher.health(),
        routines: routine_engine.health(),
    };
    let readiness_route = readiness.clone();
    let mcp_router = if access_authenticator.enabled() && mcp_oauth.is_none() {
        Router::new()
            .route("/mcp", any(service_unavailable))
            .route("/.well-known/oauth-protected-resource/mcp", get(not_found))
    } else {
        mcp::server::streamable_http_router_with_options(waltr_mcp, mcp_options)
    };
    grpc_routes.add_service(ResourceServiceServer::from_arc(config_service));
    grpc_routes.add_service(AuthServiceServer::from_arc(auth_service));
    grpc_routes.add_service(CatalogServiceServer::from_arc(catalog_service));
    grpc_routes.add_service(RoutineServiceServer::new(routine_service));
    grpc_routes.add_service(StatusServiceServer::new(StatusService));
    let grpc_routes = grpc_routes.routes();
    let grpc = ServiceBuilder::new()
        .layer(OtelGrpcLayer::default())
        .layer(MetricsLayerBuilder::new().build())
        .layer(
            ServiceBuilder::new()
                .layer(GrpcTransportLayer)
                .layer(GrpcWebLayer::new())
                .layer(GrpcAccessAuthLayer::new(access_authenticator.clone())),
        )
        .service(grpc_routes);

    let router = Router::new()
        .route("/health", get(health))
        .route(
            "/ready",
            get(move || {
                let readiness = readiness_route.clone();
                async move { readiness.response().await }
            }),
        )
        .merge(outbound_oauth_router)
        .merge(access_authenticator.router())
        .merge(mcp_router);
    let router = if let Some(oauth) = &mcp_oauth {
        router.merge(oauth.router())
    } else {
        router
            .route(
                "/.well-known/oauth-authorization-server/oauth",
                get(not_found),
            )
            .route("/oauth/{*path}", any(not_found))
    };
    #[cfg(feature = "web")]
    let router = {
        let browser_auth = middleware::from_fn_with_state(
            access_authenticator.clone(),
            crate::access_auth::require_browser_session,
        );
        router
            .route(
                "/",
                get(crate::web_assets::serve_root)
                    .layer(browser_auth.clone())
                    .fallback_service(grpc.clone()),
            )
            .route(
                "/{*path}",
                get(crate::web_assets::serve_path)
                    .layer(browser_auth)
                    .fallback_service(grpc.clone()),
            )
    };

    let router = router
        .fallback_service(grpc)
        .layer(TraceLayer::new_for_http().make_span_with(request_span));
    let router = match local_admission {
        Some(local) => router.layer(middleware::from_fn_with_state(
            local.bearer,
            require_local_protocol_bearer,
        )),
        None => router,
    };
    Ok(ServerApplication::new(
        router,
        definition_watcher,
        mcp_oauth,
        routine_engine,
        session_host,
        readiness,
    ))
}

fn request_span<B>(request: &Request<B>) -> tracing::Span {
    let (method, path) = request_trace_fields(request);
    tracing::debug_span!("request", method = %method, path = %path)
}

fn request_trace_fields<B>(request: &Request<B>) -> (&axum::http::Method, &str) {
    (request.method(), request.uri().path())
}

#[tracing::instrument(name = "health_check")]
async fn health() -> Json<HealthResponse> {
    tracing::trace!("health check requested");
    Json(HealthResponse { status: "ok" })
}

async fn not_found() -> axum::response::Response {
    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn service_unavailable() -> axum::response::Response {
    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use axum::{body::Body, response::Response};
    use futures_util::{StreamExt as _, stream};
    use tokio::sync::{Notify, oneshot};

    use super::*;

    #[test]
    fn request_trace_path_excludes_query_parameters() {
        let request = Request::builder()
            .method("GET")
            .uri("/oidc/callback?code=secret-code&state=secret-state")
            .body(())
            .unwrap();

        let (method, path) = request_trace_fields(&request);
        assert_eq!(method, "GET");
        assert_eq!(path, "/oidc/callback");
        drop(request_span(&request));
    }

    #[test]
    fn server_supplies_its_origin_to_agent_refresh_policy() {
        let configured = reqwest::Url::parse("https://oauth.internal.example").unwrap();
        let server = reqwest::Url::parse("https://preview-waltr.example/mcp").unwrap();
        let origins = agent_trusted_origins(vec![configured.clone()], Some(&server));
        assert_eq!(origins, [configured, server.clone()]);

        let same_origin = reqwest::Url::parse("https://preview-waltr.example/oauth").unwrap();
        let origins = agent_trusted_origins(vec![same_origin.clone()], Some(&server));
        assert_eq!(origins, [same_origin]);
    }

    #[tokio::test]
    async fn policy_builder_supplies_the_shared_agent_definition_state_to_sessions() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let expected_snapshot = state.snapshot();
        let config_service =
            ConfigService::from_state(state.clone(), crate::test_support::default_catalog_loader());
        let persistence =
            Persistence::initialize(crate::persistence::DatabaseConfig::for_tests().unwrap())
                .await
                .unwrap();

        let app = build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth_policy(
            config_service,
            persistence,
            crate::test_support::default_catalog_loader(),
            &AccessAuthenticator::disabled(),
            None,
            Vec::new(),
            OutboundOAuthConfig::default(),
            None,
            Some(state),
            None,
            move |routes, config, credentials, callback_state, event_sink| {
                assert!(Arc::ptr_eq(&expected_snapshot, &callback_state.snapshot()));
                session::add_store_service_with_agent_state_and_event_sink(
                    routes,
                    waltr_agent::MemorySessionStore::new(),
                    config,
                    credentials,
                    Vec::new(),
                    callback_state,
                    event_sink,
                )
            },
        )
        .unwrap();

        app.shutdown().await;
    }

    #[test]
    fn build_error_preserves_agent_definition_context_and_source() {
        let load_error = crate::agent_definition::load_default_agent(&[], "missing").unwrap_err();
        let error = BuildAppError::AgentDefinition(load_error);

        assert!(
            error
                .to_string()
                .contains("failed to resolve default agent `missing`")
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn maintenance_readiness_requires_live_running_tasks() {
        let (definition_tx, definition_rx) =
            tokio::sync::watch::channel(DefinitionWatcherHealth::Running);
        assert!(definition_watcher_ready(&definition_rx));

        definition_tx
            .send(DefinitionWatcherHealth::Failed("watcher failed".to_owned()))
            .unwrap();
        assert!(!definition_watcher_ready(&definition_rx));

        let (definition_tx, definition_rx) =
            tokio::sync::watch::channel(DefinitionWatcherHealth::Running);
        drop(definition_tx);
        assert!(!definition_watcher_ready(&definition_rx));

        let (routine_tx, routine_rx) = tokio::sync::watch::channel(EngineHealth::Running);
        assert!(routine_engine_ready(&routine_rx));
        routine_tx
            .send(EngineHealth::Failed("worker exited".to_owned()))
            .unwrap();
        assert!(!routine_engine_ready(&routine_rx));

        let (routine_tx, routine_rx) = tokio::sync::watch::channel(EngineHealth::Running);
        drop(routine_tx);
        assert!(!routine_engine_ready(&routine_rx));
    }

    #[tokio::test]
    async fn server_shutdown_joins_cleanup_and_stops_future_activity() {
        tokio::time::pause();
        let cancellation = CancellationToken::new();
        let activity = Arc::new(AtomicUsize::new(0));
        let cleanup_started = Arc::new(Notify::new());
        let task = {
            let cancellation = cancellation.clone();
            let activity = Arc::clone(&activity);
            let cleanup_started = Arc::clone(&cleanup_started);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return,
                        _ = interval.tick() => {
                            activity.fetch_add(1, Ordering::SeqCst);
                            cleanup_started.notify_one();
                        }
                    }
                }
            })
        };
        let app = ServerApplication::with_background_task_for_test(cancellation, task);
        cleanup_started.notified().await;
        assert_eq!(activity.load(Ordering::SeqCst), 1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(app.serve_with_shutdown(listener, async move {
            drop(shutdown_rx.await);
        }));

        shutdown_tx.send(()).unwrap();
        server.await.unwrap().unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        assert_eq!(activity.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn server_shutdown_stops_waiting_for_open_stream_then_joins_background_task() {
        tokio::time::pause();
        let cancellation = CancellationToken::new();
        let cleaned_up = Arc::new(AtomicBool::new(false));
        let task = {
            let cancellation = cancellation.clone();
            let cleaned_up = Arc::clone(&cleaned_up);
            tokio::spawn(async move {
                cancellation.cancelled().await;
                cleaned_up.store(true, Ordering::SeqCst);
            })
        };
        let router = Router::new().route(
            "/stream",
            get(|| async {
                let body = stream::once(async { Ok::<_, Infallible>("open") })
                    .chain(stream::pending::<Result<&'static str, Infallible>>());
                Response::new(Body::from_stream(body))
            }),
        );
        let app =
            ServerApplication::with_router_and_background_task_for_test(router, cancellation, task);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (shutdown_observed_tx, shutdown_observed_rx) = oneshot::channel();
        let server = tokio::spawn(app.serve_with_shutdown(listener, async move {
            drop(shutdown_rx.await);
            let _ = shutdown_observed_tx.send(());
        }));
        let response = reqwest::get(format!("http://{address}/stream"))
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        shutdown_tx.send(()).unwrap();
        shutdown_observed_rx.await.unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(GRACEFUL_SHUTDOWN_TIMEOUT - Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(!server.is_finished());

        // Tokio's timer wheel may surface an exact deadline on the following millisecond tick.
        tokio::time::advance(Duration::from_millis(2)).await;
        for _ in 0..10 {
            if server.is_finished() {
                break;
            }
            tokio::time::advance(Duration::ZERO).await;
        }
        assert!(
            server.is_finished(),
            "server remained active at drain deadline (cleanup joined: {})",
            cleaned_up.load(Ordering::SeqCst)
        );
        server.await.unwrap().unwrap();
        assert!(cleaned_up.load(Ordering::SeqCst));
        drop(response);
    }
}
