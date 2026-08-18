use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep, provider_view},
};

#[tokio::test]
async fn stream_dispatches_config_provider_commands() {
    let runtime = ClientRuntime::for_endpoint(server_endpoint());
    let mut stream = runtime.into_state_stream(no_sleep);

    dispatch_stream_command(&mut stream, ClientCommand::RequestConfig)
        .await
        .expect("config should load");
    let initial = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.load.status == "ready" && snapshot.config.current.is_some()
    })
    .await;
    let initial_config = initial
        .config
        .current
        .expect("config snapshot should include config");
    assert!(initial_config.providers.contains_key("anthropic"));
    assert!(initial_config.providers.contains_key("openai"));
    assert!(initial_config.providers.contains_key("codex"));
    assert!(!initial_config.etag.is_empty());

    let patch_started = dispatch_stream_command(
        &mut stream,
        ClientCommand::AddProvider {
            provider: provider_view("custom-anthropic"),
        },
    )
    .await
    .expect("provider should be added");
    assert_eq!(patch_started.config.patch.status, "loading");
    let operation_key = patch_started
        .config
        .patch
        .key
        .clone()
        .expect("config patch should expose operation key");
    assert!(matches!(
        patch_started.operations.current.get(&operation_key),
        Some(ClientOperationView::ConfigPatch {
            status: "loading",
            ..
        })
    ));

    let patched = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::ConfigPatch {
                status: "ready",
                ..
            })
        )
    })
    .await;
    let patched_config = patched
        .config
        .current
        .expect("patched snapshot should include config");
    assert_eq!(
        patched.config.patch.key.as_deref(),
        Some(operation_key.as_str())
    );
    assert!(matches!(
        patched.operations.current.get(&operation_key),
        Some(ClientOperationView::ConfigPatch {
            status: "ready",
            config: None,
            ..
        })
    ));
    assert_eq!(
        patched_config.providers.len(),
        initial_config.providers.len() + 1
    );
    assert!(patched_config.providers.contains_key("custom-anthropic"));
    assert_ne!(patched_config.etag, initial_config.etag);

    let duplicate_error = dispatch_stream_command(
        &mut stream,
        ClientCommand::AddProvider {
            provider: provider_view("custom-anthropic"),
        },
    )
    .await
    .expect_err("duplicate provider should be rejected");
    assert!(duplicate_error.to_string().contains("already used"));
}
