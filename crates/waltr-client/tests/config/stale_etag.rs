use waltr_client::GrpcEffectExecutor;

use crate::support::{server::server_endpoint, snapshots::config_with_provider};

#[tokio::test]
async fn grpc_executor_rejects_stale_etag() {
    let client = GrpcEffectExecutor::for_endpoint(server_endpoint());

    let initial = client.load_config().await.expect("config should load");
    let initial_etag = initial.etag.clone();
    assert!(initial.providers.is_empty());
    assert!(!initial.etag.is_empty());

    let patch = config_with_provider("anthropic");
    let patched = client
        .patch_config(patch, initial_etag.clone())
        .await
        .expect("config should patch");
    assert_ne!(patched.etag, initial_etag);

    let stale_patch = config_with_provider("openai");
    let error = client
        .patch_config(stale_patch, initial_etag)
        .await
        .expect_err("stale etag should be rejected");
    assert!(error.to_string().contains("failed_precondition"));
}
