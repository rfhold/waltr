use std::{io, path::Path, time::Duration};

use hyper_util::rt::TokioIo;
use tokio::{io::DuplexStream, sync::mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Endpoint, Server};

use crate::sandbox_gateway::{SandboxGateway, SandboxHandle};

const WORKSPACE: &str = "session-test";

pub(in crate::agent::session) async fn worker_handle(root: impl AsRef<Path>) -> SandboxHandle {
    let gateway = SandboxGateway::new(WORKSPACE.into());
    let (sender, receiver) = mpsc::channel::<Result<DuplexStream, io::Error>>(4);
    let service = gateway.service();
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(ReceiverStream::new(receiver))
            .await
            .expect("sandbox server");
    });
    let channel = Endpoint::from_static("http://sandbox.test")
        .connect_with_connector(tower::service_fn(move |_| {
            let sender = sender.clone();
            async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                sender.send(Ok(server)).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::ConnectionAborted, "sandbox server stopped")
                })?;
                Ok::<_, io::Error>(TokioIo::new(client))
            }
        }))
        .await
        .expect("sandbox channel");
    let path = root.as_ref().to_path_buf();
    tokio::spawn(async move {
        waltr_agent_sandbox::run_worker(channel, path, WORKSPACE)
            .await
            .expect("sandbox worker");
    });
    tokio::time::timeout(Duration::from_secs(10), gateway.wait_ready())
        .await
        .expect("sandbox startup timeout")
        .expect("sandbox startup");
    gateway.handle()
}
