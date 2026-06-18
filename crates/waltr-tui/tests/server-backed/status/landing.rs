use waltr_client::ClientRuntime;
use waltr_server::test_support::setup_server;
use waltr_tui::app::App;

use crate::support::server::{drive_app_to_landing, no_sleep};

#[tokio::test]
async fn client_stream_reaches_landing_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();
    drive_app_to_landing(&mut stream, &mut app).await;
}
