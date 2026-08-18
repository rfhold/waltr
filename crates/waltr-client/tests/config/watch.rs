use std::time::Duration;

use futures_util::StreamExt;
use waltr_client::GrpcEffectExecutor;

use crate::support::{
    server::{absent_slot_etag, provider_resource_write, server_endpoint},
    snapshots::{assert_initial_watch_event, assert_watch_event},
};

#[tokio::test]
async fn grpc_executor_watches_config_changes() {
    let client = GrpcEffectExecutor::for_endpoint(server_endpoint());

    let initial = client.load_config().await.expect("config should load");
    let initial_etag = initial.etag.clone();
    assert!(initial.providers.contains_key("anthropic"));
    assert!(initial.providers.contains_key("openai"));
    assert!(initial.providers.contains_key("codex"));
    assert!(!initial.etag.is_empty());

    let mut watch = client
        .watch_config()
        .await
        .expect("config watch should start");
    let initial_watch_event = tokio::time::timeout(Duration::from_secs(2), watch.next())
        .await
        .expect("watch stream should emit initial config")
        .expect("watch stream should remain open")
        .expect("initial watch event should succeed");
    assert_initial_watch_event(&initial_watch_event, &initial);

    let expected_etag = absent_slot_etag(&client).await.unwrap();
    client
        .write_resource(provider_resource_write("custom", expected_etag))
        .await
        .expect("config should patch");
    let patched = client
        .load_config()
        .await
        .expect("patched config should load");
    assert!(patched.providers.contains_key("custom"));
    assert_ne!(patched.etag, initial_etag);

    let watch_event = tokio::time::timeout(Duration::from_secs(2), watch.next())
        .await
        .expect("watch stream should emit config change")
        .expect("watch stream should remain open")
        .expect("watch event should succeed");
    assert_watch_event(&watch_event, &initial_etag, &patched);
}
