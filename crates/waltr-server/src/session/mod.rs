//! Session service coordination.

use crate::config::ConfigHandle;
use waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer;
use waltr_agent::{FilesystemSessionStore, MemorySessionStore, SessionStore};
use tonic::service::RoutesBuilder;

pub mod service;
pub mod storage;

mod convert;
mod error;
mod runtime_config;

pub use service::SessionService;
pub use storage::SessionStorageConfigError;

use storage::SessionStorageConfig;

/// Add the configured session service to the gRPC routes builder.
///
/// # Errors
///
/// Returns an error when session storage environment configuration is invalid.
pub fn add_service_from_env(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
) -> Result<(), SessionStorageConfigError> {
    match SessionStorageConfig::from_env()? {
        SessionStorageConfig::Memory => {
            add_store_service(routes, MemorySessionStore::new(), config);
        }
        SessionStorageConfig::Filesystem { root } => {
            add_store_service(routes, FilesystemSessionStore::new(root), config);
        }
    }

    Ok(())
}

/// Add the default session service to the gRPC routes builder.
pub fn add_default_service(routes: &mut RoutesBuilder) {
    routes.add_service(SessionServiceServer::from_arc(SessionService::new(
        MemorySessionStore::new(),
    )));
}

/// Add an in-memory session service using an explicit config store.
pub fn add_memory_service(routes: &mut RoutesBuilder, config: ConfigHandle) {
    add_store_service(routes, MemorySessionStore::new(), config);
}

fn add_store_service<S>(routes: &mut RoutesBuilder, store: S, config: ConfigHandle)
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    routes.add_service(SessionServiceServer::from_arc(
        SessionService::new_with_config(store, config),
    ));
}
