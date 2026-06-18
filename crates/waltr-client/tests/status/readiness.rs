use waltr_client::ClientRuntime;

use crate::support::{
    server::server_endpoint,
    snapshots::{next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn shared_state_stream_reaches_readiness() {
    let mut stream = ClientRuntime::for_endpoint(server_endpoint()).into_state_stream(no_sleep);

    let ready = next_snapshot_until(&mut stream, |snapshot| snapshot.status.ready).await;
    assert!(ready.status.ready);
    assert_eq!(ready.status.watch.status, "received");
}
