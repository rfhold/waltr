use std::time::Instant;

use waltr_client::{ClientCommand, ClientRuntime};
use waltr_server::test_support::setup_server;
use waltr_tui::app::{App, AppAction, AppScreen};

use crate::support::{
    input::{char_press, ctrl_x_press},
    render::render_app,
    server::drive_app_to_landing,
};

#[tokio::test]
async fn opening_config_before_watch_data_renders_waiting_state() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream();
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    assert!(
        stream.snapshot().config.current.is_none(),
        "startup should not load config before watch interest"
    );

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let AppAction::ClientCommand(register) = app.handle_event(&char_press('c'), now) else {
        panic!("config entry should register config watch interest");
    };
    assert_eq!(register, ClientCommand::RegisterConfigWatchInterest);
    assert_eq!(app.screen(), AppScreen::Config);

    let rendered = render_app(&app);
    assert!(rendered.contains("┌ Configuration"));
    assert!(!rendered.contains("Waltr / Configuration"));
    // Before any watch data arrives the surface shows the intermediary loading
    // indicator with the current watch status in place of section content.
    assert!(rendered.contains("Loading configuration..."));
    assert!(!rendered.contains("Waiting for configuration..."));
    assert!(!rendered.contains("load: ready"));
    assert!(!rendered.contains("Read-only settings synced from the local runtime."));
}
