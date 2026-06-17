//! Session service coordination.

use waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer;
use waltr_agent::{FilesystemSessionStore, MemorySessionStore, SessionStore};
use tonic::service::RoutesBuilder;

pub mod service;
pub mod storage;

mod convert;
mod error;

pub use service::SessionService;
pub use storage::SessionStorageConfigError;

use storage::SessionStorageConfig;

/// Add the configured session service to the gRPC routes builder.
///
/// # Errors
///
/// Returns an error when session storage environment configuration is invalid.
pub fn add_service_from_env(routes: &mut RoutesBuilder) -> Result<(), SessionStorageConfigError> {
    match SessionStorageConfig::from_env()? {
        SessionStorageConfig::Memory => add_store_service(routes, MemorySessionStore::new()),
        SessionStorageConfig::Filesystem { root } => {
            add_store_service(routes, FilesystemSessionStore::new(root));
        }
    }

    Ok(())
}

/// Add the default session service to the gRPC routes builder.
pub fn add_default_service(routes: &mut RoutesBuilder) {
    add_store_service(routes, MemorySessionStore::new())
}

fn add_store_service<S>(routes: &mut RoutesBuilder, store: S)
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    routes.add_service(SessionServiceServer::from_arc(SessionService::new(store)));
}
