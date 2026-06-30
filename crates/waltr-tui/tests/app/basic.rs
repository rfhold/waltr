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
        THEME.canvas_style(),
        Style::default()
            .fg(Color::Rgb(246, 241, 255))
            .bg(Color::Rgb(7, 5, 12))
            .not_dim()
    );
    assert_eq!(
        THEME.surface_style(),
        Style::default()
            .fg(Color::Rgb(246, 241, 255))
            .bg(Color::Rgb(15, 12, 23))
            .not_dim()
    );
    assert_eq!(
        THEME.panel_style(),
        Style::default()
            .fg(Color::Rgb(246, 241, 255))
            .bg(Color::Rgb(24, 20, 34))
            .not_dim()
    );
    assert_eq!(
        THEME.panel_muted_style(),
        Style::default()
            .fg(Color::DarkGray)
            .bg(Color::Rgb(24, 20, 34))
            .not_dim()
    );
    assert_eq!(
        THEME.footer_style(),
        Style::default()
            .fg(Color::DarkGray)
            .bg(Color::Rgb(30, 20, 46))
            .not_dim()
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
    let manifest = include_str!("../../Cargo.toml");

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
