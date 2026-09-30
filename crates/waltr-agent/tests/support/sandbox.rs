use std::{io, path::Path, time::Duration};

use hyper_util::rt::TokioIo;
use waltr_agent::sandbox_gateway::{SandboxGateway, SandboxHandle};
use tokio::{io::DuplexStream, sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Endpoint, Server};

const WORKSPACE: &str = "agent-test";

pub struct TestSandbox {
    gateway: SandboxGateway,
    server: JoinHandle<()>,
    worker: JoinHandle<()>,
}

impl TestSandbox {
    pub async fn new(root: impl AsRef<Path>) -> Self {
        let gateway = SandboxGateway::new(WORKSPACE.into());
        let (sender, receiver) = mpsc::channel::<Result<DuplexStream, io::Error>>(4);
        let service = gateway.service();
        let server = tokio::spawn(async move {
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
        let worker = tokio::spawn(async move {
            waltr_agent_sandbox::run_worker(channel, path, WORKSPACE)
                .await
                .expect("sandbox worker");
        });
        tokio::time::timeout(Duration::from_secs(10), gateway.wait_ready())
            .await
            .expect("sandbox startup timeout")
            .expect("sandbox startup");
        Self {
            gateway,
            server,
            worker,
        }
    }

    pub fn handle(&self) -> SandboxHandle {
        self.gateway.handle()
    }

    pub fn disconnect(&self) {
        self.gateway.shutdown();
    }
}

impl Drop for TestSandbox {
    fn drop(&mut self) {
        self.gateway.shutdown();
        self.worker.abort();
        self.server.abort();
    }
}
