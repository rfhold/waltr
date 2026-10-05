//! Waltr server entry point.

mod build_info;
mod standalone_apps;

use waltr_server::{DEFAULT_LISTEN_ADDR, build_app, observability};
use std::{env, net::SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if env::args().nth(1).as_deref() == Some("app-container") {
        let bind = env::var("WALTR_APP_CONTAINER_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:14334".into())
            .parse()?;
        let container = waltr_server::mcp_apps_host::spawn_app_container(
            waltr_server::mcp_apps_host::AppsContainerConfig {
                bind,
                public_origin: env::var("WALTR_APP_CONTAINER_ORIGIN").ok(),
                allowed_host_origins: env::var("WALTR_APP_CONTAINER_HOST_ORIGINS")?
                    .split(',')
                    .map(str::to_owned)
                    .collect(),
            },
        )
        .await?;
        shutdown_signal().await;
        container.shutdown().await?;
        return Ok(());
    }
    let _observability = observability::init()?;
    #[cfg(feature = "profiling")]
    let _profiling = waltr_server::profiling::init()?;
    tracing::info!(
        version = build_info::VERSION,
        revision = build_info::revision(),
        "starting server"
    );

    let listen_addr =
        env::var("WALTR_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_owned());
    let listen_addr: SocketAddr = listen_addr.parse()?;

    let apps = standalone_apps::from_env()?;
    let app = build_app().await?;
    #[cfg(feature = "web")]
    let (app, listener, listen_addr) = match apps {
        Some(config) => app.bind_with_apps_config(listen_addr, config).await?,
        None => app.bind(listen_addr).await?,
    };
    #[cfg(not(feature = "web"))]
    let (app, listener, listen_addr) = {
        debug_assert!(apps.is_none());
        app.bind(listen_addr).await?
    };
    tracing::info!(%listen_addr, "server listening");
    app.serve_with_shutdown(listener, shutdown_signal()).await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => {
                    tracing::error!(%error, "failed to listen for SIGTERM");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            () = ctrl_c => {}
            () = terminate => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;

    tracing::info!("server shutdown requested");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use waltr_server::test_support::blocked_background_application;

    #[tokio::test]
    async fn occupied_port_cancels_and_joins_cleanup_before_startup_error() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap();
        let (app, probe) = blocked_background_application();
        probe.cleanup_started.notified().await;

        let startup = tokio::spawn(app.bind(addr));
        probe.shutdown_observed.notified().await;
        assert!(probe.cleanup_dropped.load(Ordering::SeqCst));
        assert!(!startup.is_finished());
        probe.release_cleanup.notify_one();

        let error = startup
            .await
            .expect("startup should join")
            .expect_err("occupied port should fail startup");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    }
}
