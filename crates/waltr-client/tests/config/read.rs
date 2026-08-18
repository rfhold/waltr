use waltr_client::GrpcEffectExecutor;

use crate::support::server::{absent_slot_etag, provider_resource_write, server_endpoint};

#[tokio::test]
async fn grpc_executor_loads_and_reloads_config() {
    let client = GrpcEffectExecutor::for_endpoint(server_endpoint());

    let initial = client.load_config().await.expect("config should load");
    assert!(initial.providers.contains_key("anthropic"));
    assert!(initial.providers.contains_key("openai"));
    assert!(initial.providers.contains_key("codex"));
    assert!(!initial.etag.is_empty());

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

    let reloaded = client.load_config().await.expect("config should reload");
    assert_eq!(reloaded, patched);
}
