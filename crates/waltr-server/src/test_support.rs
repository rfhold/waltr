//! Shared integration test server setup.

use crate::{build_app_with_services, config_service::ConfigService, session};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    thread,
};

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

fn setup_server_with_listener(
    config_service: std::sync::Arc<ConfigService>,
    listener: TcpListener,
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
                build_app_with_services(config_service, |routes, config| {
                    session::add_memory_service(routes, config);
                }),
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
    let root = std::env::temp_dir().join(format!(
        "waltr-config-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temp root should be created");
    root
}
