use waltr_client::{ClientCommand, ClientRuntime, SessionPhaseView};

use crate::support::{
    server::server_endpoint,
    snapshots::{next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn client_surface_creates_session_and_updates_snapshot() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream();

    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;

    let snapshot = stream
        .dispatch(ClientCommand::CreateSession, no_sleep)
        .await
        .expect("session create should succeed");

    assert_eq!(snapshot.sessions.create.status, "ready");
    assert_eq!(snapshot.sessions.create.error, None);
    assert_eq!(snapshot.sessions.current.len(), 1);

    let session = &snapshot.sessions.current[0];
    assert!(!session.id.is_empty());
    assert_eq!(session.revision, 0);
    assert_eq!(session.status.phase, SessionPhaseView::Idle);
    assert_eq!(session.current_prompt, None);
    assert!(session.created_at.is_some());
    assert!(session.updated_at.is_some());
    assert!(session.last_activity_at.is_some());
}
