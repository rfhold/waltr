use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, DeleteSessionRequest, ListSessionsRequest,
};
use waltr_server::test_support::setup_server;
use tonic::Code;

#[tokio::test]
async fn removes_session_from_list() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    let created = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    client
        .delete_session(DeleteSessionRequest {
            id: created.id.clone(),
        })
        .await?;

    let sessions = client
        .list_sessions(ListSessionsRequest::default())
        .await?
        .into_inner()
        .sessions;

    assert!(sessions.iter().all(|session| session.id != created.id));

    Ok(())
}

#[tokio::test]
async fn missing_session_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    client
        .delete_session(DeleteSessionRequest {
            id: "never-created".to_owned(),
        })
        .await?;

    Ok(())
}

#[tokio::test]
async fn empty_id_returns_invalid_argument() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    let status = client
        .delete_session(DeleteSessionRequest {
            id: "  ".to_owned(),
        })
        .await
        .expect_err("empty id should be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);

    Ok(())
}
