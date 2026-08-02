#[test]
fn leader_w_opens_session_data_and_escape_returns_to_sessions() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(Vec::new()));
    let now = Instant::now();

    let sessions = render_app(&app);
    assert!(sessions.contains("C-x w session data"), "{sessions}");
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('w'), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::SessionData);
    insta::assert_snapshot!("session_data_confirmation", render_app(&app));

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Sessions);
}

#[test]
fn session_data_entrypoint_is_rejected_outside_sessions() {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));

    assert!(!render_app(&app).contains("C-x w session data"));
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('w'), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Landing);

    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('c'), now), expected_config_entry_action());
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('w'), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Config);
}

#[test]
fn session_data_pre_key_n_and_escape_cancel_after_single_dispatch() {
    let now = Instant::now();

    for cancel in [char_press('n'), escape_press()] {
        let mut app = open_sessions_app_with(watched_sessions_snapshot(Vec::new()));
        drop(app.handle_event(&ctrl_x_press(), now));
        drop(app.handle_event(&char_press('w'), now));

        assert_eq!(
            app.handle_event(&enter_press(), now),
            AppAction::ClientCommand(ClientCommand::WipeSessions)
        );
        assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
        assert_eq!(
            app.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now),
            AppAction::Continue
        );
        assert_eq!(app.handle_event(&cancel, now), AppAction::Continue);
        assert_eq!(app.screen(), AppScreen::Sessions);
    }
}

#[test]
fn session_data_pending_locks_input_and_failure_is_retryable() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('w'), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::SessionData);
    assert_eq!(
        app.handle_event(&char_press('y'), now),
        AppAction::ClientCommand(ClientCommand::WipeSessions)
    );
    drop(app.handle_event(
        &AppEvent::Client(Box::new(session_wipe_snapshot_from(
            watched_sessions_snapshot(vec![session_view("one", 1)]),
            "wipe-1",
            "loading",
            None,
        ))),
        now,
    ));
    insta::assert_snapshot!("session_data_pending", render_app(&app));

    for input in [enter_press(), char_press('n'), escape_press(), ctrl_x_press()] {
        assert_eq!(app.handle_event(&input, now), AppAction::Continue);
    }
    assert_eq!(app.screen(), AppScreen::SessionData);

    drop(app.handle_event(
        &AppEvent::Client(Box::new(session_wipe_snapshot_from(
            watched_sessions_snapshot(vec![session_view("one", 1)]),
            "wipe-1",
            "failed",
            Some("session store unavailable"),
        ))),
        now,
    ));
    insta::assert_snapshot!("session_data_failure", render_app(&app));
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::WipeSessions)
    );
    assert!(!render_app(&app).contains("session store unavailable"));
}

#[test]
fn session_data_only_matching_key_completes_and_success_stays_on_page() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();
    drop(app.handle_event(&ctrl_x_press(), now));
    drop(app.handle_event(&char_press('w'), now));
    drop(app.handle_event(&enter_press(), now));

    drop(app.handle_event(
        &AppEvent::Client(Box::new(session_wipe_snapshot_from(
            watched_sessions_snapshot(Vec::new()),
            "stale-wipe",
            "ready",
            None,
        ))),
        now,
    ));
    assert!(render_app(&app).contains("Wiping session data..."));

    let loading = session_wipe_snapshot_from(
        watched_sessions_snapshot(vec![session_view("one", 1)]),
        "wipe-1",
        "loading",
        None,
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(loading.clone())), now));

    let mut unrelated = session_wipe_snapshot_from(
        watched_sessions_snapshot(Vec::new()),
        "other-wipe",
        "ready",
        None,
    );
    unrelated.operations.current.extend(loading.operations.current);
    drop(app.handle_event(&AppEvent::Client(Box::new(unrelated)), now));
    assert!(render_app(&app).contains("Wiping session data..."));

    drop(app.handle_event(
        &AppEvent::Client(Box::new(session_wipe_snapshot_from(
            watched_sessions_snapshot(Vec::new()),
            "wipe-1",
            "ready",
            None,
        ))),
        now,
    ));
    assert_eq!(app.screen(), AppScreen::SessionData);
    let rendered = render_app(&app);
    assert!(rendered.contains("No sessions"), "{rendered}");
    assert!(rendered.contains("remain."), "{rendered}");
    insta::assert_snapshot!("session_data_success", rendered);
}

#[test]
fn opening_session_data_from_detail_releases_timeline_and_keeps_delete_binding_separate() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("one")
    );

    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(
        app.handle_event(&char_press('w'), now),
        AppAction::ClientCommand(ClientCommand::ReleaseSessionTimelineWatchInterest {
            session_id: "one".to_owned(),
        })
    );
    assert_eq!(app.sessions_section().detail_session_id(), None);
    assert_eq!(app.screen(), AppScreen::SessionData);
}

#[test]
fn remote_clear_reconciles_open_detail_from_shared_snapshot() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("one")
    );

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(watched_sessions_snapshot(Vec::new()))),
            now,
        ),
        AppAction::ClientCommand(ClientCommand::ReleaseSessionTimelineWatchInterest {
            session_id: "one".to_owned(),
        })
    );
    assert_eq!(app.sessions_section().detail_session_id(), None);
    assert!(render_app(&app).contains("No sessions yet."));
}
