use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, DeleteSessionRequest, ListSessionsRequest, WipeSessionsRequest,
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

#[tokio::test]
async fn wipe_requires_confirmation() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;
    client
        .create_session(CreateSessionRequest::default())
        .await?;

    let status = client
        .wipe_sessions(WipeSessionsRequest { confirm: false })
        .await
        .expect_err("unconfirmed wipe should be rejected");

    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        client
            .list_sessions(ListSessionsRequest::default())
            .await?
            .into_inner()
            .sessions
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn wipe_is_empty_idempotent_and_removes_all_sessions()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    client
        .wipe_sessions(WipeSessionsRequest { confirm: true })
        .await?;
    client
        .create_session(CreateSessionRequest::default())
        .await?;
    client
        .create_session(CreateSessionRequest::default())
        .await?;
    client
        .wipe_sessions(WipeSessionsRequest { confirm: true })
        .await?;
    client
        .wipe_sessions(WipeSessionsRequest { confirm: true })
        .await?;

    assert!(
        client
            .list_sessions(ListSessionsRequest::default())
            .await?
            .into_inner()
            .sessions
            .is_empty()
    );
    Ok(())
}
