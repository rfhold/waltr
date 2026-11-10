use crate::support::grpc::session_client;
use tonic::Code;
use waltr::grpc::waltr::session::v1::{
    ContinueSessionRequest, CreateSessionRequest, InterruptSessionRequest, RollbackSessionRequest,
    RollbackTarget, SubmitUserMessageRequest, WatchTimelineRequest, rollback_target,
    watch_timeline_response,
};
use waltr_agent::{PendingTimelineEvent, SessionCreateRequest, SessionId, TimelineContent};
use waltr_server::test_support::{setup_server, setup_server_with_sessions};

#[tokio::test]
async fn submit_user_message_validates_request() -> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
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
    waltr_server::isolate_application_test!();
    let (server, sessions) = setup_server_with_sessions();
    let session = sessions
        .create(SessionCreateRequest::new(SessionId::generate()))
        .await?;
    let mut client = session_client(server).await?;

    let error = client
        .submit_user_message(SubmitUserMessageRequest {
            session_id: session.record.id.as_str().to_owned(),
            text: "hello".to_owned(),
        })
        .await
        .expect_err("missing runtime should fail before starting a run");
    assert_eq!(error.code(), Code::FailedPrecondition);

    Ok(())
}

#[tokio::test]
async fn interaction_response_requires_typed_payload() -> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let server = setup_server();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let mut interactions = crate::support::grpc::interaction_client(server).await?;
    let error = interactions
        .respond_interaction(waltr::grpc::waltr::session::v1::RespondInteractionRequest {
            session_id: session.id,
            id: "request-1".to_owned(),
            generation: "generation".into(),
            operation_id: "operation".into(),
            response: None,
        })
        .await
        .expect_err("missing typed response should fail");
    assert_eq!(error.code(), Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn interrupt_session_validates_session_id() -> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
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

#[tokio::test]
async fn rollback_session_requires_target() -> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let server = setup_server();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let error = client
        .rollback_session(RollbackSessionRequest {
            session_id: session.id,
            target: None,
        })
        .await
        .expect_err("missing target should fail");

    assert_eq!(error.code(), Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn continue_session_rejects_before_cursor_target() -> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let server = setup_server();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let error = client
        .continue_session(ContinueSessionRequest {
            session_id: session.id,
            target: Some(RollbackTarget {
                target: Some(rollback_target::Target::BeforeCursor(1)),
            }),
        })
        .await
        .expect_err("before-cursor continue should fail");

    assert_eq!(error.code(), Code::InvalidArgument);
    Ok(())
}

#[tokio::test]
async fn rollback_before_cursor_returns_truncation() -> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let (server, sessions) = setup_server_with_sessions();
    let mut client = session_client(server).await?;
    let session = client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let session_id = SessionId::new(session.id.clone());
    sessions
        .append_timeline(
            &session_id,
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "first prompt",
                ),
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    2,
                    waltr_agent::PendingTimelineItemId::New,
                    "second prompt",
                ),
            ],
        )
        .await?;

    let response = client
        .rollback_session(RollbackSessionRequest {
            session_id: session.id.clone(),
            target: Some(RollbackTarget {
                target: Some(rollback_target::Target::BeforeCursor(2)),
            }),
        })
        .await?
        .into_inner();

    let truncation = response.truncation.expect("truncation should be present");
    assert_eq!(truncation.session_id, session.id);
    assert_eq!(truncation.after_cursor, Some(1));
    Ok(())
}

#[tokio::test]
async fn rollback_before_cursor_publishes_timeline_truncation()
-> Result<(), Box<dyn std::error::Error>> {
    waltr_server::isolate_application_test!();
    let (server, sessions) = setup_server_with_sessions();
    let mut command_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let session = command_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");
    let session_id = SessionId::new(session.id.clone());
    sessions
        .append_timeline(
            &session_id,
            vec![
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    1,
                    waltr_agent::PendingTimelineItemId::New,
                    "first prompt",
                ),
                PendingTimelineEvent::user_text_snapshot(
                    session_id.clone(),
                    2,
                    waltr_agent::PendingTimelineItemId::New,
                    "second prompt",
                ),
            ],
        )
        .await?;
    let mut watch = watch_client
        .watch_timeline(WatchTimelineRequest {
            session_id: session.id.clone(),
            projection_mode: 0,
            after_cursor: Some(0),
            channels: Vec::new(),
            initial_limit: None,
        })
        .await?
        .into_inner();

    command_client
        .rollback_session(RollbackSessionRequest {
            session_id: session.id.clone(),
            target: Some(RollbackTarget {
                target: Some(rollback_target::Target::BeforeCursor(2)),
            }),
        })
        .await?;

    let truncation = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let update = watch
                .message()
                .await?
                .expect("watch stream should emit timeline update");
            if let Some(watch_timeline_response::Update::Truncation(truncation)) = update.update {
                return Ok::<_, tonic::Status>(truncation);
            }
        }
    })
    .await??;

    assert_eq!(truncation.session_id, session.id);
    assert_eq!(truncation.after_cursor, Some(1));
    Ok(())
}

#[test]
fn legacy_external_request_history_is_rejected_instead_of_recreating_live_work() {
    let history = serde_json::json!({"ExternalRequest":{"request":{
        "id":"question-1","kind":"Question","tool_call_id":"call-question",
        "prompt":"Choose a path","payload":{},"status":"Pending"
    }}});
    assert!(serde_json::from_value::<TimelineContent>(history).is_err());
}
