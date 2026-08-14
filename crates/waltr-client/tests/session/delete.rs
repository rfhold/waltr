use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the test covers one complete create-delete-watch interaction"
)]
async fn client_surface_deletes_session_and_updates_snapshot() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;
    dispatch_stream_command(&mut stream, ClientCommand::RegisterSessionWatchInterest)
        .await
        .expect("session watch interest should register");

    let create_started = dispatch_stream_command(
        &mut stream,
        ClientCommand::CreateSession {
            agent_name: "coding".to_owned(),
        },
    )
    .await
    .expect("session create should succeed");
    let create_key = create_started
        .sessions
        .create
        .key
        .clone()
        .expect("session create should expose operation key");
    let created = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&create_key),
            Some(ClientOperationView::SessionCreate {
                status: "ready",
                ..
            })
        ) && snapshot.sessions.current.len() == 1
    })
    .await;
    let Some(ClientOperationView::SessionCreate {
        session: Some(session),
        ..
    }) = created.operations.current.get(&create_key)
    else {
        panic!("session create operation should include created session result");
    };
    let id = session.id.clone();
    assert_eq!(created.sessions.current.len(), 1);
    assert_eq!(created.sessions.current[0].id, id);

    let started =
        dispatch_stream_command(&mut stream, ClientCommand::DeleteSession { id: id.clone() })
            .await
            .expect("session delete should succeed");
    assert_eq!(started.sessions.delete.status, "loading");
    let operation_key = started
        .sessions
        .delete
        .key
        .clone()
        .expect("session delete should expose operation key");
    assert!(matches!(
        started.operations.current.get(&operation_key),
        Some(ClientOperationView::SessionDelete {
            status: "loading",
            id: operation_id,
            ..
        }) if operation_id == &id
    ));

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::SessionDelete {
                status: "ready",
                ..
            })
        ) && snapshot.sessions.current.is_empty()
    })
    .await;

    assert_eq!(snapshot.sessions.delete.status, "ready");
    assert_eq!(snapshot.sessions.delete.error, None);
    assert_eq!(
        snapshot.sessions.delete.key.as_deref(),
        Some(operation_key.as_str())
    );
    assert!(snapshot.sessions.current.is_empty());

    let Some(ClientOperationView::SessionDelete {
        status: "ready",
        error: None,
        id: operation_id,
    }) = snapshot.operations.current.get(&operation_key)
    else {
        panic!("session delete operation should include deleted id result");
    };
    assert_eq!(operation_id, &id);
}

#[tokio::test]
async fn client_surface_treats_missing_session_delete_as_success() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;

    let id = "missing-session".to_owned();
    let started =
        dispatch_stream_command(&mut stream, ClientCommand::DeleteSession { id: id.clone() })
            .await
            .expect("idempotent session delete should succeed");
    let operation_key = started
        .sessions
        .delete
        .key
        .clone()
        .expect("session delete should expose operation key");

    let snapshot = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::SessionDelete {
                status: "ready",
                ..
            })
        )
    })
    .await;

    assert!(snapshot.sessions.current.is_empty());
    assert!(matches!(
        snapshot.operations.current.get(&operation_key),
        Some(ClientOperationView::SessionDelete {
            status: "ready",
            error: None,
            id: operation_id,
        }) if operation_id == &id
    ));
}
