use crate::support::grpc::session_client;
use waltr::grpc::waltr::session::v1::{
    CreateSessionRequest, DeleteSessionRequest, SessionPhase, WatchSessionsRequest,
    WipeSessionsRequest, watch_sessions_response,
};
use waltr_agent::{PendingTimelineEvent, PendingTimelineItemId};
use waltr_server::test_support::{setup_server, setup_server_with_sessions};

#[tokio::test]
async fn session_service_watch_emits_created_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut watch_client = session_client(server).await?;
    let mut create_client = session_client(server).await?;
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let snapshot = next_watch_message(&mut watch).await?;
    assert_snapshot_event(&snapshot, &[]);

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
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected session event, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
        watch_sessions_response::Event::Cleared(_) => panic!("expected session event, got cleared"),
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
    let snapshot = next_watch_message(&mut watch).await?;
    assert_snapshot_event(&snapshot, &[]);

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
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected session event, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
        watch_sessions_response::Event::Cleared(_) => panic!("expected session event, got cleared"),
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
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected deleted id event, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
        watch_sessions_response::Event::Cleared(_) => {
            panic!("expected deleted id event, got cleared")
        }
    }

    assert!(deleted_update.sequence > created_update.sequence);

    Ok(())
}

#[tokio::test]
async fn multiple_watchers_receive_created_and_deleted_sessions()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut first_watch_client = session_client(server).await?;
    let mut second_watch_client = session_client(server).await?;
    let mut mutation_client = session_client(server).await?;
    let mut first_watch = first_watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let mut second_watch = second_watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let first_snapshot = next_watch_message(&mut first_watch).await?;
    let second_snapshot = next_watch_message(&mut second_watch).await?;
    assert_snapshot_event(&first_snapshot, &[]);
    assert_snapshot_event(&second_snapshot, &[]);
    assert_eq!(first_snapshot.sequence, second_snapshot.sequence);

    let created = mutation_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let first_created = next_watch_message(&mut first_watch).await?;
    let second_created = next_watch_message(&mut second_watch).await?;
    assert_session_event(&first_created, &created.id);
    assert_session_event(&second_created, &created.id);
    assert_eq!(first_created.sequence, second_created.sequence);

    mutation_client
        .delete_session(DeleteSessionRequest {
            id: created.id.clone(),
        })
        .await?;

    let first_deleted = next_watch_message(&mut first_watch).await?;
    let second_deleted = next_watch_message(&mut second_watch).await?;
    assert_deleted_event(&first_deleted, &created.id);
    assert_deleted_event(&second_deleted, &created.id);
    assert_eq!(first_deleted.sequence, second_deleted.sequence);
    assert!(first_deleted.sequence > first_created.sequence);

    Ok(())
}

#[tokio::test]
async fn session_service_watch_initial_snapshot_includes_existing_sessions()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut mutation_client = session_client(server).await?;
    let mut watch_client = session_client(server).await?;
    let created = mutation_client
        .create_session(CreateSessionRequest::default())
        .await?
        .into_inner()
        .session
        .expect("created session should be present");

    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let snapshot = next_watch_message(&mut watch).await?;

    assert_snapshot_event(&snapshot, &[created.id.as_str()]);

    Ok(())
}

#[tokio::test]
async fn session_service_watch_emits_runtime_status_updates()
-> Result<(), Box<dyn std::error::Error>> {
    let (server, sessions) = setup_server_with_sessions();
    let mut watch_client = session_client(server).await?;
    let created = sessions.create().await?;
    let session_id = created.id().clone();
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let snapshot = next_watch_message(&mut watch).await?;
    assert_snapshot_event(&snapshot, &[session_id.as_str()]);

    sessions
        .append_timeline(
            &session_id,
            vec![PendingTimelineEvent::assistant_text_delta(
                session_id.clone(),
                0,
                PendingTimelineItemId::New,
                "hello",
            )],
        )
        .await?;

    let update = next_watch_message(&mut watch).await?;
    let watched = session_event(update, session_id.as_str());
    assert_eq!(
        watched.status.expect("status should be present").phase,
        SessionPhase::StreamingProvider as i32
    );

    Ok(())
}

#[tokio::test]
async fn session_service_watch_emits_one_cleared_event_for_wipe()
-> Result<(), Box<dyn std::error::Error>> {
    let server = setup_server();
    let mut watch_client = session_client(server).await?;
    let mut mutation_client = session_client(server).await?;
    mutation_client
        .create_session(CreateSessionRequest::default())
        .await?;
    let mut watch = watch_client
        .watch_sessions(WatchSessionsRequest::default())
        .await?
        .into_inner();
    let snapshot = next_watch_message(&mut watch).await?;
    assert!(matches!(
        snapshot.event,
        Some(watch_sessions_response::Event::Snapshot(_))
    ));

    mutation_client
        .wipe_sessions(WipeSessionsRequest { confirm: true })
        .await?;

    let cleared = next_watch_message(&mut watch).await?;
    assert!(cleared.sequence > snapshot.sequence);
    assert!(matches!(
        cleared.event,
        Some(watch_sessions_response::Event::Cleared(_))
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), watch.message())
            .await
            .is_err(),
        "wipe should publish exactly one cleared event"
    );
    Ok(())
}

async fn next_watch_message(
    watch: &mut tonic::Streaming<waltr::grpc::waltr::session::v1::WatchSessionsResponse>,
) -> Result<waltr::grpc::waltr::session::v1::WatchSessionsResponse, Box<dyn std::error::Error>> {
    Ok(
        tokio::time::timeout(std::time::Duration::from_secs(2), watch.message())
            .await??
            .expect("watch stream should emit session update"),
    )
}

fn assert_session_event(
    update: &waltr::grpc::waltr::session::v1::WatchSessionsResponse,
    expected_id: &str,
) {
    match update
        .event
        .clone()
        .expect("watched event should be present")
    {
        watch_sessions_response::Event::Session(session) => assert_eq!(session.id, expected_id),
        watch_sessions_response::Event::DeletedId(id) => {
            panic!("expected session event, got deleted id {id}")
        }
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected session event, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
        watch_sessions_response::Event::Cleared(_) => panic!("expected session event, got cleared"),
    }
}

fn session_event(
    update: waltr::grpc::waltr::session::v1::WatchSessionsResponse,
    expected_id: &str,
) -> waltr::grpc::waltr::session::v1::SessionSummary {
    match update.event.expect("watched event should be present") {
        watch_sessions_response::Event::Session(session) => {
            assert_eq!(session.id, expected_id);
            session
        }
        watch_sessions_response::Event::DeletedId(id) => {
            panic!("expected session event, got deleted id {id}")
        }
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected session event, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
        watch_sessions_response::Event::Cleared(_) => panic!("expected session event, got cleared"),
    }
}

fn assert_deleted_event(
    update: &waltr::grpc::waltr::session::v1::WatchSessionsResponse,
    expected_id: &str,
) {
    match update
        .event
        .clone()
        .expect("watched event should be present")
    {
        watch_sessions_response::Event::DeletedId(id) => assert_eq!(id, expected_id),
        watch_sessions_response::Event::Session(session) => {
            panic!("expected deleted id event, got session {}", session.id)
        }
        watch_sessions_response::Event::Snapshot(snapshot) => {
            panic!(
                "expected deleted id event, got snapshot with {} sessions",
                snapshot.sessions.len()
            )
        }
        watch_sessions_response::Event::Cleared(_) => {
            panic!("expected deleted id event, got cleared")
        }
    }
}

fn assert_snapshot_event(
    update: &waltr::grpc::waltr::session::v1::WatchSessionsResponse,
    expected_ids: &[&str],
) {
    match update
        .event
        .clone()
        .expect("watched event should be present")
    {
        watch_sessions_response::Event::Snapshot(snapshot) => {
            let mut ids = snapshot
                .sessions
                .into_iter()
                .map(|session| session.id)
                .collect::<Vec<_>>();
            ids.sort();
            let mut expected = expected_ids
                .iter()
                .map(|id| (*id).to_owned())
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(ids, expected);
        }
        watch_sessions_response::Event::Session(session) => {
            panic!("expected snapshot event, got session {}", session.id)
        }
        watch_sessions_response::Event::DeletedId(id) => {
            panic!("expected snapshot event, got deleted id {id}")
        }
        watch_sessions_response::Event::Cleared(_) => {
            panic!("expected snapshot event, got cleared")
        }
    }
}
