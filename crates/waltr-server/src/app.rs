//! HTTP and gRPC router construction.

use crate::{
    auth::{AuthService, FileCredentialStore},
    catalog::{CatalogLoader, CatalogService},
    config::{ConfigError, ConfigHandle},
    config_service::ConfigService,
    mcp_service::WaltrMcpServer,
    persistence::{Persistence, PersistenceError},
    session,
    status_service::StatusService,
};
use axum::{Json, Router, response::IntoResponse as _, routing::get};
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
struct Readiness(Persistence);

impl Readiness {
    async fn response(self) -> axum::response::Response {
        if self.0.ready().await.is_ok() {
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
}

impl fmt::Debug for ServerApplication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerApplication")
            .finish_non_exhaustive()
    }
}

impl ServerApplication {
    fn new(router: Router, store: Arc<dyn mcp::server::StreamableHttpSessionStore>) -> Self {
        Self {
            router,
            mcp_cleanup: McpSessionCleanupTask::spawn(store, MCP_CLEANUP_INTERVAL),
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
    }

    /// Bind the listener, shutting down background work if startup fails.
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
        }
    }

    /// Serve until the listener fails or shutdown resolves, then join background work.
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
        } = self;
        let cancellation = mcp_cleanup.cancellation.clone();
        let result = axum::serve(listener, router.into_make_service())
            .with_graceful_shutdown(async move {
                shutdown.await;
                cancellation.cancel();
            })
            .await;
        mcp_cleanup.shutdown().await;
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

/// Failure to construct the environment-owned application.
pub enum BuildAppError {
    /// Filesystem-backed configuration could not start.
    Config(ConfigError),
    /// Server persistence could not initialize.
    Persistence(PersistenceError),
    /// Agent session storage configuration is invalid.
    SessionStorage(session::SessionStorageConfigError),
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
        }
    }
}

impl Error for BuildAppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Persistence(error) => Some(error),
            Self::SessionStorage(error) => Some(error),
        }
    }
}

/// Build the Axum application from environment-owned dependencies.
///
/// # Errors
///
/// Returns an error when persistence, config, or agent session storage cannot initialize.
pub async fn build_app() -> Result<ServerApplication, BuildAppError> {
    tracing::debug!("building app with config service from current directory");
    let persistence = Persistence::from_env()
        .await
        .map_err(BuildAppError::Persistence)?;
    let config_service = ConfigService::from_current_dir().map_err(BuildAppError::Config)?;
    let mut session_storage = Ok(());
    let app = build_app_with_services(
        config_service,
        persistence,
        |grpc_routes, config, credential_store| {
            session_storage = session::add_service_from_env(grpc_routes, config, credential_store);
        },
    );
    session_storage.map_err(BuildAppError::SessionStorage)?;
    Ok(app)
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
    build_app_with_catalog_loader(
        config_service,
        persistence,
        CatalogLoader::from_env(),
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
    tracing::debug!("building app router");
    let config = ConfigHandle::watching(
        config_service.store().clone(),
        config_service.pubsub().clone(),
    );
    let credential_store = FileCredentialStore::from_env();
    let auth_service = AuthService::new(config_service.store().clone(), credential_store.clone());
    let catalog_service = CatalogService::new(config_service.store().clone(), catalog_loader);
    let mcp_session_store = persistence.mcp_session_store();
    let readiness = Readiness(persistence);
    let mcp_router = mcp::server::streamable_http_router_with_options(
        Arc::new(WaltrMcpServer::new(Arc::clone(&config_service))),
        mcp::server::StreamableHttpOptions::default()
            .with_session_store(Arc::clone(&mcp_session_store)),
    );
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
        .layer(GrpcWebLayer::new())
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
        .merge(mcp_router);
    #[cfg(feature = "web")]
    let router = router.route("/", get(crate::web_assets::serve_root)).route(
        "/{*path}",
        get(crate::web_assets::serve_path).fallback_service(grpc.clone()),
    );

    let router = router
        .fallback_service(grpc)
        .layer(TraceLayer::new_for_http());
    ServerApplication::new(router, mcp_session_store)
}

#[tracing::instrument(name = "health_check")]
async fn health() -> Json<HealthResponse> {
    tracing::trace!("health check requested");
    Json(HealthResponse { status: "ok" })
}
