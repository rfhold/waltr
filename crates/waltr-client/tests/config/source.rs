use waltr_client::{ClientCommand, ClientRuntime, GrpcEffectExecutor};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn provider_and_mcp_edits_preserve_bundled_raw_source() {
    let endpoint = server_endpoint();
    let runtime = ClientRuntime::for_endpoint(endpoint.clone());
    let mut stream = runtime.into_state_stream(no_sleep);
    let initial = load_config(&mut stream).await;
    let provider_baseline = initial.config.resource_baselines["providers/codex.yaml"].clone();
    let mcp_baseline = initial.config.resource_baselines["mcp-servers/waltr.yaml"].clone();
    let config = initial.config.current.expect("initial config");
    let initial_catalog = GrpcEffectExecutor::for_endpoint(endpoint.clone())
        .load_resource_catalog()
        .await
        .expect("initial catalog");
    let initial_mcp_source = resource_source(&initial_catalog, "mcp-servers/waltr.yaml").to_owned();

    let mut codex = config
        .providers
        .get("codex")
        .cloned()
        .expect("codex provider");
    let original_codex = codex.clone();
    codex.url = "https://edited.example.test/responses".to_owned();
    dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateProvider {
            original_name: "codex".to_owned(),
            original: Box::new(original_codex),
            baseline: provider_baseline,
            provider: codex,
        },
    )
    .await
    .expect("codex edit should start");
    next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.patch.status == "ready"
    })
    .await;

    let waltr = config
        .mcp_servers
        .get("waltr")
        .cloned()
        .expect("self MCP server");
    dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateMcpServer {
            original_name: "waltr".to_owned(),
            original: Box::new(waltr.clone()),
            baseline: mcp_baseline,
            server: waltr,
        },
    )
    .await
    .expect("self MCP edit should start");
    next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.patch.status == "ready"
    })
    .await;

    let catalog = GrpcEffectExecutor::for_endpoint(endpoint)
        .load_resource_catalog()
        .await
        .expect("catalog should load");
    let codex_source = resource_source(&catalog, "providers/codex.yaml");
    assert!(codex_source.contains("include:\n    - gpt-5.*"));
    assert!(codex_source.contains("keyPrefix: codex/"));
    assert!(codex_source.contains("codex/gpt-5.6: gpt-5.6"));
    let mcp_source = resource_source(&catalog, "mcp-servers/waltr.yaml");
    assert_eq!(mcp_source, initial_mcp_source);
}

fn resource_source<'a>(catalog: &'a waltr_client::ResourceCatalog, path: &str) -> &'a str {
    &catalog
        .resources
        .iter()
        .find(|resource| resource.path == path)
        .unwrap_or_else(|| panic!("resource '{path}' missing"))
        .active_source
}

#[allow(
    clippy::future_not_send,
    reason = "the client state stream intentionally runs on a local task set"
)]
async fn load_config(stream: &mut waltr_client::ClientStateStream) -> waltr_client::ClientSnapshot {
    dispatch_stream_command(stream, ClientCommand::RequestConfig)
        .await
        .expect("config load should start");
    next_snapshot_until(stream, |snapshot| {
        snapshot.config.load.status == "ready" && snapshot.config.current.is_some()
    })
    .await
}
