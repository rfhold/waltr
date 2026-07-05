use mcp::client::McpTransportConfig;

use super::support::{
    BoxTestFuture, EcosystemClientCase, assert_standard_surface_with_mcp_client,
    assert_standard_surface_with_rmcp_client, start_mcp_server,
};

pub async fn ecosystem_clients_interoperate_with_mcp_server() -> mcp::Result<()> {
    let server = start_mcp_server().await?;
    for case in client_cases() {
        case.run(server.transport()).await?;
    }
    Ok(())
}

fn client_cases() -> Vec<EcosystemClientCase> {
    vec![
        EcosystemClientCase::new("rmcp::client streamable-http", rmcp_client_case),
        EcosystemClientCase::new("mcp::client streamable-http", mcp_client_case),
    ]
}

fn rmcp_client_case(transport: McpTransportConfig) -> BoxTestFuture<mcp::Result<()>> {
    Box::pin(async move { assert_standard_surface_with_rmcp_client(transport).await })
}

fn mcp_client_case(transport: McpTransportConfig) -> BoxTestFuture<mcp::Result<()>> {
    Box::pin(async move { assert_standard_surface_with_mcp_client("mcp::client", transport).await })
}
