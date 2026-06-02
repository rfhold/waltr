use waltr_client::{ClientCommand, ClientRuntime};

use crate::support::{server::server_endpoint, snapshots::no_sleep};

#[tokio::test]
async fn runtime_dispatches_readiness_command() {
    let mut runtime = ClientRuntime::for_endpoint(server_endpoint());

    let ready = runtime
        .dispatch(ClientCommand::WaitForSystemReady, no_sleep)
        .await
        .expect("system should become ready");
    assert!(ready.status.ready);
    assert_eq!(ready.status.watch.status, "received");
}
