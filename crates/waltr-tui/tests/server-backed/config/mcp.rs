use std::time::Instant;

use waltr_client::{ClientRuntime, McpServerTypeView, ModelTypeView};
use waltr_server::test_support::setup_server;
use waltr_tui::{
    app::{App, AppAction},
    event::AppEvent,
    input::InputEvent,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::support::{
    input::{char_press, ctrl_x_press, delete_press, enter_press, right_press, tab_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_config_screen,
    },
};

#[tokio::test]
async fn client_stream_creates_edits_and_deletes_mcp_server_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_config_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "received" && snapshot.config.current.is_some()
    })
    .await;

    let now = Instant::now();

    // Switch to the MCP Servers tab and confirm its content renders.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("MCP Servers"));
    assert!(rendered.contains("No MCP servers configured."));

    // Create an sse server with a url.
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    for character in "remote".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    // name -> type, toggle to sse.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    // Move to the conditional url field and fill it.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    for character in "https://mcp.example".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    let action = app.handle_event(&enter_press(), now);
    let created = dispatch_app_command(&mut stream, &mut app, action).await;
    let server_view = created
        .config
        .current
        .as_ref()
        .and_then(|config| config.mcp_servers.get("remote"))
        .expect("created MCP server should be persisted");
    assert_eq!(server_view.kind, McpServerTypeView::Sse);
    assert_eq!(server_view.url, "https://mcp.example");

    // Edit the selected server (still on the MCP tab).
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let edited = dispatch_app_command(&mut stream, &mut app, action).await;
    assert!(
        edited
            .config
            .current
            .as_ref()
            .is_some_and(|config| config.mcp_servers.contains_key("remote")),
        "edited MCP server should remain persisted"
    );

    // Delete the selected server.
    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let deleted = dispatch_app_command(&mut stream, &mut app, action).await;
    assert!(
        deleted
            .config
            .current
            .as_ref()
            .is_some_and(|config| config.mcp_servers.is_empty()),
        "deleted MCP server should be removed from persisted config"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn client_stream_manages_provider_models_via_update_provider_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_config_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "received" && snapshot.config.current.is_some()
    })
    .await;

    let now = Instant::now();

    // Create a provider to host models.
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    for character in "anthropic".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    let action = app.handle_event(&enter_press(), now);
    assert!(
        matches!(action, AppAction::ClientCommand(_)),
        "expected provider create, saw {action:?}; rendered: {}",
        render_app(&app)
    );
    dispatch_app_command(&mut stream, &mut app, action).await;

    // Open the provider form and create a model in the provider draft.
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("Edit provider"));

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    assert!(render_app(&app).contains("Create model"));
    for character in "opus".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    // Saving without a type is blocked.
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("type required before saving"));

    // key -> name -> type, pick a type, then save through UpdateProvider.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);

    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    assert!(
        render_app(&app).contains("> opus"),
        "created model should be staged in the provider draft before dispatch"
    );

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert!(
        matches!(action, AppAction::ClientCommand(_)),
        "expected provider save after staged create, saw {action:?}; rendered: {}",
        render_app(&app)
    );
    let created = dispatch_app_command(&mut stream, &mut app, action).await;
    let provider = created
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("anthropic"))
        .expect("provider should be persisted");
    assert!(
        provider.models.contains_key("opus"),
        "created model should ride inside the provider"
    );

    // Reopen the provider form, edit the model type, and save one provider update.
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);

    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert!(
        matches!(action, AppAction::ClientCommand(_)),
        "expected provider save after staged edit, saw {action:?}; rendered: {}",
        render_app(&app)
    );
    let edited = dispatch_app_command(&mut stream, &mut app, action).await;
    let model = edited
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("anthropic"))
        .and_then(|provider| provider.models.get("opus"))
        .expect("edited model should remain persisted");
    assert_eq!(model.kind, ModelTypeView::Embedding);

    // Reopen the provider form, delete the model from the draft, then save.
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&backspace_press(), now),
        AppAction::Continue
    );

    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    assert!(render_app(&app).contains("No models configured."));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert!(
        matches!(action, AppAction::ClientCommand(_)),
        "expected provider save after staged delete, saw {action:?}; rendered: {}",
        render_app(&app)
    );
    let deleted = dispatch_app_command(&mut stream, &mut app, action).await;
    assert!(
        deleted
            .config
            .current
            .as_ref()
            .and_then(|config| config.providers.get("anthropic"))
            .is_some_and(|provider| provider.models.is_empty()),
        "deleted model should be removed by the provider update"
    );
}

const fn backspace_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Backspace,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}
