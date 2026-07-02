//! Shared integration test server setup.

use crate::{
    build_app_with_catalog_loader, catalog::CatalogLoader, config_service::ConfigService, session,
};
use waltr_agent::{MemorySessionStore, SessionCoordinator};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

static TEMP_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Shared test server for integration tests in one test binary.
#[derive(Debug)]
pub struct TestServer {
    addr: SocketAddr,
    _thread: thread::JoinHandle<()>,
}

impl TestServer {
    /// Build an absolute URL for a path on the test server.
    #[must_use]
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    /// Return the base URL for tonic clients.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Start an isolated integration test server.
#[must_use]
pub fn setup_server() -> &'static TestServer {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener(config_service, listener)
}

/// Start an isolated integration test server at a fixed address.
#[must_use]
pub fn setup_server_at(addr: SocketAddr) -> &'static TestServer {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    let listener = TcpListener::bind(addr).expect("test server should bind fixed address");
    setup_server_with_listener(config_service, listener)
}

/// Start an isolated integration test server with an explicit config service.
#[must_use]
pub fn setup_server_with_services(
    config_service: std::sync::Arc<ConfigService>,
) -> &'static TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener(config_service, listener)
}

/// Start an isolated integration test server with an explicit catalog loader.
#[must_use]
pub fn setup_server_with_catalog_loader(catalog_loader: CatalogLoader) -> &'static TestServer {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener_and_catalog_loader(config_service, listener, catalog_loader)
}

/// Start an isolated integration test server with an explicit session coordinator.
#[must_use]
pub fn setup_server_with_sessions() -> (&'static TestServer, SessionCoordinator<MemorySessionStore>)
{
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    let sessions = SessionCoordinator::new(MemorySessionStore::new());
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    let server =
        setup_server_with_listener_and_sessions(config_service, listener, sessions.clone());
    (server, sessions)
}

/// Start an isolated integration test server with a caller-seeded session coordinator.
#[must_use]
pub fn setup_server_with_seeded_sessions(
    sessions: SessionCoordinator<MemorySessionStore>,
) -> &'static TestServer {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
    setup_server_with_listener_and_sessions(config_service, listener, sessions)
}

fn setup_server_with_listener(
    config_service: std::sync::Arc<ConfigService>,
    listener: TcpListener,
) -> &'static TestServer {
    setup_server_with_listener_and_sessions(
        config_service,
        listener,
        SessionCoordinator::new(MemorySessionStore::new()),
    )
}

fn setup_server_with_listener_and_catalog_loader(
    config_service: std::sync::Arc<ConfigService>,
    listener: TcpListener,
    catalog_loader: CatalogLoader,
) -> &'static TestServer {
    setup_server_with_listener_sessions_and_catalog_loader(
        config_service,
        listener,
        SessionCoordinator::new(MemorySessionStore::new()),
        catalog_loader,
    )
}

fn setup_server_with_listener_and_sessions(
    config_service: std::sync::Arc<ConfigService>,
    listener: TcpListener,
    sessions: SessionCoordinator<MemorySessionStore>,
) -> &'static TestServer {
    setup_server_with_listener_sessions_and_catalog_loader(
        config_service,
        listener,
        sessions,
        CatalogLoader::from_env(),
    )
}

fn setup_server_with_listener_sessions_and_catalog_loader(
    config_service: std::sync::Arc<ConfigService>,
    listener: TcpListener,
    sessions: SessionCoordinator<MemorySessionStore>,
    catalog_loader: CatalogLoader,
) -> &'static TestServer {
    listener
        .set_nonblocking(true)
        .expect("test server listener should be nonblocking");
    let addr = listener
        .local_addr()
        .expect("test server local address should be available");

    let server_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test server runtime should start");

        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("test server listener should become async");
            axum::serve(
                listener,
                build_app_with_catalog_loader(
                    config_service,
                    catalog_loader,
                    |routes, config, credential_store| {
                        session::add_sessions_service(routes, sessions, config, credential_store);
                    },
                ),
            )
            .await
            .expect("test server should run");
        });
    });

    Box::leak(Box::new(TestServer {
        addr,
        _thread: server_thread,
    }))
}

#[must_use]
pub fn temp_root() -> PathBuf {
    let counter = TEMP_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "waltr-config-test-{}-{counter}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temp root should be created");
    root
}
