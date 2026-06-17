use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{CreateSessionRequest, SessionPhase, WatchSessionsRequest};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn session_service_watch_emits_created_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut watch_client = session_client(server).await?;
    let mut create_client = session_client(server).await?;
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();

    let created = create_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let update = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("watch stream should emit created session");
    let watched = update.session.expect("watched session should be present");

    assert!(update.sequence > 0);
    assert_eq!(watched.id, created.id);
    assert_eq!(watched.revision, 0);
    assert_eq!(
        watched.status.expect("status should be present").phase,
        SessionPhase::Idle as i32
    );

    Ok(())
}
