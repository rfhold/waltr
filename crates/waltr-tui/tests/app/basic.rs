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

#[test]
fn waltr_login_required_routes_to_dedicated_access_screen() {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(login_required_snapshot("session expired"))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Access);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::NativeAuth(waltr_tui::native_auth::NativeAuthAction::Start)
    );
    insta::assert_snapshot!("waltr_access_login_required", render_app(&app));
}

#[test]
fn access_screen_shows_copyable_url_timeout_and_cancel() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(
        &AppEvent::Client(Box::new(login_required_snapshot("login required"))),
        now,
    ));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::AwaitingBrowser {
            authorization_url: "https://id.example/authorize?client_id=waltr-native&state=redacted"
                .to_owned(),
            expires_at: now + Duration::from_mins(5),
            browser_opened: true,
        }),
        now,
    ));

    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::NativeAuth(waltr_tui::native_auth::NativeAuthAction::CopyUrl(
            "https://id.example/authorize?client_id=waltr-native&state=redacted".to_owned()
        ))
    );
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::NativeAuth(waltr_tui::native_auth::NativeAuthAction::Cancel)
    );
    insta::assert_snapshot!("waltr_access_awaiting_callback", render_app(&app));

    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::BrowserLaunchFailed),
        now,
    ));
    assert_eq!(
        app.native_login_url(),
        Some("https://id.example/authorize?client_id=waltr-native&state=redacted")
    );
    assert_eq!(
        app.native_login_status(),
        "Browser unavailable; open the URL manually"
    );
}

#[test]
fn access_failure_is_retryable_and_success_waits_for_ready_reconnect() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(
        &AppEvent::Client(Box::new(login_required_snapshot("session expired"))),
        now,
    ));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::Failed {
            error: "authorization provider unavailable".to_owned(),
        }),
        now,
    ));
    assert_eq!(
        app.handle_event(&char_press('r'), now),
        AppAction::NativeAuth(waltr_tui::native_auth::NativeAuthAction::Start)
    );

    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::Succeeded),
        now,
    ));
    let mut reconnecting = ready_snapshot();
    reconnecting.access.status = waltr_client::AccessStatus::UnknownChecking;
    reconnecting.status.ready = false;
    drop(app.handle_event(&AppEvent::Client(Box::new(reconnecting)), now));
    assert_eq!(app.screen(), AppScreen::Access);

    drop(app.handle_event(
        &AppEvent::Client(Box::new(ready_snapshot())),
        now,
    ));
    assert_eq!(app.screen(), AppScreen::Landing);
}

#[test]
fn access_success_cannot_restart_login_before_reconnect() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(
        &AppEvent::Client(Box::new(login_required_snapshot("session expired"))),
        now,
    ));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::Succeeded),
        now,
    ));

    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('r'), now), AppAction::Continue);
}

#[test]
fn access_expiry_restores_config_without_watch_command_churn() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(login_required_snapshot("session expired"))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Access);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(watched_config_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Config);
}

#[test]
fn access_expiry_preserves_multi_item_config_selections_until_authoritative_restore() {
    let mut snapshot = watched_config_snapshot();
    let config = snapshot.config.current.as_mut().unwrap();
    let anthropic = config.providers.get("anthropic").unwrap().clone();
    for name in ["bravo", "charlie"] {
        let mut provider = anthropic.clone();
        provider.name = name.to_owned();
        config.providers.insert(name.to_owned(), provider);
    }
    for name in ["alpha", "bravo", "charlie"] {
        config.mcp_servers.insert(
            name.to_owned(),
            McpServerView {
                name: name.to_owned(),
                kind: McpServerTypeView::Stdio,
                command: "server".to_owned(),
                url: String::new(),
            },
        );
    }
    let restored = snapshot.clone();
    let mut app = open_config_app_with(snapshot);
    let now = Instant::now();
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.provider_section().selected_provider(), 2);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.mcp_section().selected_server(), 2);

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(login_required_snapshot("expired"))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(app.provider_section().selected_provider(), 2);
    assert_eq!(app.mcp_section().selected_server(), 2);

    let mut status_only = ready_snapshot();
    status_only.config.current = None;
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(status_only)), now),
        AppAction::Continue
    );
    assert_eq!(app.provider_section().selected_provider(), 2);
    assert_eq!(app.mcp_section().selected_server(), 2);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(restored)), now),
        AppAction::Continue
    );
    assert_eq!(app.provider_section().selected_provider(), 2);
    assert_eq!(app.mcp_section().selected_server(), 2);
    assert_eq!(app.screen(), AppScreen::Config);
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseConfigWatchInterest)
    );
}

#[test]
fn access_expiry_restores_session_detail_without_watch_command_churn() {
    let snapshot = watched_sessions_snapshot(vec![session_view("owned", 1)]);
    let mut app = open_sessions_app_with(snapshot.clone());
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("owned")
    );

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(login_required_snapshot("session expired"))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Access);
    assert_eq!(app.sessions_section().detail_session_id(), Some("owned"));
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Sessions);
    assert_eq!(app.sessions_section().detail_session_id(), Some("owned"));
}

#[test]
fn access_expiry_preserves_multi_item_session_selection_until_authoritative_restore() {
    let snapshot = watched_sessions_snapshot(vec![
        session_view("alpha", 3),
        session_view("bravo", 2),
        session_view("charlie", 1),
    ]);
    let mut app = open_sessions_app_with(snapshot.clone());
    let now = Instant::now();
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.sessions_section().selected_session(), 2);

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(login_required_snapshot("expired"))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(app.sessions_section().selected_session(), 2);

    let status_only = ready_snapshot();
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(status_only)), now),
        AppAction::Continue
    );
    assert_eq!(app.sessions_section().selected_session(), 2);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.sessions_section().selected_session(), 2);
    assert_eq!(app.screen(), AppScreen::Sessions);
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseSessionWatchInterest)
    );
}

#[test]
fn explicit_logout_clears_protected_forms_and_composer_draft() {
    let mut snapshot = watched_config_snapshot();
    snapshot.config.current.as_mut().unwrap().providers.get_mut("anthropic").unwrap()
        .headers
        .insert("authorization".to_owned(), "secret draft".to_owned());
    let sessions = watched_sessions_snapshot(vec![session_view("owned", 1)]);
    snapshot.sessions = sessions.sessions;
    let mut app = open_config_app_with(snapshot);
    let now = Instant::now();

    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(app.provider_section().config_form().is_some());
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseConfigWatchInterest,
            ClientCommand::RegisterSessionWatchInterest,
        ])
    );
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("owned")
    );
    for character in "private draft".chars() {
        assert_eq!(app.handle_event(&char_press(character), now), AppAction::Continue);
    }
    assert!(render_app(&app).contains("private draft"));

    assert_eq!(
        app.handle_event(
            &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Waiting);
    assert!(app.client_snapshot().is_none());
    assert_eq!(app.active_config_tab(), ConfigTab::Providers);
    assert_eq!(
        app.provider_section(),
        &waltr_tui::screens::ProviderSection::new()
    );
    assert_eq!(app.mcp_section(), &waltr_tui::screens::McpSection::new());
    assert_eq!(
        app.runtime_defaults_section(),
        &waltr_tui::screens::RuntimeDefaultsSection::new()
    );
    assert_eq!(app.sessions_section(), &SessionsSection::new());
    assert!(app.auth_input().is_empty());
    assert_eq!(app.notice(), None);
    assert!(!app.can_logout());
}

#[test]
fn explicit_logout_rejects_buffered_authenticated_snapshot_until_reducer_barrier() {
    let protected = watched_config_snapshot();
    let mut app = open_config_app_with(protected.clone());
    let now = Instant::now();
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(protected)), now),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Waiting);
    assert!(app.client_snapshot().is_none());
    assert_eq!(
        app.provider_section(),
        &waltr_tui::screens::ProviderSection::new()
    );

    let mut checking = ready_snapshot();
    checking.access.status = waltr_client::AccessStatus::UnknownChecking;
    checking.status.ready = false;
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(checking)), now),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Waiting);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Landing);
    assert!(!app.can_logout());
}

#[test]
fn explicit_logout_clears_mcp_defaults_auth_and_notices() {
    let now = Instant::now();

    let mut mcp_app = open_config_app_with_mcp();
    assert_eq!(mcp_app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(mcp_app.handle_event(&delete_press(), now), AppAction::Continue);
    assert_eq!(mcp_app.mcp_section().delete_server(), Some("files"));
    drop(mcp_app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert_eq!(mcp_app.mcp_section(), &waltr_tui::screens::McpSection::new());

    let mut defaults_app = open_config_app_with_mcp();
    assert_eq!(defaults_app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(defaults_app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(defaults_app.handle_event(&char_press(' '), now), AppAction::Continue);
    assert!(defaults_app.runtime_defaults_section().has_overlay());
    drop(defaults_app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert_eq!(
        defaults_app.runtime_defaults_section(),
        &waltr_tui::screens::RuntimeDefaultsSection::new()
    );

    let mut protected = ready_snapshot();
    protected.auth.pending.push(waltr_client::AuthRequirementSnapshot {
        target: waltr_client::ClientAuthTarget {
            kind: "provider".to_owned(),
            key: "anthropic".to_owned(),
        },
        message: "authorization required".to_owned(),
        source_session_id: None,
    });
    protected.auth.active_attempt = Some(waltr_client::AuthAttemptSnapshot {
        target: waltr_client::ClientAuthTarget {
            kind: "provider".to_owned(),
            key: "anthropic".to_owned(),
        },
        attempt_id: "attempt".to_owned(),
        auth_url: "https://id.example/authorize".to_owned(),
        verification_url: String::new(),
        user_code: String::new(),
        expires_at_ms: 0,
        poll_interval_seconds: 0,
        messages: Vec::new(),
    });
    let mut auth_app = App::new();
    drop(auth_app.handle_event(&AppEvent::Client(Box::new(protected)), now));
    for character in "private callback".chars() {
        assert_eq!(auth_app.handle_event(&char_press(character), now), AppAction::Continue);
    }
    assert_eq!(auth_app.auth_input(), "private callback");
    drop(auth_app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert!(auth_app.auth_input().is_empty());
    assert!(auth_app.client_snapshot().is_none());

    let mut notice_app = App::new();
    drop(notice_app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));
    assert_eq!(notice_app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(notice_app.handle_event(&char_press('z'), now), AppAction::Continue);
    assert!(notice_app.notice().is_some());
    drop(notice_app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert_eq!(notice_app.notice(), None);
}

#[test]
fn current_401_disables_logout_and_uses_transient_reprobe() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::Succeeded),
        now,
    ));
    assert!(app.can_logout());

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(login_required_snapshot("credential rejected"))),
            now,
        ),
        AppAction::ClientCommand(ClientCommand::ReprobeWaltrAccess)
    );
    assert!(!app.can_logout());
    let mut checking = ready_snapshot();
    checking.access.status = waltr_client::AccessStatus::UnknownChecking;
    checking.status.ready = false;
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(checking)), now),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
        AppAction::Continue
    );
    assert_eq!(app.screen(), AppScreen::Landing);
    assert!(!app.can_logout());
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('l'), now), AppAction::Continue);
}

#[test]
fn provider_oauth_modal_remains_separate_from_waltr_access() {
    let mut snapshot = ready_snapshot();
    snapshot.auth.pending.push(waltr_client::AuthRequirementSnapshot {
        target: waltr_client::ClientAuthTarget {
            kind: "provider".to_owned(),
            key: "anthropic".to_owned(),
        },
        message: "provider credentials required".to_owned(),
        source_session_id: None,
    });
    let mut app = App::new();
    drop(app.handle_event(
        &AppEvent::Client(Box::new(snapshot.clone())),
        Instant::now(),
    ));
    assert!(render_app(&app).contains("OAuth required"));

    snapshot.access.status = waltr_client::AccessStatus::LoginRequired;
    snapshot.status.ready = false;
    drop(app.handle_event(
        &AppEvent::Client(Box::new(snapshot)),
        Instant::now(),
    ));
    let rendered = render_app(&app);
    assert!(rendered.contains("Waltr sign in"));
    assert!(!rendered.contains("OAuth required"));
}

#[test]
fn leader_l_logs_out_through_native_runtime() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::Succeeded),
        now,
    ));
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('l'), now),
        AppAction::NativeAuth(waltr_tui::native_auth::NativeAuthAction::Logout)
    );
}

#[test]
fn logout_action_is_hidden_and_disabled_without_native_credential() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));

    assert!(!render_app(&app).contains("C-x l logout"));
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('l'), now), AppAction::Continue);
}

#[test]
fn access_screen_ignores_release_and_repeat_keys() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(
        &AppEvent::Client(Box::new(login_required_snapshot("login required"))),
        now,
    ));

    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        for code in [KeyCode::Enter, KeyCode::Char('r'), KeyCode::Char('c'), KeyCode::Esc] {
            assert_eq!(
                app.handle_event(
                    &AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
                        code,
                        KeyModifiers::NONE,
                        kind,
                    ))),
                    now,
                ),
                AppAction::Continue
            );
        }
    }

    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::AwaitingBrowser {
            authorization_url: "https://id.example/authorize".to_owned(),
            expires_at: now + Duration::from_mins(5),
            browser_opened: false,
        }),
        now,
    ));
    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        for code in [KeyCode::Char('c'), KeyCode::Esc] {
            assert_eq!(
                app.handle_event(
                    &AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
                        code,
                        KeyModifiers::NONE,
                        kind,
                    ))),
                    now,
                ),
                AppAction::Continue
            );
        }
    }
}

#[test]
fn logout_ignores_release_and_repeat_keys() {
    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        let mut app = App::new();
        let now = Instant::now();
        drop(app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));
        assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
        assert_eq!(
            app.handle_event(
                &AppEvent::Input(InputEvent::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('l'),
                    KeyModifiers::NONE,
                    kind,
                ))),
                now,
            ),
            AppAction::Continue
        );
    }
}
