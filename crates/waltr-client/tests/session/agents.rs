use waltr_client::{ClientCommand, ClientOperationView, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn session_surface_discovers_selects_and_switches_agents() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);
    next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;

    dispatch_stream_command(&mut stream, ClientCommand::RegisterSessionWatchInterest)
        .await
        .expect("session interest should register");
    let catalog = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.sessions.agents.load.status == "ready"
    })
    .await;
    assert_eq!(catalog.sessions.agents.default_agent_name, "coding");
    assert_eq!(catalog.sessions.agents.entries[0].name, "coding");

    let created = dispatch_stream_command(
        &mut stream,
        ClientCommand::CreateSession {
            agent_name: "coding".to_owned(),
        },
    )
    .await
    .expect("selected session create should start");
    let create_key = created.sessions.create.key.expect("create operation key");
    let created = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&create_key),
            Some(ClientOperationView::SessionCreate {
                status: "ready",
                ..
            })
        ) && !snapshot.sessions.current.is_empty()
    })
    .await;
    let session_id = created.sessions.current[0].id.clone();
    assert_eq!(created.sessions.current[0].agent_name, "coding");

    let switching = dispatch_stream_command(
        &mut stream,
        ClientCommand::UpdateSessionAgent {
            id: session_id.clone(),
            agent_name: "coding".to_owned(),
        },
    )
    .await
    .expect("agent switch should start");
    let switch_key = switching
        .sessions
        .agent_update
        .key
        .expect("switch operation key");
    let switched = next_snapshot_until(&mut stream, |snapshot| {
        matches!(
            snapshot.operations.current.get(&switch_key),
            Some(ClientOperationView::SessionAgentUpdate {
                status: "ready",
                ..
            })
        )
    })
    .await;
    let Some(ClientOperationView::SessionAgentUpdate {
        session: Some(result),
        ..
    }) = switched.operations.current.get(&switch_key)
    else {
        panic!("switch result should expose the updated session");
    };
    assert_eq!(result.id, session_id);
    assert_eq!(result.agent_name, "coding");
}
