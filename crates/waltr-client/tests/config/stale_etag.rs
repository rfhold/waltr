use waltr_client::{
    ClientCommand, ClientOperationView, ClientRuntime, GrpcEffectExecutor, ResourceWrite,
};

use crate::support::{
    server::{absent_slot_etag, provider_resource_write, server_endpoint},
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn stale_resource_baseline_conflicts_instead_of_overwriting() {
    let client_a = GrpcEffectExecutor::for_endpoint(server_endpoint());
    let client_b = client_a.clone();

    let initial = client_a.load_config().await.expect("config should load");
    assert!(initial.providers.contains_key("anthropic"));
    assert!(initial.providers.contains_key("openai"));
    assert!(initial.providers.contains_key("codex"));
    assert!(!initial.etag.is_empty());

    let baseline_a = absent_slot_etag(&client_a).await.unwrap();
    let baseline_b = absent_slot_etag(&client_b).await.unwrap();
    client_b
        .write_resource(provider_resource_write("custom", baseline_b))
        .await
        .expect("client B should write first");

    let error = client_a
        .write_resource(provider_resource_write("custom", baseline_a))
        .await
        .expect_err("client A's stale baseline must conflict");

    assert!(error.to_string().contains("failed_precondition"));
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the two-client watch, conflict, and authoritative reload lifecycle stays explicit"
)]
async fn watched_update_after_form_open_conflicts_and_reloads_authoritative_state() {
    let endpoint = server_endpoint();
    let runtime = ClientRuntime::for_endpoint(endpoint.clone());
    let mut stream = runtime.into_state_stream(no_sleep);
    dispatch_stream_command(&mut stream, ClientCommand::RegisterConfigWatchInterest)
        .await
        .expect("config watch should register");
    let opened = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.watch.status == "received" && snapshot.config.current.is_some()
    })
    .await;
    let form_baseline = opened.config.resource_baselines["providers/anthropic.yaml"].clone();
    let original = opened
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("anthropic"))
        .cloned()
        .expect("anthropic provider");

    let remote_url = "https://remote.example/messages";
    let remote_source = form_baseline
        .active_source
        .replacen(&original.url, remote_url, 1);
    assert_ne!(remote_source, form_baseline.active_source);
    GrpcEffectExecutor::for_endpoint(endpoint)
        .write_resource(ResourceWrite::Replace {
            path: form_baseline.path.clone(),
            source: remote_source,
            expected_etag: form_baseline.persisted_etag.clone(),
        })
        .await
        .expect("client B should update provider");
    let watched = next_snapshot_until(&mut stream, |snapshot| {
        snapshot
            .config
            .current
            .as_ref()
            .and_then(|config| config.providers.get("anthropic"))
            .is_some_and(|provider| provider.url == remote_url)
    })
    .await;
    assert_ne!(
        watched.config.resource_baselines["providers/anthropic.yaml"].persisted_etag,
        form_baseline.persisted_etag
    );

    let mut draft = original.clone();
    draft.url = "https://draft.example/messages".to_owned();
    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateProvider {
            original_name: "anthropic".to_owned(),
            original: Box::new(original),
            baseline: form_baseline,
            provider: draft,
        },
    )
    .await
    .expect("stale form submission should start");
    let operation_key = started.config.patch.key.expect("patch operation key");
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

    assert!(
        failed
            .config
            .patch
            .error
            .as_deref()
            .is_some_and(|error| error.contains("failed_precondition"))
    );
    assert_eq!(
        failed
            .config
            .current
            .as_ref()
            .and_then(|config| config.providers.get("anthropic"))
            .map(|provider| provider.url.as_str()),
        Some(remote_url)
    );
}
