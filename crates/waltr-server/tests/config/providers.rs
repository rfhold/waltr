use crate::support::grpc::{config_client, config_with_provider, get_config};
use waltr::grpc::waltr::config::v1::{PatchConfigRequest, ProviderMapPatch, ProviderType};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn config_service_patch_persists_provider() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(server).await?;
    let initial = get_config(&mut client).await?;

    let patch = config_with_provider("anthropic");
    let patched = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: Some(ProviderMapPatch {
                providers: patch.providers.clone(),
            }),
            mcp_servers: None,
            runtime_defaults: None,
        })
        .await?
        .into_inner()
        .config
        .expect("patched config should be present");

    assert_eq!(patched.providers, patch.providers);

    let current = get_config(&mut client).await?;
    assert_eq!(current.providers, patched.providers);
    assert_eq!(current.etag, patched.etag);
    assert_eq!(
        current.providers["anthropic"].r#type,
        ProviderType::AnthropicMessages as i32
    );
    assert_eq!(current.providers["anthropic"].name, "anthropic");
    assert_eq!(
        current.providers["anthropic"].url,
        "https://api.anthropic.com/v1/messages"
    );

    Ok(())
}
