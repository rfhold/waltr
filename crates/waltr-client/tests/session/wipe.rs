use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn confirmed_wipe_completes_by_key_and_remote_watch_remains_interested() {
    let endpoint = server_endpoint();
    let mut initiating = ClientRuntime::for_endpoint(endpoint.clone()).into_state_stream(no_sleep);
    let mut remote = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_ready(&mut initiating).await;
    wait_for_ready(&mut remote).await;
    open_session_watch(&mut initiating).await;
    open_session_watch(&mut remote).await;

    dispatch_stream_command(&mut initiating, ClientCommand::CreateSession)
        .await
        .expect("first session create should succeed");
    dispatch_stream_command(&mut initiating, ClientCommand::CreateSession)
        .await
        .expect("second session create should succeed");
    next_snapshot_until(&mut initiating, |snapshot| {
        snapshot.sessions.current.len() == 2
    })
    .await;
    next_snapshot_until(&mut remote, |snapshot| snapshot.sessions.current.len() == 2).await;

    let started = dispatch_stream_command(&mut initiating, ClientCommand::WipeSessions)
        .await
        .expect("confirmed wipe request should succeed");
    assert_eq!(started.sessions.wipe.status, "loading");
    let operation_key = started
        .sessions
        .wipe
        .key
        .clone()
        .expect("wipe should expose an operation key");
    assert!(matches!(
        started.operations.current.get(&operation_key),
        Some(ClientOperationView::SessionWipe {
            status: "loading",
            error: None,
        })
    ));

    let completed = next_snapshot_until(&mut initiating, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::SessionWipe {
                status: "ready",
                ..
            })
        )
    })
    .await;
    assert!(completed.sessions.current.is_empty());
    assert_eq!(completed.sessions.wipe.status, "ready");

    let remotely_cleared =
        next_snapshot_until(&mut remote, |snapshot| snapshot.sessions.current.is_empty()).await;
    assert_eq!(remotely_cleared.sessions.watch.status, "received");
    assert_eq!(remotely_cleared.sessions.wipe.status, "idle");

    dispatch_stream_command(&mut initiating, ClientCommand::CreateSession)
        .await
        .expect("session create after wipe should succeed");
    next_snapshot_until(&mut initiating, |snapshot| {
        snapshot.sessions.current.len() == 1
    })
    .await;
    let remote_after_create =
        next_snapshot_until(&mut remote, |snapshot| snapshot.sessions.current.len() == 1).await;
    assert_eq!(remote_after_create.sessions.watch.status, "received");
}

async fn wait_for_ready(stream: &mut waltr_client::ClientStateStream) {
    next_snapshot_until(stream, |snapshot| snapshot.status.ready).await;
}

async fn open_session_watch(stream: &mut waltr_client::ClientStateStream) {
    dispatch_stream_command(stream, ClientCommand::RegisterSessionWatchInterest)
        .await
        .expect("session watch interest should register");
    next_snapshot_until(stream, |snapshot| {
        snapshot.sessions.watch.status == "received"
    })
    .await;
}
