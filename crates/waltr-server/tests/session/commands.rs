use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, InterruptSessionRequest, RespondExternalRequestRequest,
    SubmitUserMessageRequest,
};
use waltr_server::test_support::setup_server;
use tonic::Code;

#[tokio::test]
async fn submit_user_message_validates_request() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    let error = client
        .submit_user_message(SubmitUserMessageRequest {
            session_id: String::new(),
            text: "hello".to_owned(),
        })
        .await
        .expect_err("empty session id should fail");
    assert_eq!(error.code(), Code::InvalidArgument);

    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let error = client
        .submit_user_message(SubmitUserMessageRequest {
            session_id: session.id,
            text: "   ".to_owned(),
        })
        .await
        .expect_err("blank prompt should fail");
    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn submit_user_message_requires_session_runtime() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let error = client
        .submit_user_message(SubmitUserMessageRequest {
            session_id: session.id,
            text: "hello".to_owned(),
        })
        .await
        .expect_err("missing runtime should fail before starting a run");
    assert_eq!(error.code(), Code::FailedPrecondition);

    Ok(())
}

#[tokio::test]
async fn respond_external_request_validates_json() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let error = client
        .respond_external_request(RespondExternalRequestRequest {
            session_id: session.id,
            request_id: "request-1".to_owned(),
            response_json: "{".to_owned(),
        })
        .await
        .expect_err("invalid json should fail");
    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn interrupt_session_validates_session_id() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut client = session_client(server).await?;

    let error = client
        .interrupt_session(InterruptSessionRequest {
            session_id: String::new(),
        })
        .await
        .expect_err("empty session id should fail");
    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}
