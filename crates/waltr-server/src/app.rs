//! HTTP and gRPC router construction.

use crate::{
    config::ConfigHandle, config_service::ConfigService, session, status_service::StatusService,
};
use axum::{Json, Router, routing::get};
use waltr::grpc::waltr::config::v1::config_service_server::ConfigServiceServer;
use waltr::grpc::waltr::status::v1::status_service_server::StatusServiceServer;
use serde::Serialize;
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

/// Build the Axum application with health and gRPC routes.
///
/// # Panics
///
/// Panics when the current directory cannot be resolved or the config watcher cannot start.
pub fn build_app() -> Router {
    tracing::debug!("building app with config service from current directory");
    let config_service = ConfigService::from_current_dir().expect("config service should start");
    build_app_with_config_service(config_service)
}

/// Build the Axum application using an explicit config service.
///
/// # Panics
///
/// Panics when session storage environment configuration is invalid.
pub fn build_app_with_config_service(config_service: std::sync::Arc<ConfigService>) -> Router {
    build_app_with_services(config_service, |grpc_routes, config| {
        session::add_service_from_env(grpc_routes, config).expect("session service should start");
    })
}

/// Build the Axum application using explicit services.
pub fn build_app_with_services<F>(
    config_service: std::sync::Arc<ConfigService>,
    add_session_service: F,
) -> Router
where
    F: FnOnce(&mut RoutesBuilder, ConfigHandle),
{
    tracing::debug!("building app router");
    let config = ConfigHandle::watching(
        config_service.store().clone(),
        config_service.pubsub().clone(),
    );
    let mut grpc_routes = Routes::builder();
    grpc_routes.add_service(ConfigServiceServer::from_arc(config_service));
    add_session_service(&mut grpc_routes, config);
    grpc_routes.add_service(StatusServiceServer::new(StatusService));
    let grpc_routes = grpc_routes.routes();
    let grpc = ServiceBuilder::new()
        .layer(OtelGrpcLayer::default())
        .layer(MetricsLayerBuilder::new().build())
        .layer(GrpcWebLayer::new())
        .service(grpc_routes);

    Router::new()
        .route("/health", get(health))
        .fallback_service(grpc)
        .layer(TraceLayer::new_for_http())
}

#[tracing::instrument(name = "health_check")]
async fn health() -> Json<HealthResponse> {
    tracing::trace!("health check requested");
    Json(HealthResponse { status: "ok" })
}
