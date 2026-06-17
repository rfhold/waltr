//! Stdio MCP test server used by integration tests.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoMarkerRequest {
    value: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DelayedWriteRequest {
    path: String,
    value: String,
    delay_ms: u64,
}

#[derive(Debug, Clone)]
struct EchoMarkerServer {
    tool_router: ToolRouter<Self>,
}

impl EchoMarkerServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl EchoMarkerServer {
    #[tool(name = "echo_marker", description = "Echo a deterministic marker.")]
    fn echo_marker(
        &self,
        Parameters(EchoMarkerRequest { value }): Parameters<EchoMarkerRequest>,
    ) -> String {
        let _ = &self.tool_router;
        value
    }

    #[tool(
        name = "delayed_write_marker",
        description = "Write a marker after a delay."
    )]
    fn delayed_write_marker(
        &self,
        Parameters(DelayedWriteRequest {
            path,
            value,
            delay_ms,
        }): Parameters<DelayedWriteRequest>,
    ) -> String {
        let _ = &self.tool_router;
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        std::fs::write(path, &value).expect("delayed marker write should succeed");
        value
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EchoMarkerServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Echo deterministic test markers")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    EchoMarkerServer::new()
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
