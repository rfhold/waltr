use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep, provider_view},
};

#[tokio::test]
async fn provider_reset_removes_override_and_reveals_bundled_fallback() {
    let runtime = ClientRuntime::for_endpoint(server_endpoint());
    let mut stream = runtime.into_state_stream(no_sleep);
    let initial = load_config(&mut stream).await;
    let initial_provider = initial
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("anthropic"))
        .cloned()
        .expect("bundled provider");
    let baseline = initial.config.resource_baselines["providers/anthropic.yaml"].clone();

    let mut override_provider = provider_view("anthropic");
    override_provider.url = "https://override.example.test/messages".to_owned();
    dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateProvider {
            original_name: "anthropic".to_owned(),
            original: Box::new(initial_provider.clone()),
            baseline,
            provider: override_provider,
        },
    )
    .await
    .expect("provider override should start");
    let overridden = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.patch.status == "ready"
            && snapshot
                .config
                .current
                .as_ref()
                .and_then(|config| config.providers.get("anthropic"))
                .is_some_and(|provider| provider.url == "https://override.example.test/messages")
    })
    .await;

    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::DeleteProvider {
            name: "anthropic".to_owned(),
        },
    )
    .await
    .expect("provider reset should start");
    let operation_key = started.config.patch.key.expect("reset operation key");
    let reset = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::ConfigPatch {
                status: "ready",
                config: None,
                ..
            })
        ) && snapshot
            .config
            .current
            .as_ref()
            .and_then(|config| config.providers.get("anthropic"))
            == Some(&initial_provider)
    })
    .await;

    let reset_provider = reset
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("anthropic"))
        .expect("bundled fallback provider");
    assert_eq!(reset_provider, &initial_provider);
    assert_ne!(
        reset.config.current.as_ref().map(|config| &config.etag),
        overridden
            .config
            .current
            .as_ref()
            .map(|config| &config.etag)
    );
}

#[tokio::test]
async fn self_mcp_reset_fails_closed_when_bundled_template_is_unavailable() {
    let runtime = ClientRuntime::for_endpoint(server_endpoint());
    let mut stream = runtime.into_state_stream(no_sleep);
    let initial = load_config(&mut stream).await;
    let prior = initial.config.current.expect("seeded config");
    assert!(prior.mcp_servers.contains_key("waltr"));

    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::DeleteMcpServer {
            name: "waltr".to_owned(),
        },
    )
    .await
    .expect("self MCP reset should start");
    let operation_key = started.config.patch.key.expect("reset operation key");
    let failed = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::ConfigPatch {
                status: "failed",
                ..
            })
        )
    })
    .await;

    assert_eq!(failed.config.current.as_ref(), Some(&prior));
    assert!(
        failed
            .config
            .patch
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid_argument"))
    );
    assert!(matches!(
        failed.operations.current.get(&operation_key),
        Some(ClientOperationView::ConfigPatch {
            status: "failed",
            config: None,
            ..
        })
    ));
}

#[tokio::test]
async fn rename_failure_keeps_destination_and_reloads_authoritative_state() {
    let endpoint = server_endpoint();
    let runtime = ClientRuntime::for_endpoint(endpoint.clone());
    let mut stream = runtime.into_state_stream(no_sleep);
    let initial = load_config(&mut stream).await;
    let baseline = initial.config.resource_baselines["mcp-servers/waltr.yaml"].clone();
    let prior = initial.config.current.expect("seeded config");
    let mut renamed = prior
        .mcp_servers
        .get("waltr")
        .cloned()
        .expect("self MCP server");
    let original = renamed.clone();
    renamed.name = "renamed-waltr".to_owned();

    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateMcpServer {
            original_name: "waltr".to_owned(),
            original: Box::new(original),
            baseline,
            server: renamed,
        },
    )
    .await
    .expect("rename should start");
    let operation_key = started.config.patch.key.expect("rename operation key");
    let failed = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::ConfigPatch {
                status: "failed",
                ..
            })
        )
    })
    .await;

    let current = failed
        .config
        .current
        .expect("failed write should reload catalog");
    assert!(current.mcp_servers.contains_key("renamed-waltr"));
    assert!(current.mcp_servers.contains_key("waltr"));
    let authoritative = waltr_client::GrpcEffectExecutor::for_endpoint(endpoint)
        .load_config()
        .await
        .expect("authoritative config should load");
    assert_eq!(current, authoritative);
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
