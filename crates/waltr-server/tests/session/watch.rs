use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, DeleteSessionRequest, SessionPhase, WatchSessionsRequest,
    watch_sessions_response,
};
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
    let watched = match update.event.expect("watched event should be present") {
        watch_sessions_response::Event::Session(session) => session,
        watch_sessions_response::Event::DeletedId(id) => {
            panic!("expected session event, got deleted id {id}")
        }
    };

    assert!(update.sequence > 0);
    assert_eq!(watched.id, created.id);
    assert_eq!(watched.revision, 0);
    assert_eq!(
        watched.status.expect("status should be present").phase,
        SessionPhase::Idle as i32
    );

    Ok(())
}

#[tokio::test]
async fn receives_deleted_session_event() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut watch_client = session_client(server).await?;
    let mut mutation_client = session_client(server).await?;
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();

    let created = mutation_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let created_update = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("watch stream should emit created session");
    match created_update
        .event
        .expect("created event should be present")
    {
        watch_sessions_response::Event::Session(session) => assert_eq!(session.id, created.id),
        watch_sessions_response::Event::DeletedId(id) => {
            panic!("expected session event, got deleted id {id}")
        }
    }

    mutation_client
        .delete_session(DeleteSessionRequest {
            id: created.id.clone(),
        })
        .await?;

    let deleted_update = tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
        .await??
        .expect("watch stream should emit deleted session");
    match deleted_update
        .event
        .expect("deleted event should be present")
    {
        watch_sessions_response::Event::DeletedId(id) => assert_eq!(id, created.id),
        watch_sessions_response::Event::Session(session) => {
            panic!("expected deleted id event, got session {}", session.id)
        }
    }

    assert!(deleted_update.sequence > created_update.sequence);

    Ok(())
}
