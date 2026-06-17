use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{CreateSessionRequest, ListSessionsRequest, SessionPhase};
use waltr_server::test_support::setup_server;

#[tokio::test]
async fn session_service_list_returns_created_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    let first = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("first created session should be present");
    let second = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("second created session should be present");

    let sessions = client
        .list_sessions(ListSessionsRequest::default())
        .await?
        .into_inner()
        .sessions;

    assert_eq!(sessions.len(), 2);
    for id in [&first.id, &second.id] {
        let session = sessions
            .iter()
            .find(|session| &session.id == id)
            .expect("created session should be listed");
        assert_eq!(session.revision, 0);
        assert_eq!(
            session
                .status
                .as_ref()
                .expect("status should be present")
                .phase,
            SessionPhase::Idle as i32
        );
    }

    Ok(())
}
