//! Waltr server entry point.

mod build_info;

use waltr_server::{DEFAULT_LISTEN_ADDR, build_app, observability, profiling};
use std::{env, net::SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _observability = observability::init()?;
    let _profiling = profiling::init()?;
    tracing::info!(
        version = build_info::VERSION,
        revision = build_info::revision(),
        "starting server"
    );

    let listen_addr =
        env::var("WALTR_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_owned());
    let listen_addr: SocketAddr = listen_addr.parse()?;

    let app = build_app().await?;
    let (app, listener, listen_addr) = app.bind(listen_addr).await?;
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

    use waltr_server::test_support::blocked_cleanup_application;

    #[tokio::test]
    async fn occupied_port_cancels_and_joins_cleanup_before_startup_error() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap();
        let (app, probe) = blocked_cleanup_application();
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
