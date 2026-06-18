use waltr_client::{ClientCommand, ClientRuntime, SessionPhaseView};

use crate::support::{
    server::server_endpoint,
    snapshots::{next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn session_watch_interest_lists_and_receives_created_sessions() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream();

    next_snapshot_until(&mut stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;

    let opened = stream
        .dispatch(ClientCommand::RegisterSessionWatchInterest, no_sleep)
        .await
        .expect("session watch interest should register");

    assert_eq!(opened.sessions.list.status, "ready");
    assert_eq!(opened.sessions.watch.status, "opened");
    assert!(opened.sessions.current.is_empty());

    stream
        .dispatch(ClientCommand::CreateSession, no_sleep)
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

    let released = stream
        .dispatch(ClientCommand::ReleaseSessionWatchInterest, no_sleep)
        .await
        .expect("session watch interest should release");
    assert_eq!(released.sessions.watch.status, "idle");
}
