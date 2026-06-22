use crate::support::grpc::{config_client, config_with_provider, get_config};
use waltr::grpc::waltr::config::v1::{PatchConfigRequest, ProviderMapPatch, WatchConfigRequest};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn config_service_watch_emits_initial_and_changed_config()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(server).await?;
    let initial = get_config(&mut client).await?;

    let mut watch = client
        .watch_config(WatchConfigRequest::default())
        .await?
        .into_inner();

    let initial_watch_event =
        tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
            .await??
            .expect("watch stream should emit initial config snapshot");
    assert!(initial_watch_event.previous_etag.is_empty());
    assert_eq!(initial_watch_event.etag, initial.etag);
    assert!(initial_watch_event.changed_keys.is_empty());
    assert!(initial_watch_event.error.is_empty());
    let initial_watch_config = initial_watch_event
        .config
        .expect("initial watch config should be present");
    assert_eq!(initial_watch_config.providers, initial.providers);
    assert_eq!(initial_watch_config.etag, initial.etag);

    let patch = config_with_provider("anthropic");
    let patch_response = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag.clone(),
            providers: Some(ProviderMapPatch {
                providers: patch.providers.clone(),
            }),
            mcp_servers: None,
            runtime_defaults: None,
        })
        .await?
        .into_inner();
    let patched = patch_response
        .config
        .as_ref()
        .expect("patched config should be present");
    assert_eq!(patched.providers, patch.providers);
    assert_ne!(patched.etag, initial.etag);

    let watch_event = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("watch stream should emit config change");
    assert_eq!(watch_event.previous_etag, initial.etag);
    assert_eq!(watch_event.etag, patched.etag);
    assert_eq!(watch_event.changed_keys, vec!["providers"]);
    assert!(watch_event.error.is_empty());
    assert_eq!(
        watch_event
            .config
            .expect("watch config should be present")
            .providers,
        patched.providers
    );

    Ok(())
}
