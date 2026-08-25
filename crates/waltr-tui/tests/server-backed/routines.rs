use std::time::Instant;

use waltr_client::{ClientCommand, ClientRuntime};
use waltr_server::test_support::setup_server;
use waltr_tui::app::{App, AppAction, AppScreen};

use crate::support::{
    input::{char_press, ctrl_x_press},
    render::render_app,
    server::{dispatch_client_command, drive_app_to_landing, next_snapshot_until, no_sleep},
};

#[tokio::test]
async fn routine_screen_receives_real_definition_and_history_watches() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();
    drive_app_to_landing(&mut stream, &mut app).await;

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let AppAction::ClientCommands(commands) = app.handle_event(&char_press('u'), now) else {
        panic!("Routine entry should register both shared watches");
    };
    assert_eq!(
        commands,
        vec![
            ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ClientCommand::RegisterRoutineInvocationWatchInterest,
        ]
    );
    let mut latest = None;
    for command in commands {
        let snapshot = dispatch_client_command(&mut stream, command).await;
        drop(app.handle_event(
            &waltr_tui::event::AppEvent::Client(Box::new(snapshot.clone())),
            now,
        ));
        latest = Some(snapshot);
    }
    let mut snapshot = latest.expect("watch registration should yield snapshots");
    if snapshot.routines.definitions.watch.status != "received"
        || snapshot.routines.history.watch.status != "received"
    {
        snapshot = next_snapshot_until(&mut stream, &mut app, |snapshot| {
            snapshot.routines.definitions.watch.status == "received"
                && snapshot.routines.history.watch.status == "received"
        })
        .await;
    }

    assert_eq!(app.screen(), AppScreen::Routines);
    assert!(!snapshot.routines.definitions.definitions.is_empty());
    let rendered = render_app(&app);
    assert!(rendered.contains("Routines"), "{rendered}");
    assert!(rendered.contains("Fork session"), "{rendered}");
    assert!(rendered.contains("No Routine invocations"), "{rendered}");
}
