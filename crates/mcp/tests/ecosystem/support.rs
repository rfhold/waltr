use std::{future::Future, pin::Pin};

use mcp::client::McpTransportConfig;

use super::{mcp_client, mcp_server, rmcp_client, rmcp_server};

pub type BoxTestFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type StartServer = fn() -> BoxTestFuture<mcp::Result<Box<dyn RunningServer>>>;

pub struct EcosystemServerCase {
    pub name: &'static str,
    start: StartServer,
}

impl EcosystemServerCase {
    pub fn new(name: &'static str, start: StartServer) -> Self {
        Self { name, start }
    }

    pub async fn start(&self) -> mcp::Result<Box<dyn RunningServer>> {
        (self.start)().await.map_err(|error| {
            mcp::Error::protocol(format!("{} failed to start: {error}", self.name))
        })
    }
}

pub struct EcosystemClientCase {
    name: &'static str,
    run: fn(McpTransportConfig) -> BoxTestFuture<mcp::Result<()>>,
}

impl EcosystemClientCase {
    pub fn new(
        name: &'static str,
        run: fn(McpTransportConfig) -> BoxTestFuture<mcp::Result<()>>,
    ) -> Self {
        Self { name, run }
    }

    pub async fn run(&self, transport: McpTransportConfig) -> mcp::Result<()> {
        (self.run)(transport)
            .await
            .map_err(|error| mcp::Error::protocol(format!("{} failed: {error}", self.name)))
    }
}

pub trait RunningServer: Send {
    fn transport(&self) -> McpTransportConfig;
}

pub fn start_mcp_server() -> BoxTestFuture<mcp::Result<Box<dyn RunningServer>>> {
    Box::pin(async {
        Ok(Box::new(mcp_server::McpStreamableHttpServer::start().await?) as Box<dyn RunningServer>)
    })
}

pub fn start_rmcp_server() -> BoxTestFuture<mcp::Result<Box<dyn RunningServer>>> {
    Box::pin(async {
        Ok(
            Box::new(rmcp_server::RmcpStreamableHttpServer::start().await?)
                as Box<dyn RunningServer>,
        )
    })
}

pub async fn assert_standard_surface_with_mcp_client(
    case_name: &str,
    transport: McpTransportConfig,
) -> mcp::Result<()> {
    mcp_client::assert_standard_surface(case_name, transport).await
}

pub async fn assert_standard_surface_with_rmcp_client(
    transport: McpTransportConfig,
) -> mcp::Result<()> {
    rmcp_client::assert_standard_surface(transport).await
}
