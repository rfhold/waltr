use super::support::{
    EcosystemServerCase, assert_standard_surface_with_mcp_client, start_mcp_server,
    start_rmcp_server,
};

pub async fn mcp_client_interoperates_with_ecosystem_servers() -> mcp::Result<()> {
    for case in server_cases() {
        let server = case.start().await?;
        assert_standard_surface_with_mcp_client(case.name, server.transport()).await?;
    }
    Ok(())
}

fn server_cases() -> Vec<EcosystemServerCase> {
    vec![
        EcosystemServerCase::new("rmcp::server streamable-http", start_rmcp_server),
        EcosystemServerCase::new("mcp::server streamable-http", start_mcp_server),
    ]
}
