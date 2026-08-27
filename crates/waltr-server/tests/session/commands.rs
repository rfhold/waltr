use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    ContinueSessionRequest, CreateSessionRequest, InterruptSessionRequest,
    RespondExternalRequestRequest, RollbackSessionRequest, RollbackTarget, SessionPhase,
    SubmitUserMessageRequest, WatchTimelineRequest, rollback_target, watch_timeline_response,
};
use waltr_agent::{
    CHANNEL_EXTERNAL_REQUEST, ExternalRequest, ExternalRequestId, ExternalRequestKind,
    ExternalRequestStatus, ExternalResponse, PendingTimelineEvent, SessionCreateRequest, SessionId,
    TimelineChannel, TimelineContent, TimelineEventKind, TimelineStatus,
};
use waltr_server::test_support::{setup_server, setup_server_with_sessions};
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

#[tokio::test]
async fn rollback_session_requires_target() -> Result<(), Box<dyn std::error::Error>> {
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

#[tokio::test]
async fn rollback_to_open_external_request_restores_suspended_prompt()
-> Result<(), Box<dyn std::error::Error>> {
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
                external_request_event(&session_id, ExternalRequestStatus::Pending),
                external_request_event(
                    &session_id,
                    ExternalRequestStatus::Responded(ExternalResponse::new(
                        serde_json::json!({ "answers": [{ "header": "Choice" }] }),
                    )),
                ),
            ],
        )
        .await?;

    let response = client
        .rollback_session(RollbackSessionRequest {
            session_id: session.id.clone(),
            target: Some(RollbackTarget {
                target: Some(rollback_target::Target::Cursor(1)),
            }),
        })
        .await?
        .into_inner();

    let summary = response.session.expect("session summary should be present");
    assert_eq!(
        summary.status.expect("status should be present").phase,
        SessionPhase::Suspended as i32
    );
    let truncation = response.truncation.expect("truncation should be present");
    assert_eq!(truncation.session_id, session.id);
    assert_eq!(truncation.after_cursor, Some(1));
    Ok(())
}

fn external_request_event(
    session_id: &SessionId,
    status: ExternalRequestStatus,
) -> PendingTimelineEvent {
    let kind = match status {
        ExternalRequestStatus::Pending => TimelineEventKind::ExternalRequestOpened,
        ExternalRequestStatus::Responded(_) | ExternalRequestStatus::Canceled(_) => {
            TimelineEventKind::ExternalRequestResolved
        }
    };
    let timeline_status = TimelineStatus::from(&status);
    let mut event = PendingTimelineEvent::new(
        session_id.clone(),
        1,
        waltr_agent::PendingTimelineItemId::New,
        kind,
        Some(TimelineChannel::from(CHANNEL_EXTERNAL_REQUEST)),
        TimelineContent::ExternalRequest {
            request: ExternalRequest {
                id: ExternalRequestId::new("question-1"),
                kind: ExternalRequestKind::Question,
                tool_call_id: Some("call-question".to_owned()),
                prompt: "Choose a path".to_owned(),
                payload: serde_json::json!({
                    "questions": [{
                        "header": "Choice",
                        "question": "Continue?",
                        "multiple": false,
                        "options": [{ "label": "Yes", "description": "Continue" }]
                    }]
                }),
                status,
            },
        },
    );
    event.status = Some(timeline_status);
    event
}
