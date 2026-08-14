use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::{server_endpoint, server_endpoint_with_unavailable_config_reset},
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep, provider_view},
};

#[tokio::test]
async fn stream_resets_config_and_exposes_returned_snapshot() {
    let runtime = ClientRuntime::for_endpoint(server_endpoint());
    let mut stream = runtime.into_state_stream(no_sleep);
    let initial = load_config(&mut stream).await;
    let initial_config = initial.config.current.expect("initial config");

    dispatch_stream_command(
        &mut stream,
        ClientCommand::AddProvider {
            provider: provider_view("custom"),
        },
    )
    .await
    .expect("custom config patch should start");
    let custom = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.patch.status == "ready"
    })
    .await
    .config
    .current
    .expect("custom config");
    assert!(custom.providers.contains_key("custom"));

    let started = dispatch_stream_command(&mut stream, ClientCommand::ResetConfig)
        .await
        .expect("reset should start");
    assert_eq!(started.config.reset.status, "loading");
    assert_eq!(started.config.current.as_ref(), Some(&custom));
    let operation_key = started.config.reset.key.expect("reset operation key");
    assert!(matches!(
        started.operations.current.get(&operation_key),
        Some(ClientOperationView::ConfigReset {
            status: "loading",
            config: None,
            ..
        })
    ));

    let reset = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::ConfigReset {
                status: "ready",
                ..
            })
        )
    })
    .await;
    let reset_config = reset.config.current.as_ref().expect("reset config");
    assert_eq!(reset.config.reset.status, "ready");
    assert_eq!(
        reset.config.reset.key.as_deref(),
        Some(operation_key.as_str())
    );
    assert_eq!(reset_config.providers, initial_config.providers);
    assert_eq!(reset_config.mcp_servers, initial_config.mcp_servers);
    assert_eq!(
        reset_config.inference_defaults,
        initial_config.inference_defaults
    );
    assert_ne!(reset_config.etag, custom.etag);
    let Some(ClientOperationView::ConfigReset {
        config: Some(operation_config),
        ..
    }) = reset.operations.current.get(&operation_key)
    else {
        panic!("reset operation should include returned config");
    };
    assert_eq!(operation_config, reset_config);
}

#[tokio::test]
async fn reset_failure_preserves_prior_config() {
    let endpoint = server_endpoint_with_unavailable_config_reset()
        .await
        .expect("failure server should start");
    let runtime = ClientRuntime::for_endpoint(endpoint);
    let mut stream = runtime.into_state_stream(no_sleep);
    let prior = load_config(&mut stream)
        .await
        .config
        .current
        .expect("seeded config");

    let started = dispatch_stream_command(&mut stream, ClientCommand::ResetConfig)
        .await
        .expect("reset should start");
    let operation_key = started.config.reset.key.expect("reset operation key");
    let failed = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.reset.status == "failed"
    })
    .await;

    assert_eq!(failed.config.current.as_ref(), Some(&prior));
    assert!(
        failed
            .config
            .reset
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unavailable"))
    );
    assert!(matches!(
        failed.operations.current.get(&operation_key),
        Some(ClientOperationView::ConfigReset {
            status: "failed",
            config: None,
            ..
        })
    ));
}

#[tokio::test]
async fn reset_rejects_stale_loaded_etag_and_preserves_snapshot() {
    let endpoint = server_endpoint();
    let primary = ClientRuntime::for_endpoint(endpoint.clone());
    let mut primary_stream = primary.into_state_stream(no_sleep);
    let prior = load_config(&mut primary_stream)
        .await
        .config
        .current
        .expect("primary config");

    let competitor = ClientRuntime::for_endpoint(endpoint);
    let mut competitor_stream = competitor.into_state_stream(no_sleep);
    load_config(&mut competitor_stream).await;
    dispatch_stream_command(
        &mut competitor_stream,
        ClientCommand::AddProvider {
            provider: provider_view("competitor"),
        },
    )
    .await
    .expect("competitor patch should start");
    next_snapshot_until(&mut competitor_stream, |snapshot| {
        snapshot.config.patch.status == "ready"
    })
    .await;

    dispatch_stream_command(&mut primary_stream, ClientCommand::ResetConfig)
        .await
        .expect("stale reset should start");
    let failed = next_snapshot_until(&mut primary_stream, |snapshot| {
        snapshot.config.reset.status == "failed"
    })
    .await;

    assert_eq!(failed.config.current.as_ref(), Some(&prior));
    assert!(
        failed
            .config
            .reset
            .error
            .as_deref()
            .is_some_and(|error| error.contains("failed_precondition"))
    );
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
