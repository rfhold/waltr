//! App-level rendering and input tests.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use waltr_client::{
    ClientCommand, ClientOperationView, ClientSnapshot, ConfigSnapshot, ConfigView,
    McpServerTypeView, McpServerView, ModelCapabilitiesView, ModelLimitsView, ModelOptionsView,
    ModelTypeView, ModelView, OperationCollectionSnapshot, OperationSnapshot, ProviderKindView,
    ProviderView, SessionPhaseView, SessionSnapshot, SessionStatusView, SessionView,
    StatusSnapshot, TimestampView,
};
use waltr_tui::{
    app::{App, AppAction, AppScreen},
    event::AppEvent,
    input::InputEvent,
    screens::ConfigTab,
    theme::{StatusRole, THEME},
    ui,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::Rect,
    style::{Color, Modifier, Style},
};

#[test]
fn app_launches_and_stabilizes_to_snapshot() {
    let app = App::new();

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn ctrl_c_first_press_arms_exit_notice() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Input(InputEvent::CtrlC), now),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn escape_first_press_arms_exit_notice() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn ctrl_c_second_press_exits_while_armed() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Input(InputEvent::CtrlC), now),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Input(InputEvent::CtrlC),
            now + Duration::from_millis(500),
        ),
        AppAction::Exit,
    );
}

#[test]
fn escape_second_press_exits_while_armed() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&escape_press(), now + Duration::from_millis(500)),
        AppAction::Exit,
    );
}

#[test]
fn close_notice_disarms_after_timeout() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&AppEvent::Tick(now + Duration::from_millis(1_500)), now),
        AppAction::Continue,
    );

    assert_eq!(app.notice(), None);
    assert_eq!(
        app.handle_event(&escape_press(), now + Duration::from_millis(1_501)),
        AppAction::Continue,
    );
}

#[test]
fn leader_q_exits_immediately() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('q'), now), AppAction::Exit);
}

#[test]
fn ready_client_snapshot_opens_landing() {
    let mut app = App::new();

    assert_eq!(app.screen(), AppScreen::Waiting);
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(ready_snapshot())),
            Instant::now()
        ),
        AppAction::Continue
    );

    assert_eq!(app.screen(), AppScreen::Landing);
}

#[test]
fn ready_landing_renders_app_shell_and_palette() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn theme_preserves_existing_neon_lavender_accent_identity() {
    assert_eq!(THEME.accent(), Color::Rgb(190, 132, 242));
    assert_eq!(THEME.accent_style(), Style::default().fg(THEME.accent()));
    assert_eq!(
        THEME.accent_bold(),
        Style::default()
            .fg(Color::Rgb(190, 132, 242))
            .add_modifier(Modifier::BOLD)
    );
}

#[test]
fn theme_exposes_semantic_surface_text_and_state_roles() {
    assert_eq!(
        THEME.dim_accent_style(),
        Style::default().fg(Color::Rgb(72, 58, 92))
    );
    assert_eq!(
        THEME.text_style(),
        Style::default().fg(Color::Rgb(246, 241, 255))
    );
    assert_eq!(
        THEME.muted_text_style(),
        Style::default().fg(Color::DarkGray)
    );
    assert_eq!(THEME.success_style(), Style::default().fg(Color::Green));
    assert_eq!(THEME.danger_style(), Style::default().fg(Color::Red));
    assert_eq!(
        THEME.surface_style(),
        Style::default().bg(Color::Black).not_dim()
    );
    assert_eq!(THEME.focus_style(), Style::default().fg(Color::Cyan));
}

#[test]
fn theme_maps_status_states_to_named_roles() {
    assert_eq!(
        THEME.status_style(StatusRole::Loading),
        THEME.accent_style()
    );
    assert_eq!(THEME.status_style(StatusRole::Ready), THEME.success_style());
    assert_eq!(THEME.status_style(StatusRole::Error), THEME.danger_style());
    assert_eq!(THEME.status_style(StatusRole::Saving), THEME.focus_style());
    assert_eq!(
        THEME.status_style(StatusRole::Deleting),
        THEME.danger_style()
    );
}

#[test]
fn ui_primitives_center_areas_and_share_prompt_text() {
    assert_eq!(
        ui::centered_rect(Rect::new(0, 0, 80, 24), 40, 10),
        Rect::new(20, 7, 40, 10)
    );
    assert_eq!(
        ui::centered_percent_rect(Rect::new(0, 0, 80, 24), 50, 10),
        Rect::new(20, 7, 40, 10)
    );

    assert_eq!(
        ui::delete_prompt_text(false, None),
        "Delete? Enter/y confirm, n/Esc cancel"
    );
    assert_eq!(ui::delete_prompt_text(true, None), "Deleting...");
    assert_eq!(
        ui::delete_prompt_text(false, Some("failed")),
        "error: failed"
    );
}

#[test]
fn ui_primitives_preserve_shared_line_styles() {
    let loading = ui::loading_line("Waiting for configuration...");
    assert_eq!(loading.spans[0].style, THEME.muted_text_style());

    let field = ui::form_field_row(1, 1, "name", "anthropic");
    assert_eq!(field.spans[0].content.as_ref(), "*");
    assert_eq!(field.spans[0].style, THEME.accent_style());
    assert_eq!(field.spans[2].style, THEME.muted_text_style());
    assert_eq!(field.spans[4].style, THEME.text_style());

    let inactive = ui::selection_marker(false);
    assert_eq!(inactive.content.as_ref(), " ");
    assert_eq!(inactive.style, THEME.accent_style());
}

#[test]
fn tui_manifest_keeps_existing_terminal_stack() {
    let manifest = include_str!("../Cargo.toml");

    assert!(manifest.contains("ratatui = { workspace = true"));
    assert!(manifest.contains("tokio = { workspace = true"));

    for framework in ["cursive", "ratatui-explorer", "tui-input", "tui-realm"] {
        assert!(!manifest.contains(framework));
    }
}

#[test]
fn leader_c_opens_config_after_ready() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    assert_eq!(app.screen(), AppScreen::Config);
}

#[test]
fn leader_s_opens_sessions_after_ready() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::RegisterSessionWatchInterest)
    );

    assert_eq!(app.screen(), AppScreen::Sessions);
    let rendered = render_app(&app);
    assert!(rendered.contains("┌ Sessions"));
    assert!(rendered.contains("Loading sessions"));
}

#[test]
fn sessions_escape_releases_watch_interest() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();

    assert_eq!(app.screen(), AppScreen::Sessions);
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseSessionWatchInterest)
    );
    assert_eq!(app.screen(), AppScreen::Landing);
}

#[test]
fn sessions_create_dispatches_and_completion_opens_detail() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(Vec::new()));
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::CreateSession)
    );

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_create_snapshot(
                "op-create",
                "loading",
                None,
            ))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_create_snapshot(
                "op-create",
                "ready",
                Some(session_view("created", 2)),
            ))),
            now,
        ),
        AppAction::Continue
    );

    assert_eq!(app.sessions_section().detail_session_id(), Some("created"));
    let rendered = render_app(&app);
    assert!(
        rendered.contains("status idle  provider none (p)  mcp none (m)  delete Backspace/Delete")
    );
}

#[test]
fn sessions_detail_delete_dispatches_and_returns_to_list() {
    let mut app =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("doomed", 1)]));
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.sessions_section().detail_session_id(), Some("doomed"));
    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::DeleteSession {
            id: "doomed".to_owned(),
        })
    );

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_delete_snapshot(
                "op-delete",
                "loading",
                "doomed",
            ))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_delete_snapshot(
                "op-delete",
                "ready",
                "doomed",
            ))),
            now,
        ),
        AppAction::Continue
    );

    assert_eq!(app.sessions_section().detail_session_id(), None);
    assert!(render_app(&app).contains("Recent sessions"));
}

#[test]
fn sessions_loading_renders_snapshot() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::RegisterSessionWatchInterest)
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn sessions_delete_confirmation_renders_snapshot() {
    let mut app =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("doomed", 1)]));

    assert_eq!(
        app.handle_event(&delete_press(), Instant::now()),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn sessions_create_pending_renders_snapshot() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(Vec::new()));
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::CreateSession)
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn sessions_create_error_renders_snapshot() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(Vec::new()));
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::CreateSession)
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_create_snapshot(
                "op-create",
                "loading",
                None,
            ))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_create_snapshot_with_error(
                "op-create",
                "failed",
                "create failed",
            ))),
            now,
        ),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn sessions_delete_pending_renders_snapshot() {
    let mut app =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("doomed", 1)]));
    let now = Instant::now();

    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::DeleteSession {
            id: "doomed".to_owned(),
        })
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn sessions_delete_error_renders_snapshot() {
    let mut app =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("doomed", 1)]));
    let now = Instant::now();

    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::DeleteSession {
            id: "doomed".to_owned(),
        })
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_delete_snapshot(
                "op-delete",
                "loading",
                "doomed",
            ))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_delete_snapshot_with_error(
                "op-delete",
                "failed",
                "doomed",
                "delete failed",
            ))),
            now,
        ),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn config_renders_watched_snapshot_as_modal_with_escape_hint() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(watched_config_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("Waltr"));
    assert!(rendered.contains("┌ Configuration"));
    assert!(rendered.contains("Providers"));
    assert!(!rendered.contains("watch received"));
    assert!(rendered.contains("> anthropic"));
    assert!(rendered.contains("C-x c create"));
    assert!(rendered.contains("Esc back"));
    assert!(!rendered.contains("open configuration"));
    assert!(!rendered.contains("etag"));
    assert!(!rendered.contains("etag-1"));
    assert!(!rendered.contains("Waltr / Configuration"));
    assert!(!rendered.contains("load: ready"));
    assert!(!rendered.contains("Read-only settings synced from the local runtime."));
}

#[test]
fn config_provider_create_form_renders_snapshot() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn config_mcp_create_form_renders_snapshot() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn config_provider_delete_confirmation_renders_snapshot() {
    let mut app = open_config_app();

    assert_eq!(
        app.handle_event(&delete_press(), Instant::now()),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn config_provider_patch_error_renders_snapshot() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateProvider { .. })
    ));
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot("op-1", "loading", None,))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot(
                "op-1",
                "failed",
                Some("write failed"),
            ))),
            now,
        ),
        AppAction::Continue
    );

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn config_surface_renders_tab_strip_with_active_providers_tab() {
    let app = open_config_app();

    assert_eq!(app.active_config_tab(), ConfigTab::Providers);

    let rendered = render_app(&app);
    assert!(rendered.contains("Providers"));
    assert!(rendered.contains("MCP Servers"));
    assert!(rendered.contains("> anthropic"));
}

#[test]
fn config_tab_switch_cycles_between_providers_mcp_and_defaults() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.active_config_tab(), ConfigTab::Providers);
    assert!(app.provider_section().config_form().is_none());

    // Tab cycles Providers -> MCP -> Defaults -> Providers.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Mcp);
    assert!(render_app(&app).contains("No MCP servers configured."));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Defaults);
    assert!(render_app(&app).contains("Space all MCP"));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Providers);

    // Provider section behavior is unchanged after switching tabs.
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    assert!(app.provider_section().config_form().is_some());
    assert!(render_app(&app).contains("Create provider"));
}

#[test]
fn config_mcp_tab_creates_edits_and_deletes_stdio_server() {
    let mut app = open_config_app();
    let now = Instant::now();

    // Activate the MCP tab.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Mcp);

    // Open the create form against the MCP section, not the provider section.
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    assert!(app.mcp_section().config_form().is_some());
    assert!(app.provider_section().config_form().is_none());
    assert!(render_app(&app).contains("Create MCP server"));

    // Type a name (the key). Default type is stdio, so the conditional field
    // is `command`.
    for character in "fs".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    // Saving with an empty command is blocked and the form stays open.
    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    assert!(app.mcp_section().config_form().is_some());
    assert!(render_app(&app).contains("command required before saving"));

    // Advance to the command field (name -> type -> command) and fill it.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    for character in "fs-server".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::AddMcpServer { server }) = action else {
        panic!("expected add mcp server command, saw {action:?}");
    };
    assert_eq!(server.kind, McpServerTypeView::Stdio);
    assert_eq!(server.command, "fs-server");
    assert!(server.url.is_empty());
}

#[test]
fn config_mcp_form_switches_to_url_field_for_sse_and_blocks_empty() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);

    for character in "remote".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    // Move to the type field and toggle to sse.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("sse"));
    assert!(rendered.contains("url required before saving"));

    // Empty url blocks save.
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(app.mcp_section().config_form().is_some());

    // Fill the conditional url field and save.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    for character in "https://example.test".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::AddMcpServer { server }) = action else {
        panic!("expected add mcp server command, saw {action:?}");
    };
    assert_eq!(server.kind, McpServerTypeView::Sse);
    assert_eq!(server.url, "https://example.test");
    assert!(server.command.is_empty());
}

#[test]
fn config_mcp_edit_and_delete_dispatch_mcp_commands() {
    let mut app = open_config_app_with_mcp();
    let now = Instant::now();

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Mcp);
    assert!(render_app(&app).contains("> files"));

    // Edit the selected server from the list.
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Edit MCP server"));
    assert_eq!(rendered.matches("Edit MCP server").count(), 1);

    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::UpdateMcpServer {
        original_name,
        server,
    }) = action
    else {
        panic!("expected update mcp server command, saw {action:?}");
    };
    assert_eq!(original_name, "files");
    assert_eq!(server.kind, McpServerTypeView::Stdio);
    assert!(app.mcp_section().config_form().is_some());
    assert!(render_app(&app).contains("Saving..."));
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot("op-1", "loading", None))),
            now
        ),
        AppAction::Continue
    );
    assert!(app.mcp_section().config_form().is_some());
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot_from(
                patched_mcp_snapshot(),
                "op-1",
                "ready",
                None,
            ))),
            now
        ),
        AppAction::Continue
    );
    assert!(app.mcp_section().config_form().is_none());

    // Delete the selected server.
    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert_eq!(
        action,
        AppAction::ClientCommand(ClientCommand::DeleteMcpServer {
            name: "files".to_owned()
        })
    );
    assert_eq!(app.mcp_section().delete_server(), Some("files"));
    assert!(render_app(&app).contains("Deleting..."));
}

#[test]
fn config_provider_patch_failure_uses_keyed_operation_result() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert!(matches!(
        action,
        AppAction::ClientCommand(ClientCommand::UpdateProvider { .. })
    ));
    assert!(render_app(&app).contains("Saving..."));

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot_from(
                patched_mcp_snapshot(),
                "op-2",
                "ready",
                None,
            ))),
            now
        ),
        AppAction::Continue
    );
    assert!(app.provider_section().config_form().is_some());

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot_from(
                patched_mcp_snapshot(),
                "op-1",
                "loading",
                None,
            ))),
            now
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot(
                "op-1",
                "failed",
                Some("write failed"),
            ))),
            now
        ),
        AppAction::Continue
    );

    assert!(app.provider_section().config_form().is_some());
    let rendered = render_app(&app);
    assert!(rendered.contains("error: write failed"));
    assert!(!rendered.contains("Saving..."));
}

#[test]
fn config_mcp_patch_failure_uses_keyed_operation_result() {
    let mut app = open_config_app_with_mcp();
    let now = Instant::now();

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert!(matches!(
        action,
        AppAction::ClientCommand(ClientCommand::UpdateMcpServer { .. })
    ));
    assert!(render_app(&app).contains("Saving..."));

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot_from(
                patched_mcp_snapshot(),
                "op-2",
                "ready",
                None,
            ))),
            now
        ),
        AppAction::Continue
    );
    assert!(app.mcp_section().config_form().is_some());

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot_from(
                patched_mcp_snapshot(),
                "op-1",
                "loading",
                None,
            ))),
            now
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(config_patch_snapshot_from(
                patched_mcp_snapshot(),
                "op-1",
                "failed",
                Some("mcp write failed"),
            ))),
            now
        ),
        AppAction::Continue
    );

    assert!(app.mcp_section().config_form().is_some());
    let rendered = render_app(&app);
    assert!(rendered.contains("error: mcp write failed"));
    assert!(!rendered.contains("Saving..."));
}

#[test]
fn config_provider_form_stages_model_create_until_provider_save() {
    let mut app = open_config_app();
    let now = Instant::now();

    // The provider list is selected; open the provider form that owns models.
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert!(app.provider_section().config_form().is_some());
    assert!(!app.provider_section().in_model_editor());
    assert!(render_app(&app).contains("No models configured."));

    // Open the model create form from inside the provider form.
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    assert!(app.provider_section().model_form().is_some());
    assert!(render_app(&app).contains("Create model"));

    // Provide a key, but no type yet.
    for character in "opus".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    // Saving without a type is blocked and the indicator shows.
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(app.provider_section().model_form().is_some());
    assert!(render_app(&app).contains("type required before saving"));

    // Move to the type field (key -> name -> type) and pick a type.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);

    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    assert!(app.provider_section().config_form().is_some());
    assert!(app.provider_section().model_form().is_none());
    assert!(render_app(&app).contains("> opus"));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::UpdateProvider {
        original_name,
        provider,
    }) = action
    else {
        panic!("expected update provider command, saw {action:?}");
    };
    assert_eq!(original_name, "anthropic");
    let model = provider
        .models
        .get("opus")
        .expect("created model should ride inside provider");
    assert_eq!(model.kind, ModelTypeView::Llm);
}

#[test]
fn config_provider_form_stages_model_edit_and_delete_until_provider_save() {
    let mut app = open_config_app_with_model();
    let now = Instant::now();

    // Open the selected provider form with its model list.
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("Claude"));

    // Edit the selected model.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("> Claude"));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Edit model"));
    assert_eq!(rendered.matches("Edit model").count(), 1);

    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    assert!(app.provider_section().config_form().is_some());

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::UpdateProvider {
        original_name,
        provider,
    }) = action
    else {
        panic!("expected update provider command, saw {action:?}");
    };
    assert_eq!(original_name, "anthropic");
    assert!(provider.models.contains_key("sonnet"));
}

#[test]
fn config_provider_model_form_escape_returns_to_provider_draft() {
    let mut app = open_config_app_with_model();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(app.provider_section().model_form().is_some());

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);

    assert!(app.provider_section().config_form().is_some());
    assert!(app.provider_section().model_form().is_none());
    assert!(render_app(&app).contains("> Claude"));
}

#[test]
fn config_provider_form_list_keys_edit_and_delete_models() {
    let mut app = open_config_app_with_model();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert!(app.provider_section().model_form().is_some());
    let rendered = render_app(&app);
    assert!(rendered.contains("Edit model"));
    assert_eq!(rendered.matches("Edit model").count(), 1);

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert!(app.provider_section().model_form().is_none());

    assert_eq!(
        app.handle_event(&backspace_press(), now),
        AppAction::Continue
    );
    assert!(render_app(&app).contains("Delete? Enter/y confirm"));

    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("No models configured."));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::UpdateProvider {
        original_name,
        provider,
    }) = action
    else {
        panic!("expected update provider command for delete, saw {action:?}");
    };
    assert_eq!(original_name, "anthropic");
    assert!(!provider.models.contains_key("sonnet"));
}

#[test]
fn config_provider_form_stages_model_delete_until_provider_save() {
    let mut app = open_config_app_with_model();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("Claude"));

    // Delete the selected model; no command is emitted until the provider saves.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("> Claude"));
    assert_eq!(
        app.handle_event(&backspace_press(), now),
        AppAction::Continue
    );
    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    assert!(render_app(&app).contains("No models configured."));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::UpdateProvider {
        original_name,
        provider,
    }) = action
    else {
        panic!("expected update provider command for delete, saw {action:?}");
    };
    assert_eq!(original_name, "anthropic");
    assert!(!provider.models.contains_key("sonnet"));
}

#[test]
fn config_new_provider_can_stage_models_before_add_provider() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    for character in "openai".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    for character in "gpt".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);

    let action = app.handle_event(&enter_press(), now);
    assert_eq!(action, AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("> gpt"), "{rendered}");

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::AddProvider { provider }) = action else {
        panic!("expected add provider command, saw {action:?}");
    };
    assert_eq!(provider.name, "openai");
    assert!(provider.models.contains_key("gpt"));
}

#[test]
fn config_leader_c_opens_create_form_and_adds_provider() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Create provider"));
    assert!(rendered.contains("Tab focus"));
    assert!(!rendered.contains("Tab switch field"));

    for character in "openai".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::AddProvider { provider }) = action else {
        panic!("expected add provider command");
    };
    assert_eq!(provider.name, "openai");
}

#[test]
fn config_right_opens_edit_form_and_updates_provider() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Edit provider"));
    assert_eq!(rendered.matches("Edit provider").count(), 1);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);

    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::UpdateProvider {
        original_name,
        provider,
    }) = action
    else {
        panic!("expected update provider command");
    };
    assert_eq!(original_name, "anthropic");
    assert_eq!(provider.kind, ProviderKindView::OpenAi);
}

#[test]
fn config_enter_opens_provider_and_mcp_items() {
    let mut provider_app = open_config_app();
    let now = Instant::now();

    assert_eq!(
        provider_app.handle_event(&enter_press(), now),
        AppAction::Continue
    );
    assert!(provider_app.provider_section().config_form().is_some());
    assert!(render_app(&provider_app).contains("Edit provider"));

    let mut mcp_app = open_config_app_with_mcp();
    assert_eq!(mcp_app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(mcp_app.active_config_tab(), ConfigTab::Mcp);
    assert_eq!(
        mcp_app.handle_event(&enter_press(), now),
        AppAction::Continue
    );
    assert!(mcp_app.mcp_section().config_form().is_some());
    assert!(render_app(&mcp_app).contains("Edit MCP server"));
}

#[test]
fn config_leader_e_is_unknown_command() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('e'), now), AppAction::Continue);

    assert!(render_app(&app).contains("unrecognized command: e"));
    assert!(app.provider_section().config_form().is_none());
}

#[test]
fn config_delete_key_confirms_delete_and_deletes_provider() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&delete_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("> anthropic"));
    assert!(rendered.contains("Delete? Enter/y confirm"));

    let action = app.handle_event(&enter_press(), now);
    let AppAction::ClientCommand(ClientCommand::DeleteProvider { name }) = action else {
        panic!("expected delete provider command");
    };
    assert_eq!(name, "anthropic");
}

#[test]
fn config_leader_d_is_unknown_command() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('d'), now), AppAction::Continue);

    assert!(render_app(&app).contains("unrecognized command: d"));
    assert!(app.provider_section().delete_provider().is_none());
}

#[test]
fn config_waits_instead_of_rendering_startup_loaded_config() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(loaded_config_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("Waiting for configuration..."));
    assert!(!rendered.contains("provider  anthropic"));
    assert!(!rendered.contains("load: ready"));
}

#[test]
fn config_renders_waiting_watch_status() {
    let mut app = App::new();
    let now = Instant::now();
    let mut snapshot = ready_snapshot();
    snapshot.config.watch.status = "waiting";

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    assert!(render_app(&app).contains("watch waiting"));
}

#[test]
fn config_pending_shows_loading_indicator_with_watch_status() {
    let mut app = App::new();
    let now = Instant::now();
    let mut snapshot = ready_snapshot();
    snapshot.config.watch.status = "opened";

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    let rendered = render_app(&app);
    // Pending: intermediary loading indicator carrying the watch status, in
    // place of the provider section content.
    assert!(rendered.contains("Loading configuration..."));
    assert!(rendered.contains("watch opened"));
    assert!(!rendered.contains("Waiting for configuration..."));
    assert!(!rendered.contains("> anthropic"));
    assert!(!rendered.contains("No providers configured."));
}

#[test]
fn config_renders_section_content_once_config_available() {
    let app = open_config_app();

    let rendered = render_app(&app);
    // Config available (watch received with data): section content renders and
    // the loading indicator is gone.
    assert!(rendered.contains("> anthropic"));
    assert!(!rendered.contains("Loading configuration..."));
}

#[test]
fn config_error_takes_precedence_over_loading_indicator() {
    let mut app = App::new();
    let now = Instant::now();
    let mut snapshot = ready_snapshot();
    snapshot.config.watch.status = "opened";
    snapshot.config.watch.error = Some("watch stream failed".to_owned());

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    let rendered = render_app(&app);
    // Error treatment shows; the loading indicator must not.
    assert!(rendered.contains("error: watch stream failed"));
    assert!(!rendered.contains("Loading configuration..."));
}

#[test]
fn config_renders_opened_and_received_watch_statuses() {
    let now = Instant::now();

    for status in ["opened", "received"] {
        let mut app = App::new();
        let mut snapshot = ready_snapshot();
        snapshot.config.watch.status = status;

        assert_eq!(
            app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
            AppAction::Continue
        );
        assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
        assert_eq!(
            app.handle_event(&char_press('c'), now),
            AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
        );

        assert!(render_app(&app).contains(&format!("watch {status}")));
    }
}

#[test]
fn config_omits_non_ready_load_status() {
    let mut app = App::new();
    let now = Instant::now();
    let mut snapshot = ready_snapshot();
    snapshot.config.load.status = "loading";

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("watch idle"));
    assert!(!rendered.contains("load: loading"));
}

#[test]
fn escape_returns_from_config_to_landing() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseConfigWatchInterest)
    );

    assert_eq!(app.screen(), AppScreen::Landing);
    assert_eq!(app.notice(), None);
}

#[test]
fn leader_c_waits_for_ready_snapshot() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);

    assert_eq!(app.screen(), AppScreen::Waiting);
}

#[test]
fn unknown_leader_command_renders_notice() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('z'), now), AppAction::Continue);

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn splash_logo_uses_neon_lavender() {
    let app = App::new();
    let mut terminal = test_terminal();

    terminal
        .draw(|frame| app.draw(frame))
        .expect("app should render");

    let buffer = terminal.backend().buffer();
    let logo_cell = &buffer[(24, 9)];

    assert_eq!(logo_cell.fg, Color::Rgb(190, 132, 242));
}

fn render_app(app: &App) -> String {
    let mut terminal = test_terminal();

    terminal
        .draw(|frame| app.draw(frame))
        .expect("app should render");

    terminal.backend().to_string()
}

fn test_terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(80, 24)).expect("test backend should initialize")
}

const fn escape_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Esc,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

const fn ctrl_x_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    )))
}

const fn char_press(character: char) -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char(character),
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

const fn enter_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

const fn backspace_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Backspace,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

const fn delete_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Delete,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

const fn tab_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Tab,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

const fn right_press() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Right,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )))
}

fn open_config_app() -> App {
    open_config_app_with(watched_config_snapshot())
}

fn open_config_app_with_mcp() -> App {
    open_config_app_with(patched_mcp_snapshot())
}

fn patched_mcp_snapshot() -> ClientSnapshot {
    let mut snapshot = watched_config_snapshot();
    if let Some(config) = snapshot.config.current.as_mut() {
        config.mcp_servers.insert(
            "files".to_owned(),
            McpServerView {
                name: "files".to_owned(),
                kind: McpServerTypeView::Stdio,
                command: "fs-server".to_owned(),
                url: String::new(),
            },
        );
    }
    snapshot.config.patch.status = "ready";
    snapshot
}

fn open_config_app_with_model() -> App {
    let mut snapshot = watched_config_snapshot();
    if let Some(config) = snapshot.config.current.as_mut()
        && let Some(provider) = config.providers.get_mut("anthropic")
    {
        provider.models.insert(
            "sonnet".to_owned(),
            ModelView {
                name: "Claude".to_owned(),
                kind: ModelTypeView::Llm,
                options: ModelOptionsView {
                    temperature: None,
                    top_p: None,
                    top_k: None,
                    min_p: None,
                    presence_penalty: None,
                    repetition_penalty: None,
                },
                limits: ModelLimitsView {
                    context: None,
                    output: None,
                },
                capabilities: ModelCapabilitiesView {
                    tools: false,
                    image: false,
                },
            },
        );
    }
    open_config_app_with(snapshot)
}

fn open_config_app_with(snapshot: ClientSnapshot) -> App {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    app
}

fn open_sessions_app_with(snapshot: ClientSnapshot) -> App {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::RegisterSessionWatchInterest)
    );

    app
}

fn config_patch_snapshot(key: &str, status: &'static str, error: Option<&str>) -> ClientSnapshot {
    config_patch_snapshot_from(watched_config_snapshot(), key, status, error)
}

fn config_patch_snapshot_from(
    mut snapshot: ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) -> ClientSnapshot {
    snapshot.config.patch = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::ConfigPatch {
            status,
            error: error.map(str::to_owned),
            config: (status == "ready")
                .then(|| snapshot.config.current.clone())
                .flatten(),
        },
    );
    snapshot
}

fn watched_sessions_snapshot(sessions: Vec<SessionView>) -> ClientSnapshot {
    let mut snapshot = ready_snapshot();
    snapshot.sessions.current = sessions;
    snapshot.sessions.list.status = "ready";
    snapshot.sessions.watch.status = "received";
    snapshot
}

fn session_create_snapshot(
    key: &str,
    status: &'static str,
    session: Option<SessionView>,
) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(session.iter().cloned().collect());
    snapshot.sessions.create = OperationSnapshot {
        status,
        error: None,
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionCreate {
            status,
            error: None,
            session,
        },
    );
    snapshot
}

fn session_create_snapshot_with_error(
    key: &str,
    status: &'static str,
    error: &str,
) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(Vec::new());
    snapshot.sessions.create = OperationSnapshot {
        status,
        error: Some(error.to_owned()),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionCreate {
            status,
            error: Some(error.to_owned()),
            session: None,
        },
    );
    snapshot
}

fn session_delete_snapshot(key: &str, status: &'static str, id: &str) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(Vec::new());
    snapshot.sessions.delete = OperationSnapshot {
        status,
        error: None,
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionDelete {
            status,
            error: None,
            id: id.to_owned(),
        },
    );
    snapshot
}

fn session_delete_snapshot_with_error(
    key: &str,
    status: &'static str,
    id: &str,
    error: &str,
) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(vec![session_view(id, 1)]);
    snapshot.sessions.delete = OperationSnapshot {
        status,
        error: Some(error.to_owned()),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionDelete {
            status,
            error: Some(error.to_owned()),
            id: id.to_owned(),
        },
    );
    snapshot
}

fn session_view(id: &str, seconds: i64) -> SessionView {
    SessionView {
        id: id.to_owned(),
        revision: 1,
        status: SessionStatusView {
            phase: SessionPhaseView::Idle,
            failed_message: String::new(),
        },
        current_prompt: None,
        created_at: Some(TimestampView { seconds, nanos: 0 }),
        updated_at: Some(TimestampView { seconds, nanos: 0 }),
        last_activity_at: None,
        runtime_config: None,
    }
}

const fn ready_snapshot() -> ClientSnapshot {
    ClientSnapshot {
        operations: OperationCollectionSnapshot {
            current: BTreeMap::new(),
        },
        config: ConfigSnapshot {
            current: None,
            load: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            patch: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            watch: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
        },
        status: StatusSnapshot {
            ready: true,
            watch: OperationSnapshot {
                status: "ready",
                error: None,
                key: None,
            },
        },
        sessions: SessionSnapshot {
            current: Vec::new(),
            create: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            list: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            delete: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            runtime_config_update: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            submit_user_message: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            respond_external_request: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            interrupt: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            watch: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            timelines: BTreeMap::new(),
        },
    }
}

fn loaded_config_snapshot() -> ClientSnapshot {
    let mut snapshot = ready_snapshot();
    snapshot.config.current = Some(ConfigView {
        etag: "etag-1".to_owned(),
        providers: BTreeMap::from([(
            "anthropic".to_owned(),
            ProviderView {
                kind: ProviderKindView::Anthropic,
                name: "anthropic".to_owned(),
                url: "https://api.anthropic.com".to_owned(),
                headers: BTreeMap::new(),
                models: BTreeMap::new(),
            },
        )]),
        mcp_servers: BTreeMap::new(),
        runtime_defaults: None,
    });
    snapshot.config.load.status = "ready";
    snapshot
}

fn watched_config_snapshot() -> ClientSnapshot {
    let mut snapshot = loaded_config_snapshot();
    snapshot.config.watch.status = "received";
    snapshot.config.load.status = "idle";
    snapshot
}
