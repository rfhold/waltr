use crate::support::grpc::{config_client, get_config};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn config_service_get_returns_empty_config_with_etag()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = config_client(server).await?;

    let initial = get_config(&mut client).await?;

    assert!(initial.providers.is_empty());
    assert!(!initial.etag.is_empty());

    Ok(())
}
