use crate::support::grpc::{config_client, config_with_provider, get_config};
use waltr::grpc::waltr::config::v1::{PatchConfigRequest, ProviderMapPatch};
use waltr_server::test_support::setup_server;
use tonic::Code;

#[tokio::test]
async fn config_service_rejects_stale_etag() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(&server).await?;
    let initial = get_config(&mut client).await?;

    client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag.clone(),
            providers: Some(ProviderMapPatch {
                providers: config_with_provider("anthropic").providers,
            }),
            mcp_servers: None,
        })
        .await?;

    let error = client
        .patch_config(PatchConfigRequest {
            expected_etag: initial.etag,
            providers: Some(ProviderMapPatch {
                providers: config_with_provider("openai").providers,
            }),
            mcp_servers: None,
        })
        .await
        .expect_err("stale etag should be rejected");
    assert_eq!(error.code(), Code::FailedPrecondition);

    Ok(())
}
