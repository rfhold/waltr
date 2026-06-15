use std::time::Instant;

use waltr_client::{ClientCommand, ClientRuntime};
use waltr_server::test_support::setup_server;
use waltr_tui::app::{App, AppAction, AppScreen};

use crate::support::{
    input::{char_press, ctrl_x_press, escape_press},
    render::render_app,
    server::{drive_app_to_landing, next_snapshot_until, no_sleep, provider_view},
};

#[tokio::test]
async fn client_stream_renders_config_status_with_real_server() {
    let server = setup_server();
    let mut seeder = ClientRuntime::for_endpoint(server.base_url());
    seeder
        .dispatch(ClientCommand::RequestConfig, no_sleep)
        .await
        .expect("seed config load should succeed");
    seeder
        .dispatch(
            ClientCommand::AddProvider {
                provider: provider_view("anthropic"),
            },
            no_sleep,
        )
        .await
        .expect("seed provider add should succeed");

    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream();
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let AppAction::ClientCommand(register) = app.handle_event(&char_press('c'), now) else {
        panic!("config entry should register config watch interest");
    };
    assert_eq!(register, ClientCommand::RegisterConfigWatchInterest);
    assert_eq!(app.screen(), AppScreen::Config);

    let opened = stream
        .dispatch(register, no_sleep)
        .await
        .expect("config watch interest should register");
    assert_eq!(opened.config.watch.status, "opened");
    assert_eq!(
        app.handle_event(
            &waltr_tui::event::AppEvent::Client(Box::new(opened)),
            Instant::now()
        ),
        AppAction::Continue
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("┌ Configuration"));
    assert!(!rendered.contains("Waltr / Configuration"));
    // The opened-but-no-data state is pending: the surface shows the loading
    // indicator carrying the current watch status in place of section content.
    assert!(rendered.contains("Loading configuration..."));
    assert!(rendered.contains("watch opened"));
    assert!(!rendered.contains("Waiting for configuration..."));
    assert!(!rendered.contains("> anthropic"));
    assert!(!rendered.contains("load: ready"));
    assert!(!rendered.contains("Read-only settings synced from the local runtime."));

    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "received" && snapshot.config.current.is_some()
    })
    .await;
    let rendered = render_app(&app);
    // Once config is loaded the section content fills the area and the
    // standalone watch footer is gone.
    assert!(!rendered.contains("watch received"));
    assert!(rendered.contains("> anthropic"));
    assert!(!rendered.contains("load: ready"));

    let AppAction::ClientCommand(release) = app.handle_event(&escape_press(), Instant::now())
    else {
        panic!("config exit should release config watch interest");
    };
    assert_eq!(release, ClientCommand::ReleaseConfigWatchInterest);

    let released = stream
        .dispatch(release, no_sleep)
        .await
        .expect("config watch interest should release");
    assert_eq!(released.config.watch.status, "idle");
    assert_eq!(app.screen(), AppScreen::Landing);
}
