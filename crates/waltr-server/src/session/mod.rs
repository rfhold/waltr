//! Session service coordination.

use crate::config::ConfigHandle;
use waltr::grpc::waltr::session::v1::session_service_server::SessionServiceServer;
use waltr_agent::{
    CredentialStore, FilesystemSessionStore, MemorySessionStore, SessionCoordinator, SessionStore,
};
use tonic::service::RoutesBuilder;

pub mod service;
pub mod storage;

mod convert;
mod error;
mod runtime_config;
mod timeline_convert;

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
    credential_store: CredentialStore,
) -> Result<(), SessionStorageConfigError> {
    match SessionStorageConfig::from_env()? {
        SessionStorageConfig::Memory => {
            add_store_service(routes, MemorySessionStore::new(), config, credential_store);
        }
        SessionStorageConfig::Filesystem { root } => {
            add_store_service(
                routes,
                FilesystemSessionStore::new(root),
                config,
                credential_store,
            );
        }
        SessionStorageConfig::S3(s3_config) => {
            add_store_service(routes, s3_config.into_store(), config, credential_store);
        }
    }

    Ok(())
}

/// Add the default session service to the gRPC routes builder.
pub fn add_default_service(routes: &mut RoutesBuilder, credential_store: CredentialStore) {
    routes.add_service(SessionServiceServer::from_arc(
        SessionService::new_with_config(
            MemorySessionStore::new(),
            ConfigHandle::default(),
            credential_store,
        ),
    ));
}

/// Add an in-memory session service using an explicit config store.
pub fn add_memory_service(
    routes: &mut RoutesBuilder,
    config: ConfigHandle,
    credential_store: CredentialStore,
) {
    add_store_service(routes, MemorySessionStore::new(), config, credential_store);
}

/// Add an in-memory session service using an existing session coordinator.
pub fn add_sessions_service<S>(
    routes: &mut RoutesBuilder,
    sessions: SessionCoordinator<S>,
    config: ConfigHandle,
    credential_store: CredentialStore,
) where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    routes.add_service(SessionServiceServer::from_arc(
        SessionService::from_sessions_with_config(sessions, config, credential_store),
    ));
}

fn add_store_service<S>(
    routes: &mut RoutesBuilder,
    store: S,
    config: ConfigHandle,
    credential_store: CredentialStore,
) where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    routes.add_service(SessionServiceServer::from_arc(
        SessionService::new_with_config(store, config, credential_store),
    ));
}
