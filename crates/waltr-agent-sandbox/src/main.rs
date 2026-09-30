//! Standalone private workspace worker.

use std::{
    env, io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use hyper_util::rt::TokioIo;
use waltr_agent_sandbox::{Sandbox, run_worker_with_shutdown};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Endpoint};

const WORKSPACE_ID: &str = "local";

fn path_from_env(name: &str) -> io::Result<PathBuf> {
    let value = env::var(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be set to an absolute path"),
        )
    })?;
    let path = PathBuf::from(value);
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an absolute path to a file or directory"),
        ));
    }
    Ok(path)
}

fn validate_socket(path: &Path) -> io::Result<()> {
    if !path.parent().is_some_and(Path::is_dir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox socket parent directory does not exist",
        ));
    }
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox socket path is not a Unix socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn private_channel(socket: PathBuf) -> Channel {
    // Dial lazily so the worker can start before the host creates its listener.
    Endpoint::from_static("http://sandbox.local").connect_with_connector_lazy(tower::service_fn(
        move |_| {
            let socket = socket.clone();
            async move { UnixStream::connect(socket).await.map(TokioIo::new) }
        },
    ))
}

async fn shutdown_signal() -> io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let root = path_from_env("WALTR_SANDBOX_WORKSPACE_ROOT")?;
    let socket = path_from_env("WALTR_SANDBOX_SOCKET")?;
    Sandbox::new(&root)?;
    validate_socket(&socket)?;

    let channel = private_channel(socket);
    let stop = CancellationToken::new();
    let worker = run_worker_with_shutdown(
        channel,
        root,
        WORKSPACE_ID,
        stop.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    tokio::pin!(worker);
    tokio::select! {
        result = &mut worker => result?,
        result = shutdown_signal() => {
            stop.cancel();
            worker.await?;
            result?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn socket_may_be_missing_but_not_a_regular_file_or_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gateway.sock");
        validate_socket(&socket).unwrap();
        std::fs::write(&socket, "not a socket").unwrap();
        assert!(validate_socket(&socket).is_err());
        std::fs::remove_file(&socket).unwrap();
        std::os::unix::fs::symlink(dir.path(), &socket).unwrap();
        assert!(validate_socket(&socket).is_err());
        assert!(validate_socket(&dir.path().join("missing/gateway.sock")).is_err());
    }

    #[tokio::test]
    async fn worker_retries_when_host_has_not_bound_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let worker = tokio::spawn(run_worker_with_shutdown(
            private_channel(dir.path().join("gateway.sock")),
            dir.path().to_path_buf(),
            WORKSPACE_ID,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
        ));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!worker.is_finished());
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
