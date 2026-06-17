use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{CreateSessionRequest, SessionPhase};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn session_service_create_returns_idle_session() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    assert!(!session.id.is_empty());
    assert_eq!(session.revision, 0);
    assert_eq!(
        session.status.expect("status should be present").phase,
        SessionPhase::Idle as i32
    );
    assert!(session.current_prompt.is_none());
    assert!(session.created_at.is_some());
    assert!(session.updated_at.is_some());
    assert!(session.last_activity_at.is_some());

    Ok(())
}
