use std::time::Instant;

use waltr_client::{ClientOperationView, ClientRuntime};
use waltr_server::test_support::setup_server;
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{char_press, ctrl_x_press, delete_press, enter_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_sessions_screen,
    },
};

#[tokio::test]
async fn client_stream_creates_opens_and_deletes_session_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_sessions_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.sessions.watch.status == "received"
    })
    .await;

    let rendered = render_app(&app);
    assert!(rendered.contains("┌ Sessions"));
    assert!(rendered.contains("No sessions yet."));

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let create_action = app.handle_event(&char_press('s'), now);
    let created = dispatch_app_command(&mut stream, &mut app, create_action).await;
    let create_key = created
        .sessions
        .create
        .key
        .as_ref()
        .expect("create should expose operation key");
    let session_id = match created.operations.current.get(create_key) {
        Some(ClientOperationView::SessionCreate {
            status: "ready",
            session: Some(session),
            ..
        }) => session.id.clone(),
        operation => panic!("expected ready session create operation, saw {operation:?}"),
    };
    assert_eq!(
        app.sessions_section().detail_session_id(),
        Some(session_id.as_str())
    );

    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot
            .sessions
            .current
            .iter()
            .any(|session| session.id == session_id)
    })
    .await;
    assert!(render_app(&app).contains(&format!("Sessions / {session_id}")));

    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    let delete_action = app.handle_event(&enter_press(), now);
    let deleted = dispatch_app_command(&mut stream, &mut app, delete_action).await;
    let delete_key = deleted
        .sessions
        .delete
        .key
        .as_ref()
        .expect("delete should expose operation key");
    assert!(matches!(
        deleted.operations.current.get(delete_key),
        Some(ClientOperationView::SessionDelete {
            status: "ready",
            id,
            ..
        }) if id == &session_id
    ));
    assert_eq!(app.sessions_section().detail_session_id(), None);

    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot
            .sessions
            .current
            .iter()
            .all(|session| session.id != session_id)
    })
    .await;
}
