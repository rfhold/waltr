use std::time::Duration;

use futures_util::StreamExt;
use waltr_client::{
    Client, ClientEffect, ClientEvent, GrpcEffectExecutor, StatusEffect, StatusEvent,
};

use crate::support::server::server_endpoint;

#[tokio::test]
async fn status_watch_effect_updates_reducer_readiness() {
    let executor = GrpcEffectExecutor::for_endpoint(server_endpoint());
    let mut client = Client::new();

    let update = client.request_status_watch();
    assert_eq!(
        update.effects(),
        &[ClientEffect::Status(StatusEffect::Watch)]
    );

    let mut stream = executor
        .watch_status()
        .await
        .expect("status watch should start");
    client.apply(ClientEvent::Status(StatusEvent::WatchStarted));

    let status = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("status watch should emit readiness")
        .expect("status watch stream should remain open")
        .expect("status watch event should succeed");
    client.apply(ClientEvent::Status(StatusEvent::WatchChanged(status)));

    assert!(
        client
            .state()
            .status
            .current
            .is_some_and(|status| status.ready)
    );
}
