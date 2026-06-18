use std::time::Instant;

use waltr_client::{ClientRuntime, ProviderKindView};
use waltr_server::test_support::setup_server;
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{char_press, ctrl_x_press, delete_press, enter_press, right_press, tab_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_config_screen,
    },
};

#[tokio::test]
async fn client_stream_creates_edits_and_deletes_provider_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_config_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "received" && snapshot.config.current.is_some()
    })
    .await;

    // The config surface presents a tab strip with the "Providers" tab active
    // and shows the providers section content for that tab.
    let rendered = render_app(&app);
    assert!(rendered.contains("┌ Configuration"));
    assert!(rendered.contains("Providers"));
    assert!(rendered.contains("MCP Servers"));
    assert!(rendered.contains("Tab switch"));
    assert!(rendered.contains("No providers configured."));

    let now = Instant::now();
    // Switching tabs moves to the MCP section, then back to providers.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let switched = render_app(&app);
    assert!(switched.contains("No MCP servers configured."));
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let switched_back = render_app(&app);
    assert!(switched_back.contains("No providers configured."));

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    for character in "openai".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    for character in "https://api.openai.com".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    let action = app.handle_event(&enter_press(), now);
    let created = dispatch_app_command(&mut stream, &mut app, action).await;
    let provider = created
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("openai"))
        .expect("created provider should be persisted");
    assert_eq!(provider.kind, ProviderKindView::OpenAi);
    assert_eq!(provider.url, "https://api.openai.com");

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);

    let action = app.handle_event(&enter_press(), now);
    let edited = dispatch_app_command(&mut stream, &mut app, action).await;
    let provider = edited
        .config
        .current
        .as_ref()
        .and_then(|config| config.providers.get("openai"))
        .expect("edited provider should remain persisted");
    assert_eq!(provider.kind, ProviderKindView::Anthropic);

    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let deleted = dispatch_app_command(&mut stream, &mut app, action).await;
    assert!(
        deleted
            .config
            .current
            .as_ref()
            .is_some_and(|config| config.providers.is_empty()),
        "deleted provider should be removed from persisted config"
    );
}
