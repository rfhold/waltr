#![allow(clippy::future_not_send)]

use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime, SessionPhaseView};

use crate::support::{
    server::{server_endpoint, server_endpoint_with_runtime_config},
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn session_watch_interest_receives_initial_snapshot_and_created_sessions() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    next_snapshot_until(&mut stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;

    let watched = dispatch_stream_command(&mut stream, ClientCommand::RegisterSessionWatchInterest)
        .await
        .expect("session watch interest should register");
    let watched = if watched.sessions.watch.status == "received" {
        watched
    } else {
        next_snapshot_until(&mut stream, |snapshot| {
            snapshot.sessions.watch.status == "received"
        })
        .await
    };

    assert_eq!(watched.sessions.list.status, "idle");
    assert_eq!(watched.sessions.watch.status, "received");
    assert!(watched.sessions.current.is_empty());

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

#[tokio::test]
async fn two_session_watch_streams_stay_in_sync_across_create_and_delete() {
    let endpoint = server_endpoint();
    let mut first = ClientRuntime::for_endpoint(endpoint.clone()).into_state_stream(no_sleep);
    let mut second = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_system_ready(&mut first).await;
    wait_for_system_ready(&mut second).await;
    open_session_watch(&mut first).await;
    open_session_watch(&mut second).await;

    dispatch_stream_command(&mut first, ClientCommand::CreateSession)
        .await
        .expect("first tab should create session");
    let first_seen = next_snapshot_until(&mut first, |snapshot| {
        snapshot.sessions.watch.status == "received" && snapshot.sessions.current.len() == 1
    })
    .await;
    let created_id = first_seen.sessions.current[0].id.clone();

    let second_seen = next_snapshot_until(&mut second, |snapshot| {
        snapshot
            .sessions
            .current
            .iter()
            .any(|session| session.id == created_id)
    })
    .await;
    assert_eq!(second_seen.sessions.watch.error, None);

    dispatch_stream_command(
        &mut second,
        ClientCommand::DeleteSession {
            id: created_id.clone(),
        },
    )
    .await
    .expect("second tab should delete session");

    let second_empty = next_snapshot_until(&mut second, |snapshot| {
        snapshot.sessions.watch.status == "received" && snapshot.sessions.current.is_empty()
    })
    .await;
    assert_eq!(second_empty.sessions.watch.error, None);

    let first_empty = next_snapshot_until(&mut first, |snapshot| {
        snapshot.sessions.watch.status == "received" && snapshot.sessions.current.is_empty()
    })
    .await;
    assert_eq!(first_empty.sessions.watch.error, None);
}

#[tokio::test]
async fn session_runtime_update_preserves_reasoning_effort_in_client_state() {
    let endpoint = server_endpoint_with_runtime_config()
        .await
        .expect("runtime-configured server should start");
    let mut stream = ClientRuntime::for_endpoint(endpoint).into_state_stream(no_sleep);

    wait_for_system_ready(&mut stream).await;
    open_session_watch(&mut stream).await;

    dispatch_stream_command(&mut stream, ClientCommand::CreateSession)
        .await
        .expect("session create should succeed");
    let created = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.sessions.watch.status == "received" && snapshot.sessions.current.len() == 1
    })
    .await;
    let session_id = created.sessions.current[0].id.clone();

    let started = dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateSessionRuntimeConfig {
            id: session_id.clone(),
            provider_key: "openai".to_owned(),
            model_id: "gpt".to_owned(),
            enabled_mcp_server_keys: Vec::new(),
            reasoning_effort: Some("high".to_owned()),
        },
    )
    .await
    .expect("runtime update should succeed");
    let operation_key = started
        .sessions
        .runtime_config_update
        .key
        .clone()
        .expect("runtime update should expose operation key");

    let updated = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&operation_key),
            Some(ClientOperationView::SessionRuntimeConfigUpdate {
                status: "ready",
                ..
            })
        )
    })
    .await;
    let runtime = updated
        .sessions
        .current
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.runtime_config.as_ref())
        .expect("updated session should retain runtime config");
    let provider = runtime.provider.as_ref().expect("provider should be set");

    assert_eq!(provider.key, "openai");
    assert_eq!(provider.model_id, "gpt");
    assert_eq!(provider.reasoning_effort.as_deref(), Some("high"));
}

async fn wait_for_system_ready(stream: &mut waltr_client::ClientStateStream) {
    next_snapshot_until(stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;
}

async fn open_session_watch(stream: &mut waltr_client::ClientStateStream) {
    let opened = dispatch_stream_command(stream, ClientCommand::RegisterSessionWatchInterest)
        .await
        .expect("session watch interest should register");

    if opened.sessions.watch.status != "received" {
        next_snapshot_until(stream, |snapshot| {
            snapshot.sessions.watch.status == "received"
        })
        .await;
    }
}
