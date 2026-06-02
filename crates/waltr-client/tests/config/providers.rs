use waltr_client::{ClientCommand, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{no_sleep, provider_view},
};

#[tokio::test]
async fn runtime_dispatches_config_provider_commands() {
    let mut runtime = ClientRuntime::for_endpoint(server_endpoint());

    let initial = runtime
        .dispatch(ClientCommand::RequestConfig, no_sleep)
        .await
        .expect("config should load");
    let initial_config = initial
        .config
        .current
        .expect("config snapshot should include config");
    assert!(initial_config.providers.is_empty());
    assert!(!initial_config.etag.is_empty());

    let patched = runtime
        .dispatch(
            ClientCommand::AddProvider {
                provider: provider_view("anthropic"),
            },
            no_sleep,
        )
        .await
        .expect("provider should be added");
    let patched_config = patched
        .config
        .current
        .expect("patched snapshot should include config");
    assert_eq!(patched_config.providers.len(), 1);
    assert!(patched_config.providers.contains_key("anthropic"));
    assert_ne!(patched_config.etag, initial_config.etag);

    let duplicate_error = runtime
        .dispatch(
            ClientCommand::AddProvider {
                provider: provider_view("anthropic"),
            },
            no_sleep,
        )
        .await
        .expect_err("duplicate provider should be rejected");
    assert!(duplicate_error.to_string().contains("already used"));
}
