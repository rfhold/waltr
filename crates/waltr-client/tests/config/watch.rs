use std::time::Duration;

use futures_util::StreamExt;
use waltr_client::GrpcEffectExecutor;

use crate::support::{
    server::server_endpoint,
    snapshots::{
        assert_config_matches_without_etag, assert_initial_watch_event, assert_watch_event,
        config_with_provider,
    },
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

    let patch = config_with_provider("anthropic");
    let patched = client
        .patch_config(patch.clone(), initial_etag.clone())
        .await
        .expect("config should patch");
    assert_config_matches_without_etag(&patched, &patch);
    assert_ne!(patched.etag, initial_etag);

    let watch_event = tokio::time::timeout(Duration::from_secs(2), watch.next())
        .await
        .expect("watch stream should emit config change")
        .expect("watch stream should remain open")
        .expect("watch event should succeed");
    assert_watch_event(&watch_event, &initial_etag, &patched);
}
