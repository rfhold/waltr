use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn client_surface_deletes_session_and_updates_snapshot() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;

    dispatch_stream_command(&mut stream, ClientCommand::CreateSession)
        .await
        .expect("session create should succeed");
    let created = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.sessions.current.len() == 1 && snapshot.sessions.create.status == "ready"
    })
    .await;
    let id = created.sessions.current[0].id.clone();

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
        )
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
