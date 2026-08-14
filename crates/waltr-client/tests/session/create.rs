use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime, SessionPhaseView};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn client_surface_creates_session_and_updates_snapshot() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;

    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::CreateSession {
            agent_name: "coding".to_owned(),
        },
    )
    .await
    .expect("session create should succeed");
    assert_eq!(started.sessions.create.status, "loading");
    let operation_key = started
        .sessions
        .create
        .key
        .clone()
        .expect("session create should expose operation key");
    assert!(matches!(
        started.operations.current.get(&operation_key),
        Some(ClientOperationView::SessionCreate {
            status: "loading",
            ..
        })
    ));

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::SessionCreate {
                status: "ready",
                ..
            })
        )
    })
    .await;

    assert_eq!(snapshot.sessions.create.status, "ready");
    assert_eq!(snapshot.sessions.create.error, None);
    assert_eq!(
        snapshot.sessions.create.key.as_deref(),
        Some(operation_key.as_str())
    );
    let Some(ClientOperationView::SessionCreate {
        session: Some(operation_session),
        ..
    }) = snapshot.operations.current.get(&operation_key)
    else {
        panic!("session create operation should include created session result");
    };
    assert!(!operation_session.id.is_empty());
    assert_eq!(operation_session.agent_name, "coding");
    assert_eq!(operation_session.revision, 0);
    assert_eq!(operation_session.status.phase, SessionPhaseView::Idle);
    assert_eq!(operation_session.current_prompt, None);
    assert!(operation_session.created_at.is_some());
    assert!(operation_session.updated_at.is_some());
    assert!(operation_session.last_activity_at.is_some());
    assert!(snapshot.sessions.current.is_empty());
}
