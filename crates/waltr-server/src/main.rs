//! Waltr server entry point.

use waltr_server::{DEFAULT_LISTEN_ADDR, build_app, observability, profiling};
use std::{env, net::SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _observability = observability::init()?;
    let _profiling = profiling::init()?;

    let listen_addr =
        env::var("WALTR_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.to_owned());
    let listen_addr: SocketAddr = listen_addr.parse()?;

    tracing::info!(%listen_addr, "starting server");
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!(%listen_addr, "server listening");
    axum::serve(listener, build_app()).await?;

    Ok(())
}
