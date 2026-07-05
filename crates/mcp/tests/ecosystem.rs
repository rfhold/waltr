#[path = "ecosystem/mod.rs"]
mod ecosystem;

#[tokio::test]
async fn mcp_client_interoperates_with_ecosystem_servers() -> mcp::Result<()> {
    ecosystem::client_against_servers::mcp_client_interoperates_with_ecosystem_servers().await
}

#[tokio::test]
async fn ecosystem_clients_interoperate_with_mcp_server() -> mcp::Result<()> {
    ecosystem::server_against_clients::ecosystem_clients_interoperate_with_mcp_server().await
}
