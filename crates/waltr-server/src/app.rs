//! HTTP and gRPC router construction.

use crate::{
    access_auth::{
        AccessAuthConfig, AccessAuthConfigError, AccessAuthError, AccessAuthHttpClient,
        AccessAuthenticator, GrpcAccessAuthLayer, GrpcTransportLayer,
    },
    auth::{AuthService, FileCredentialStore},
    catalog::{CatalogLoader, CatalogService},
    config::{ConfigError, ConfigHandle},
    config_service::ConfigService,
    mcp_oauth::{McpOAuthConfig, McpOAuthConfigError, McpOAuthError, McpOAuthServer},
    mcp_service::WaltrMcpServer,
    persistence::{Persistence, PersistenceError},
    session,
    status_service::StatusService,
};
#[cfg(feature = "web")]
use axum::middleware;
use axum::{
    Json, Router,
    http::Request,
    response::IntoResponse as _,
    routing::{any, get},
};
use waltr::grpc::waltr::auth::v1::auth_service_server::AuthServiceServer;
use waltr::grpc::waltr::catalog::v1::catalog_service_server::CatalogServiceServer;
use waltr::grpc::waltr::config::v1::config_service_server::ConfigServiceServer;
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

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Clone, Debug)]
struct Readiness {
    persistence: Persistence,
    oauth: Option<McpOAuthServer>,
}

impl Readiness {
    async fn response(self) -> axum::response::Response {
        let ready = self.persistence.ready().await.is_ok()
            && match self.oauth {
                Some(oauth) => oauth.ready().await.is_ok(),
                None => true,
            };
        if ready {
            Json(HealthResponse { status: "ready" }).into_response()
        } else {
            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// Waltr router and server-owned background work.
pub struct ServerApplication {
    router: Router,
    mcp_cleanup: McpSessionCleanupTask,
    oauth_maintenance: Option<McpOAuthMaintenanceTask>,
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
        store: Arc<dyn mcp::server::StreamableHttpSessionStore>,
        oauth: Option<McpOAuthServer>,
    ) -> Self {
        Self {
            router,
            mcp_cleanup: McpSessionCleanupTask::spawn(store, MCP_CLEANUP_INTERVAL),
            oauth_maintenance: oauth.map(McpOAuthMaintenanceTask::spawn),
        }
    }

    /// Clone the router for in-process request testing.
    #[must_use]
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    /// Cancel and join all server-owned background work.
    pub async fn shutdown(self) {
        self.mcp_cleanup.shutdown().await;
        if let Some(task) = self.oauth_maintenance {
            task.shutdown().await;
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

    #[cfg(any(debug_assertions, feature = "test"))]
    pub(crate) fn with_cleanup_task_for_test(
        cancellation: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            router: Router::new(),
            mcp_cleanup: McpSessionCleanupTask {
                cancellation,
                task: Some(task),
            },
            oauth_maintenance: None,
        }
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
            mcp_cleanup,
            oauth_maintenance,
        } = self;
        let cancellation = mcp_cleanup.cancellation.clone();
        let oauth_cancellation = oauth_maintenance
            .as_ref()
            .map(|task| task.cancellation.clone());
        let result = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            shutdown.await;
            cancellation.cancel();
            if let Some(cancellation) = oauth_cancellation {
                cancellation.cancel();
            }
        })
        .await;
        mcp_cleanup.shutdown().await;
        if let Some(task) = oauth_maintenance {
            task.shutdown().await;
        }
        result
    }
}

const MCP_CLEANUP_INTERVAL: Duration = Duration::from_mins(5);

struct McpSessionCleanupTask {
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl McpSessionCleanupTask {
    fn spawn(
        store: Arc<dyn mcp::server::StreamableHttpSessionStore>,
        cleanup_interval: Duration,
    ) -> Self {
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = task_cancellation.cancelled() => return,
                    _ = interval.tick() => {}
                }
                let limit = store.config().cleanup_batch_size;
                tokio::select! {
                    biased;
                    () = task_cancellation.cancelled() => return,
                    result = store.cleanup_expired(std::time::SystemTime::now(), limit) => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "failed to clean up expired MCP sessions");
                        }
                    }
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
            tracing::warn!(%error, "MCP session cleanup task failed to join");
        }
    }
}

impl Drop for McpSessionCleanupTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
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
    /// Filesystem-backed configuration could not start.
    Config(ConfigError),
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
            Self::McpOAuth(_) => formatter.write_str("failed to initialize MCP OAuth server"),
        }
    }
}

impl Error for BuildAppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Persistence(error) => Some(error),
            Self::SessionStorage(error) => Some(error),
            Self::AccessAuthConfig(error) => Some(error),
            Self::AccessAuthClient(error) => Some(error),
            Self::AccessAuthDiscovery(error) | Self::AccessAuthStore(error) => Some(error),
            Self::McpOAuthConfig(error) => Some(error),
            Self::McpOAuth(error) => Some(error),
        }
    }
}

/// Build the Axum application from environment-owned dependencies.
///
/// # Errors
///
/// Returns an error when persistence, config, authentication, or agent session storage cannot initialize.
pub async fn build_app() -> Result<ServerApplication, BuildAppError> {
    tracing::debug!("building app with config service from current directory");
    let persistence = Persistence::from_env()
        .await
        .map_err(BuildAppError::Persistence)?;
    let config_service = ConfigService::from_current_dir().map_err(BuildAppError::Config)?;
    let access_auth_config =
        AccessAuthConfig::from_env().map_err(BuildAppError::AccessAuthConfig)?;
    let mcp_oauth_config =
        McpOAuthConfig::from_env(&access_auth_config).map_err(BuildAppError::McpOAuthConfig)?;
    let access_auth_client = AccessAuthHttpClient::build(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10)),
    )
    .map_err(BuildAppError::AccessAuthClient)?;
    let access_authenticator =
        AccessAuthenticator::initialize(access_auth_config, &persistence, access_auth_client)
            .await
            .map_err(|error| match error {
                AccessAuthError::Persistence(_) => BuildAppError::AccessAuthStore(error),
                _ => BuildAppError::AccessAuthDiscovery(error),
            })?;
    let mcp_oauth = match mcp_oauth_config {
        McpOAuthConfig::Disabled => None,
        McpOAuthConfig::Enabled(config) => Some(
            McpOAuthServer::initialize(config, &persistence, access_authenticator.clone())
                .await
                .map_err(BuildAppError::McpOAuth)?,
        ),
    };
    let mut session_storage = Ok(());
    let router = build_app_with_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        access_authenticator,
        mcp_oauth,
        |grpc_routes, config, credential_store| {
            session_storage = session::add_service_from_env(grpc_routes, config, credential_store);
        },
    );
    session_storage.map_err(BuildAppError::SessionStorage)?;
    Ok(router)
}

/// Build the Axum application using an explicit config service.
///
/// # Panics
///
/// Panics when session storage environment configuration is invalid.
#[must_use]
pub fn build_app_with_config_service(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
) -> ServerApplication {
    build_app_with_services(
        config_service,
        persistence,
        |grpc_routes, config, credential_store| {
            session::add_service_from_env(grpc_routes, config, credential_store)
                .expect("session service should start");
        },
    )
}

/// Build the Axum application using explicit services.
pub fn build_app_with_services<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    add_session_service: F,
) -> ServerApplication
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle, FileCredentialStore),
{
    build_app_with_catalog_loader_and_access_authenticator(
        config_service,
        persistence,
        CatalogLoader::from_env(),
        AccessAuthenticator::disabled(),
        add_session_service,
    )
}

/// Build the Axum application with an initialized access authenticator.
pub fn build_app_with_access_authenticator<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    access_authenticator: AccessAuthenticator,
    add_session_service: F,
) -> ServerApplication
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle, FileCredentialStore),
{
    build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        CatalogLoader::from_env(),
        access_authenticator,
        None,
        add_session_service,
    )
}

/// Build the application with initialized access authentication and hosted MCP OAuth.
pub fn build_app_with_access_authenticator_and_mcp_oauth<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    access_authenticator: AccessAuthenticator,
    mcp_oauth: Option<McpOAuthServer>,
    add_session_service: F,
) -> ServerApplication
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle, FileCredentialStore),
{
    build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        CatalogLoader::from_env(),
        access_authenticator,
        mcp_oauth,
        add_session_service,
    )
}

/// Build the Axum application using explicit services and catalog loader.
pub fn build_app_with_catalog_loader<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    add_session_service: F,
) -> ServerApplication
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle, FileCredentialStore),
{
    build_app_with_catalog_loader_and_access_authenticator(
        config_service,
        persistence,
        catalog_loader,
        AccessAuthenticator::disabled(),
        add_session_service,
    )
}

/// Build the Axum application using explicit services, catalog, and access authentication.
///
/// # Panics
///
/// Panics if enabled access authentication produces invalid MCP authorization metadata.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "router assembly consumes initialized services in dependency order"
)]
pub fn build_app_with_catalog_loader_and_access_authenticator<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    access_authenticator: AccessAuthenticator,
    add_session_service: F,
) -> ServerApplication
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle, FileCredentialStore),
{
    build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth(
        config_service,
        persistence,
        catalog_loader,
        access_authenticator,
        None,
        add_session_service,
    )
}

/// Build the application with explicit catalog, access auth, and hosted MCP OAuth.
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
pub fn build_app_with_catalog_loader_and_access_authenticator_and_mcp_oauth<F>(
    config_service: Arc<ConfigService>,
    persistence: Persistence,
    catalog_loader: CatalogLoader,
    access_authenticator: AccessAuthenticator,
    mcp_oauth: Option<McpOAuthServer>,
    add_session_service: F,
) -> ServerApplication
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle, FileCredentialStore),
{
    tracing::debug!("building app router");
    let config = ConfigHandle::watching(
        config_service.store().clone(),
        config_service.pubsub().clone(),
    );
    let outbound_oauth_store = persistence.outbound_oauth_store();
    let credential_store =
        FileCredentialStore::from_env().with_mcp_store(outbound_oauth_store.clone());
    let auth_service = AuthService::new(
        config_service.store().clone(),
        credential_store.clone(),
        outbound_oauth_store,
    );
    let outbound_oauth_router = auth_service.router();
    let catalog_service = CatalogService::new(config_service.store().clone(), catalog_loader);
    let mcp_session_store = persistence.mcp_session_store();
    let readiness = Readiness {
        persistence: persistence.clone(),
        oauth: mcp_oauth.clone(),
    };
    // Waltr has no browser MCP client, so the generic empty Origin allowlist stays in force.
    let mut mcp_options = mcp::server::StreamableHttpOptions::default()
        .with_session_store(Arc::clone(&mcp_session_store))
        .without_root_protected_resource_metadata();
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
    let mcp_router = if access_authenticator.enabled() && mcp_oauth.is_none() {
        Router::new()
            .route("/mcp", any(service_unavailable))
            .route("/.well-known/oauth-protected-resource/mcp", get(not_found))
    } else {
        mcp::server::streamable_http_router_with_options(
            Arc::new(WaltrMcpServer::new(Arc::clone(&config_service))),
            mcp_options,
        )
    };
    let mut grpc_routes = Routes::builder();
    grpc_routes.add_service(ConfigServiceServer::from_arc(config_service));
    grpc_routes.add_service(AuthServiceServer::from_arc(auth_service));
    grpc_routes.add_service(CatalogServiceServer::from_arc(catalog_service));
    add_session_service(&mut grpc_routes, config, credential_store);
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
                let readiness = readiness.clone();
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
    ServerApplication::new(router, mcp_session_store, mcp_oauth)
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
}
