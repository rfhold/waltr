use waltr_client::{ClientCommand, ClientRuntime, SessionPhaseView};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn session_watch_interest_lists_and_receives_created_sessions() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    next_snapshot_until(&mut stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;

    let opened = dispatch_stream_command(&mut stream, ClientCommand::RegisterSessionWatchInterest)
        .await
        .expect("session watch interest should register");
    let opened = if opened.sessions.watch.status == "opened" {
        opened
    } else {
        next_snapshot_until(&mut stream, |snapshot| {
            snapshot.sessions.watch.status == "opened"
        })
        .await
    };

    assert_eq!(opened.sessions.list.status, "ready");
    assert_eq!(opened.sessions.watch.status, "opened");
    assert!(opened.sessions.current.is_empty());

    dispatch_stream_command(&mut stream, ClientCommand::CreateSession)
        .await
        .expect("session create should succeed");

    let watched = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.sessions.watch.status == "received" && snapshot.sessions.current.len() == 1
    })
    .await;

    assert_eq!(watched.sessions.watch.error, None);
    assert_eq!(
        watched.sessions.current[0].status.phase,
        SessionPhaseView::Idle
    );
    let id = watched.sessions.current[0].id.clone();

    dispatch_stream_command(&mut stream, ClientCommand::DeleteSession { id })
        .await
        .expect("session delete should succeed");

    let watched = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.sessions.watch.status == "received" && snapshot.sessions.current.is_empty()
    })
    .await;
    assert_eq!(watched.sessions.watch.error, None);

    let released = dispatch_stream_command(&mut stream, ClientCommand::ReleaseSessionWatchInterest)
        .await
        .expect("session watch interest should release");
    assert_eq!(released.sessions.watch.status, "idle");
}
