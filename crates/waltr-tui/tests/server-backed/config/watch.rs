use std::time::Instant;

use waltr_client::{ClientCommand, ClientRuntime};
use waltr_server::test_support::setup_server;
use waltr_tui::app::{App, AppAction, AppScreen};

use crate::support::{
    input::{char_press, ctrl_x_press, escape_press},
    render::render_app,
    server::{
        dispatch_client_command, drive_app_to_landing, next_snapshot_until, no_sleep, provider_view,
    },
};

#[tokio::test]
async fn client_stream_renders_config_status_with_real_server() {
    let server = setup_server();
    let mut seeder = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    dispatch_client_command(&mut seeder, ClientCommand::RequestConfig).await;
    next_snapshot_until(&mut seeder, &mut App::new(), |snapshot| {
        snapshot.config.current.is_some()
    })
    .await;
    dispatch_client_command(
        &mut seeder,
        ClientCommand::AddProvider {
            provider: provider_view("custom-anthropic"),
        },
    )
    .await;
    next_snapshot_until(&mut seeder, &mut App::new(), |snapshot| {
        snapshot.config.patch.status == "ready"
    })
    .await;

    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let AppAction::ClientCommand(register) = app.handle_event(&char_press('c'), now) else {
        panic!("config entry should register config watch interest");
    };
    assert_eq!(register, ClientCommand::RegisterConfigWatchInterest);
    assert_eq!(app.screen(), AppScreen::Config);

    let opening = dispatch_client_command(&mut stream, register).await;
    assert_eq!(
        app.handle_event(
            &waltr_tui::event::AppEvent::Client(Box::new(opening)),
            Instant::now()
        ),
        AppAction::Continue
    );
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "opened"
    })
    .await;

    let rendered = render_app(&app);
    assert!(rendered.contains("Configuration"));
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
    assert!(rendered.contains("custom-anthropic"));
    assert!(!rendered.contains("load: ready"));

    let AppAction::ClientCommand(release) = app.handle_event(&escape_press(), Instant::now())
    else {
        panic!("config exit should release config watch interest");
    };
    assert_eq!(release, ClientCommand::ReleaseConfigWatchInterest);

    let released = dispatch_client_command(&mut stream, release).await;
    assert_eq!(released.config.watch.status, "idle");
    assert_eq!(app.screen(), AppScreen::Landing);
}
