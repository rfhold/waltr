use waltr_client::{ClientCommand, ClientRuntime};

use crate::support::{
    server::server_endpoint,
    snapshots::{dispatch_stream_command, next_snapshot_until, no_sleep, provider_view},
};

#[tokio::test]
async fn shared_state_stream_keeps_watch_streams_open_until_final_release() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    let ready = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.status.ready && snapshot.status.watch.status == "received"
    })
    .await;
    assert_eq!(ready.config.watch.status, "idle");
    assert_eq!(ready.config.load.status, "idle");
    assert!(ready.config.current.is_none());

    dispatch_stream_command(&mut stream, ClientCommand::RegisterConfigWatchInterest)
        .await
        .expect("config watch interest should register");
    let opened = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.watch.status == "opened"
    })
    .await;
    assert_eq!(opened.config.watch.status, "opened");

    let loaded = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.current.is_some() && snapshot.config.watch.status == "received"
    })
    .await;
    assert!(loaded.config.current.is_some());

    let still_open =
        dispatch_stream_command(&mut stream, ClientCommand::RegisterConfigWatchInterest)
            .await
            .expect("second config watch interest should share stream");
    assert_eq!(still_open.config.watch.status, "received");

    dispatch_stream_command(
        &mut stream,
        ClientCommand::AddProvider {
            provider: provider_view("anthropic"),
        },
    )
    .await
    .expect("provider add should succeed");

    let received = next_snapshot_until(&mut stream, |snapshot| {
        snapshot.config.patch.status == "ready" && snapshot.config.watch.status == "received"
    })
    .await;
    assert_eq!(received.config.watch.error, None);

    let after_first_release =
        dispatch_stream_command(&mut stream, ClientCommand::ReleaseConfigWatchInterest)
            .await
            .expect("first config watch interest should release");
    assert_eq!(after_first_release.config.watch.status, "received");

    let after_final_release =
        dispatch_stream_command(&mut stream, ClientCommand::ReleaseConfigWatchInterest)
            .await
            .expect("final config watch interest should release");
    assert_eq!(after_final_release.config.watch.status, "idle");
}
