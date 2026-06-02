use waltr_client::GrpcEffectExecutor;

use crate::support::{
    server::server_endpoint,
    snapshots::{assert_config_matches, assert_config_matches_without_etag, config_with_provider},
};

#[tokio::test]
async fn grpc_executor_loads_and_reloads_config() {
    let client = GrpcEffectExecutor::for_endpoint(server_endpoint());

    let initial = client.load_config().await.expect("config should load");
    let initial_etag = initial.etag.clone();
    assert!(initial.providers.is_empty());
    assert!(!initial.etag.is_empty());

    let patch = config_with_provider("anthropic");
    let patched = client
        .patch_config(patch.clone(), initial_etag)
        .await
        .expect("config should patch");
    assert_config_matches_without_etag(&patched, &patch);

    let reloaded = client.load_config().await.expect("config should reload");
    assert_config_matches(&reloaded, &patched);
}
