use waltr_client::{AccessStatus, ClientCommand, ClientRuntime};
use waltr_server::test_support::setup_server;
use waltr_tui::{
    app::{App, AppAction, AppScreen},
    event::{AppEvent, NativeLoginEvent},
};

use crate::support::server::{
    dispatch_client_command, drive_app_to_landing, next_snapshot_until, no_sleep,
};

#[tokio::test]
async fn auth_disabled_logout_reprobes_anonymously_and_returns_to_landing() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();
    drive_app_to_landing(&mut stream, &mut app).await;

    assert_eq!(
        app.handle_event(
            &AppEvent::NativeLogin(NativeLoginEvent::LoggedOut),
            std::time::Instant::now(),
        ),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Waiting);

    let checking = dispatch_client_command(&mut stream, ClientCommand::LogoutWaltr).await;
    assert_eq!(checking.access.status, AccessStatus::UnknownChecking);
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(checking)),
            std::time::Instant::now(),
        ),
        AppAction::Continue
    );

    let ready = next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.access.status == AccessStatus::Authenticated && snapshot.status.ready
    })
    .await;
    assert_eq!(ready.access.status, AccessStatus::Authenticated);
    assert_eq!(app.screen(), AppScreen::Landing);
}
