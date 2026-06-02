use crate::support::grpc::status_client;
use waltr::grpc::waltr::status::v1::WatchStatusRequest;
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn status_service_watch_reports_ready() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = status_client(&server).await?;

    let message = client
        .watch_status(WatchStatusRequest::default())
        .await?
        .into_inner()
        .message()
        .await?
        .expect("status stream should emit initial status");
    let status = message.status.expect("status should be present");

    assert!(status.ready);

    Ok(())
}
