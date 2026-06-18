use std::time::Duration;

use waltr_client::ClientRuntime;
use waltr_server::test_support::setup_server_at;
use waltr_tui::app::App;

use crate::support::server::{drive_app_to_landing, no_sleep, unused_addr};

#[tokio::test]
async fn client_stream_retries_until_delayed_real_server_is_ready() {
    let addr = unused_addr();
    let mut stream =
        ClientRuntime::for_endpoint(format!("http://{addr}")).into_state_stream(no_sleep);
    let mut app = App::new();

    let loading = stream.next().await;
    assert_eq!(loading.status.watch.status, "loading");

    let failed = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("client should surface the initial connection failure");
    assert_eq!(failed.status.watch.status, "failed");

    let server = setup_server_at(addr);

    drive_app_to_landing(&mut stream, &mut app).await;

    assert_eq!(server.base_url(), format!("http://{addr}"));
}
