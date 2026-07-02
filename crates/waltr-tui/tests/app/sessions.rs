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
    let rendered = render_app_with_size(&app, 120, 24);
    assert!(rendered.contains("Sessions"));
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
        expected_register_timeline_command("created")
    );

    assert_eq!(app.sessions_section().detail_session_id(), Some("created"));
    let rendered = render_app_with_size(&app, 120, 24);
    assert!(rendered.contains("provider none (C-x p)"));
    assert!(rendered.contains("mcp none (C-x m)"));
    assert!(rendered.contains("ctx 0"));
}

#[test]
fn sessions_provider_runtime_editor_renders_layered_snapshot() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Search"));
    assert!(rendered.contains("Models"));
    assert!(rendered.contains("anthropic/claude"));
    assert!(rendered.contains("openai/gpt4o"));
    assert!(rendered.contains("[selected]"));
    assert!(rendered.contains("[current]"));
    assert!(!rendered.contains("Ctrl-T"));
    assert!(!rendered.contains("thinking low"));
    assert!(!rendered.contains("Providers"));
    assert!(!rendered.contains("Thinking"));
    insta::assert_snapshot!(rendered);
}

#[test]
fn sessions_runtime_search_supports_middle_editing() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('g'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('t'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&left_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Search"));
    assert!(rendered.contains("openai/gpt4o"));
    assert!(!rendered.contains("anthropic/claude"));
}

#[test]
fn sessions_detail_ctrl_t_cycles_runtime_thinking() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(
        app.handle_event(&ctrl_t_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionRuntimeConfig {
            id: "chat".to_owned(),
            provider_key: "openai".to_owned(),
            model_id: "gpt4o".to_owned(),
            reasoning_effort: Some("high".to_owned()),
            enabled_mcp_server_keys: vec!["files".to_owned(), "ghost".to_owned()],
        })
    );

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("(Ctrl-T)"));
}

#[test]
fn sessions_mcp_runtime_editor_renders_layered_snapshot() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('m'), now), AppAction::Continue);

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Search"));
    assert!(rendered.contains("MCP Servers"));
    assert!(rendered.contains("[enabled]"));
    assert!(rendered.contains("[available]"));
    assert!(rendered.contains("[missing]"));
    assert!(rendered.contains("[selected]"));
    insta::assert_snapshot!(rendered);
}

#[test]
fn sessions_provider_runtime_editor_enter_saves_selected_model() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionRuntimeConfig {
            id: "chat".to_owned(),
            provider_key: "anthropic".to_owned(),
            model_id: "claude".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
            enabled_mcp_server_keys: vec!["files".to_owned(), "ghost".to_owned()],
        })
    );
}

#[test]
fn sessions_mcp_runtime_editor_toggles_and_saves_enabled_servers() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('m'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press(' '), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionRuntimeConfig {
            id: "chat".to_owned(),
            provider_key: "openai".to_owned(),
            model_id: "gpt4o".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
            enabled_mcp_server_keys: vec!["ghost".to_owned()],
        })
    );
}

#[test]
fn sessions_runtime_editor_requests_config_then_opens_when_config_arrives() {
    let mut snapshot = runtime_editor_snapshot();
    snapshot.config.current = None;
    let mut app = open_sessions_app_with(snapshot);
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('p'), now),
        AppAction::ClientCommand(ClientCommand::RequestConfig)
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(runtime_editor_snapshot())), now),
        AppAction::Continue
    );

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Models"), "{rendered}");
}

#[test]
fn sessions_runtime_editor_escape_closes_overlay_before_interrupting_active_session() {
    let mut app = open_sessions_app_with(active_runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert!(render_app_with_size(&app, 100, 30).contains("Models"));

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    let rendered = render_app_with_size(&app, 100, 30);
    assert!(!rendered.contains("Models"), "{rendered}");
    assert!(rendered.contains("Working..."), "{rendered}");

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&escape_press(), now + Duration::from_millis(500)),
        AppAction::ClientCommand(ClientCommand::InterruptSession {
            session_id: "chat".to_owned(),
        })
    );
}

#[test]
fn sessions_runtime_editor_update_failure_preserves_editor() {
    let mut app = open_sessions_app_with(runtime_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionRuntimeConfig { .. })
    ));
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(runtime_update_snapshot("loading", None))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(runtime_update_snapshot(
                "failed",
                Some("runtime save failed"),
            ))),
            now,
        ),
        AppAction::Continue
    );

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Models"), "{rendered}");
}

#[test]
fn sessions_detail_delete_dispatches_and_returns_to_list() {
    let mut app =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("doomed", 1)]));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("doomed")
    );
    assert_eq!(app.sessions_section().detail_session_id(), Some("doomed"));
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('d'), now), AppAction::Continue);

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
        AppAction::ClientCommand(ClientCommand::ReleaseSessionTimelineWatchInterest {
            session_id: "doomed".to_owned(),
        })
    );

    assert_eq!(app.sessions_section().detail_session_id(), None);
    assert!(render_app(&app).contains("Recent sessions"));
}

#[test]
fn sessions_detail_escape_releases_timeline_watch_interest() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("one")
    );
    assert_eq!(app.sessions_section().detail_session_id(), Some("one"));
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseSessionTimelineWatchInterest {
            session_id: "one".to_owned(),
        })
    );
    assert_eq!(app.sessions_section().detail_session_id(), None);
    assert_eq!(app.screen(), AppScreen::Sessions);
}

#[test]
fn sessions_detail_repeated_enter_does_not_register_again() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("one")
    );
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.sessions_section().detail_session_id(), Some("one"));
}

#[test]
fn sessions_detail_scroll_does_not_switch_hidden_selection() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![
        session_view("one", 2),
        session_view("two", 1),
    ]));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("one")
    );
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert_eq!(app.sessions_section().detail_session_id(), Some("one"));
}

#[test]
fn sessions_list_scrolls_to_selected_row() {
    let sessions = (1..=10)
        .map(|index| session_view(&format!("session-{index:02}"), index))
        .collect();
    let mut app = open_sessions_app_with(watched_sessions_snapshot(sessions));
    let now = Instant::now();

    for _ in 0..8 {
        assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    }

    let rendered = render_app_with_size(&app, 80, 12);
    assert!(rendered.contains("session-02"), "{rendered}");
    assert!(!rendered.contains("session-10"), "{rendered}");
}

#[test]
fn sessions_detail_leader_c_releases_timeline_before_config_watch() {
    let mut app = open_sessions_app_with(watched_sessions_snapshot(vec![session_view("one", 1)]));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("one")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseSessionTimelineWatchInterest {
                session_id: "one".to_owned(),
            },
            ClientCommand::ReleaseSessionWatchInterest,
            ClientCommand::RegisterConfigWatchInterest,
        ])
    );
    assert_eq!(app.screen(), AppScreen::Config);
}

#[test]
fn sessions_detail_renders_timeline_from_snapshot() {
    let mut snapshot = watched_sessions_snapshot(vec![session_view("chat", 1)]);
    snapshot.sessions.timelines.insert(
        "chat".to_owned(),
        SessionTimelineView {
            events: vec![
                timeline_text_event(1, "chat", "user_text", "hello"),
                timeline_text_event(2, "chat", "assistant_text", "hi there"),
                timeline_tool_call_event(
                    3,
                    "chat",
                    "tool-item-1",
                    "call-1",
                    "shell",
                    r#"{"command":"pwd","description":"Show cwd"}"#,
                ),
                timeline_tool_result_event(
                    4,
                    "chat",
                    "tool-result-1",
                    Some("tool-item-1"),
                    "call-1",
                    "shell",
                    "done",
                    false,
                ),
            ],
            load: idle_operation(),
            watch: OperationSnapshot {
                status: "received",
                error: None,
                key: None,
            },
        },
    );
    let mut app = open_sessions_app_with(snapshot);

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("hello"));
    assert!(rendered.contains("hi there"));
    assert!(rendered.contains("Show cwd"));
    assert!(rendered.contains("$ pwd"));
    assert!(rendered.contains("done"));
}

#[test]
fn sessions_detail_click_user_message_reruns_from_cursor() {
    let snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(42, "chat", "user_text", "try this")],
    );
    let mut section = open_detail_section(&snapshot);
    let area = Rect::new(0, 0, 100, 30);

    assert_eq!(
        section.handle_detail_click(Some(&snapshot), area, 1, 0),
        SectionAction::Continue
    );
    let rendered = render_section(&section, &snapshot, area);
    assert!(rendered.contains("Message actions"));
    assert!(rendered.contains("Rerun"));
    assert!(rendered.contains("Cancel"));

    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Enter), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::ContinueSessionFromCursor {
            session_id: "chat".to_owned(),
            cursor: 42,
        })
    );
}

#[test]
fn sessions_detail_click_user_message_edits_before_cursor() {
    let snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(43, "chat", "user_text", "rewrite me")],
    );
    let mut section = open_detail_section(&snapshot);
    let area = Rect::new(0, 0, 100, 30);

    assert_eq!(
        section.handle_detail_click(Some(&snapshot), area, 1, 0),
        SectionAction::Continue
    );
    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Down), Some(&snapshot), Some(area)),
        SectionAction::Continue
    );
    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Enter), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::RollbackSessionBeforeCursor {
            session_id: "chat".to_owned(),
            cursor: 43,
        })
    );

    let rendered = render_section(&section, &snapshot, area);
    assert!(rendered.contains("rewrite me"));
}

#[test]
fn sessions_detail_click_resolved_external_request_reanswers_open_cursor() {
    let snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        vec![
            external_request_event(7, "chat", "request-item", "request-1", 1, "{}"),
            resolved_external_request_event(8, "chat", "request-item"),
        ],
    );
    let mut section = open_detail_section(&snapshot);
    let area = Rect::new(0, 0, 100, 30);

    assert_eq!(
        section.handle_detail_click(Some(&snapshot), area, 1, 0),
        SectionAction::Continue
    );
    let rendered = render_section(&section, &snapshot, area);
    assert!(rendered.contains("Prompt actions"));
    assert!(rendered.contains("Re-answer"));

    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Enter), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::RollbackSessionToCursor {
            session_id: "chat".to_owned(),
            cursor: 7,
        })
    );
}

#[test]
fn sessions_detail_click_turn_item_can_rollback_or_continue() {
    let snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_metrics_event(9, "chat", 10, 5)],
    );
    let mut section = open_detail_section(&snapshot);
    let area = Rect::new(0, 0, 100, 30);

    assert_eq!(
        section.handle_detail_click(Some(&snapshot), area, 1, 0),
        SectionAction::Continue
    );
    let rendered = render_section(&section, &snapshot, area);
    assert!(rendered.contains("Turn actions"));
    assert!(rendered.contains("Roll back"));
    assert!(rendered.contains("Continue"));

    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Enter), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::RollbackSessionToTurn {
            session_id: "chat".to_owned(),
            turn: 9,
        })
    );

    let mut section = open_detail_section(&snapshot);
    assert_eq!(
        section.handle_detail_click(Some(&snapshot), area, 1, 0),
        SectionAction::Continue
    );
    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Down), Some(&snapshot), Some(area)),
        SectionAction::Continue
    );
    assert_eq!(
        section.handle_key(KeyEvent::from(KeyCode::Enter), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::ContinueSessionFromTurn {
            session_id: "chat".to_owned(),
            turn: 9,
        })
    );
}

fn open_detail_section(snapshot: &ClientSnapshot) -> SessionsSection {
    let mut section = SessionsSection::new();
    assert_eq!(
        section.handle_key(
            KeyEvent::from(KeyCode::Right),
            Some(snapshot),
            Some(Rect::new(0, 0, 100, 30)),
        ),
        SectionAction::ClientCommand(ClientCommand::RegisterSessionTimelineWatchInterest {
            session_id: "chat".to_owned(),
            after_cursor: None,
            channels: timeline_channels(),
            initial_limit: Some(64),
        })
    );
    section
}

fn render_section(section: &SessionsSection, snapshot: &ClientSnapshot, area: Rect) -> String {
    let mut terminal = test_terminal_with_size(area.width, area.height);
    terminal
        .draw(|frame| draw_sessions(frame, area, section, Some(snapshot)))
        .expect("section should render");
    terminal.backend().to_string()
}

fn resolved_external_request_event(
    cursor: u64,
    session_id: &str,
    item_id: &str,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: 1,
        item_id: item_id.to_owned(),
        parent_id: None,
        kind: TimelineEventKindView::ExternalRequestResolved,
        channel: Some("external_request".to_owned()),
        content: TimelineContentView::ExternalRequest { request: None },
        status: Some(TimelineStatusView::Resolved),
        metadata_json: "{}".to_owned(),
    }
}

#[test]
fn sessions_detail_composer_uses_latest_context_tokens() {
    let mut session = session_view("chat", 1);
    session.metrics.total_tokens = 999;
    session.metrics.input_tokens = 999;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session,
        vec![
            timeline_metrics_event(1, "chat", 10, 5),
            timeline_metrics_event(2, "chat", 150, 20),
        ],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app_with_size(&app, 120, 24);
    assert!(rendered.contains("ctx 170"));
    assert!(!rendered.contains("ctx 999"));
}

#[test]
fn sessions_detail_composer_uses_cached_context_and_rates() {
    let mut session = session_view("chat", 1);
    session.metrics.input_tokens = 100;
    session.metrics.output_tokens = 10;
    session.metrics.cache_read_tokens = 100;
    session.metrics.cache_write_tokens = 20;
    session.metrics.total_tokens = 110;
    session.metrics.total_input_tokens = 220;
    session.metrics.context_tokens = 230;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session,
        vec![timeline_cached_metrics_event(1, "chat", 100, 10, 100, 20)],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app_with_size(&app, 120, 24);
    assert!(rendered.contains("ctx 230"));
    assert!(rendered.contains("c 45.5%/9.1%"));
}

#[test]
fn sessions_detail_shell_prefers_progress_attached_by_parent_item_id() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![
            timeline_tool_call_event(
                1,
                "chat",
                "tool-item-1",
                "call-1",
                "shell",
                r#"{"command":"cargo test"}"#,
            ),
            timeline_tool_progress_event(
                2,
                "chat",
                "progress-1",
                "tool-item-1",
                "tool_progress_info",
                "progress output\n",
                true,
            ),
            timeline_tool_result_event(
                3,
                "chat",
                "result-1",
                Some("tool-item-1"),
                "call-1",
                "shell",
                "result output",
                false,
            ),
        ],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("$ cargo test"));
    assert!(rendered.contains("progress output"));
    assert!(!rendered.contains("result output"));
}

#[test]
fn sessions_detail_shell_collapses_long_output_to_recent_lines() {
    let output = (1..=20)
        .map(|line| format!("line-{line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![
            timeline_tool_call_event(
                1,
                "chat",
                "tool-item-1",
                "call-1",
                "shell",
                r#"{"command":"seq 20","description":"Count"}"#,
            ),
            timeline_tool_progress_event(
                2,
                "chat",
                "progress-1",
                "tool-item-1",
                "tool_progress_info",
                &format!("{output}\n"),
                true,
            ),
            timeline_tool_result_event(
                3,
                "chat",
                "result-1",
                Some("tool-item-1"),
                "call-1",
                "shell",
                "stale result",
                false,
            ),
        ],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app_with_size(&app, 100, 40);
    assert!(rendered.contains("Count"));
    assert!(rendered.contains("last 15 of 20 lines"));
    assert!(rendered.contains("line-06"));
    assert!(rendered.contains("line-20"));
    assert!(!rendered.contains("line-05"));
    assert!(!rendered.contains("stale result"));
}

#[test]
fn sessions_detail_renders_edit_tool_as_diff() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![
            timeline_tool_call_event(
                1,
                "chat",
                "tool-item-1",
                "call-1",
                "edit",
                r#"{"filePath":"src/main.rs","oldString":"old\nline","newString":"new\nline"}"#,
            ),
            timeline_tool_result_event(
                2,
                "chat",
                "result-1",
                Some("tool-item-1"),
                "call-1",
                "edit",
                "",
                false,
            ),
        ],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("edit"));
    assert!(rendered.contains("M src/main.rs"));
    assert!(rendered.contains("--- src/main.rs"));
    assert!(rendered.contains("+++ src/main.rs"));
    assert!(rendered.contains("- old"));
    assert!(rendered.contains("+ new"));
}

#[test]
fn sessions_detail_renders_apply_patch_tool_as_file_diff() {
    let patch_text = "*** Begin Patch\n*** Add File: notes.txt\n+hello\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** Delete File: old.txt\n*** End Patch";
    let arguments_json = serde_json::json!({ "patchText": patch_text }).to_string();
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![
            timeline_tool_call_event(
                1,
                "chat",
                "tool-item-1",
                "call-1",
                "apply_patch",
                &arguments_json,
            ),
            timeline_tool_result_event(
                2,
                "chat",
                "result-1",
                Some("tool-item-1"),
                "call-1",
                "apply_patch",
                "Patch applied",
                false,
            ),
        ],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app_with_size(&app, 100, 40);
    assert!(rendered.contains("apply_patch"));
    assert!(rendered.contains("A notes.txt"));
    assert!(rendered.contains("M src/main.rs"));
    assert!(rendered.contains("D old.txt"));
    assert!(rendered.contains("*** Add File: notes.txt"));
    assert!(rendered.contains("+hello"));
    assert!(rendered.contains("-old"));
    assert!(rendered.contains("Patch applied"));
}

#[test]
fn sessions_detail_renders_generic_tool_with_sorted_input_label() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_tool_call_event(
            1,
            "chat",
            "tool-item-1",
            "call-1",
            "read",
            r#"{"z":2,"a":"src/main.rs"}"#,
        )],
    ));

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app(&app);
    assert!(rendered.contains(r#"read(a: "src/main.rs", z: 2)"#));
    assert!(rendered.contains("loading..."));
}

#[test]
fn sessions_detail_submit_dispatches_message_and_clears_on_success() {
    let session = session_view("chat", 1);
    let mut app =
        open_sessions_app_with(watched_session_with_timeline(session.clone(), Vec::new()));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    for character in "hello".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::SubmitUserMessage {
            session_id: "chat".to_owned(),
            text: "hello".to_owned(),
        })
    );

    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_submit_snapshot(
                session.clone(),
                "op-submit",
                "loading",
                None,
            ))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_submit_snapshot(
                session,
                "op-submit",
                "ready",
                None,
            ))),
            now,
        ),
        AppAction::Continue
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("type a message"));
    assert!(!rendered.contains("hello"));
}

#[test]
fn sessions_detail_supports_cursor_editing() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        Vec::new(),
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    for character in "helo".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(app.handle_event(&left_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('l'), now), AppAction::Continue);

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::SubmitUserMessage {
            session_id: "chat".to_owned(),
            text: "hello".to_owned(),
        })
    );
}

#[test]
fn sessions_detail_shift_enter_inserts_multiline_message() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        Vec::new(),
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    for character in "first".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(
        app.handle_event(&shift_enter_press(), now),
        AppAction::Continue
    );
    for character in "second".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    let rendered = render_app_with_size(&app, 80, 30);
    assert!(!rendered.contains("> type a message"));
    assert!(rendered.contains("first"));
    assert!(rendered.contains("second"));
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::SubmitUserMessage {
            session_id: "chat".to_owned(),
            text: "first\nsecond".to_owned(),
        })
    );
}

#[test]
fn sessions_detail_submit_failure_preserves_draft() {
    let session = session_view("chat", 1);
    let mut app =
        open_sessions_app_with(watched_session_with_timeline(session.clone(), Vec::new()));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    for character in "hello".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::SubmitUserMessage {
            session_id: "chat".to_owned(),
            text: "hello".to_owned(),
        })
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_submit_snapshot(
                session.clone(),
                "op-submit",
                "loading",
                None,
            ))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(session_submit_snapshot(
                session,
                "op-submit",
                "failed",
                Some("send failed"),
            ))),
            now,
        ),
        AppAction::Continue
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("hello"));
    assert!(rendered.contains("send failed"));
}

#[test]
fn sessions_detail_stop_dispatches_only_for_active_sessions() {
    let active = session_with_phase("active", 1, SessionPhaseView::StreamingProvider, "");
    let mut app = open_sessions_app_with(watched_session_with_timeline(active, Vec::new()));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("active")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::InterruptSession {
            session_id: "active".to_owned(),
        })
    );

    let idle = session_view("idle", 1);
    let mut app = open_sessions_app_with(watched_session_with_timeline(idle, Vec::new()));
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("idle")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('s'), now), AppAction::Continue);
}

#[test]
fn sessions_detail_escape_interrupts_active_session_on_second_press() {
    let active = session_with_phase("active", 1, SessionPhaseView::StreamingProvider, "");
    let mut app = open_sessions_app_with(watched_session_with_timeline(active, Vec::new()));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("active")
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("Working..."));
    assert!(rendered.contains("Esc Esc interrupt"));

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert!(app.session_interrupt_armed());
    assert!(render_app(&app).contains("Esc again interrupt"));

    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::InterruptSession {
            session_id: "active".to_owned(),
        })
    );
}

#[test]
fn sessions_detail_page_up_requests_older_timeline() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(42, "chat", "user_text", "older")],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(
        app.handle_event(&page_up_press(), now),
        AppAction::ClientCommand(ClientCommand::RequestSessionTimeline {
            session_id: "chat".to_owned(),
            direction: ClientTimelineDirection::Backward,
            cursor: Some(42),
            limit: 64,
            channels: timeline_channels(),
        })
    );
}

#[test]
fn sessions_detail_wheel_up_requests_older_timeline_at_top() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(42, "chat", "user_text", "older")],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(
        app.handle_event(&scroll_up(), now),
        expected_request_older_timeline_action("chat", 42)
    );
}

#[test]
fn sessions_detail_wheel_up_requests_older_when_delta_reaches_top() {
    let area = Rect::new(0, 0, 80, 10);
    let mut section = SessionsSection::new();
    let snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        timeline_message_events(1..=8),
    );

    assert!(matches!(
        section.handle_key(key_event(right_press()), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::RegisterSessionTimelineWatchInterest { .. })
    ));
    assert_eq!(
        section.scroll_detail(Some(&snapshot), area, 100),
        SectionAction::ClientCommand(request_older_timeline_command("chat", 1))
    );
}

#[test]
fn sessions_detail_timeline_loading_suppresses_older_requests() {
    let mut snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(42, "chat", "user_text", "older")],
    );
    snapshot
        .sessions
        .timelines
        .get_mut("chat")
        .expect("chat timeline should exist")
        .load = OperationSnapshot {
        status: "loading",
        error: None,
        key: Some("timeline-load".to_owned()),
    };
    let mut app = open_sessions_app_with(snapshot);
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&scroll_up(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&page_up_press(), now), AppAction::Continue);
}

#[test]
fn sessions_detail_page_and_wheel_scroll_clamp_at_bottom() {
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(42, "chat", "user_text", "older")],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(
        app.handle_event(&page_down_press(), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&scroll_down(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&scroll_up(), now),
        expected_request_older_timeline_action("chat", 42)
    );
}

#[test]
fn sessions_detail_timeline_pins_to_bottom_until_manually_scrolled() {
    let area = Rect::new(0, 0, 80, 10);
    let mut section = SessionsSection::new();
    let snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        timeline_message_events(1..=8),
    );

    assert!(matches!(
        section.handle_key(key_event(right_press()), Some(&snapshot), Some(area)),
        SectionAction::ClientCommand(ClientCommand::RegisterSessionTimelineWatchInterest { .. })
    ));

    let rendered = render_sessions_section(&section, &snapshot, area);
    assert!(rendered.contains("message 8"), "{rendered}");
    assert!(!rendered.contains("message 1"), "{rendered}");

    assert_eq!(
        section.handle_key(key_event(page_up_press()), Some(&snapshot), Some(area)),
        SectionAction::Continue
    );
    let rendered = render_sessions_section(&section, &snapshot, area);
    assert!(rendered.contains("message 1"), "{rendered}");
    assert!(!rendered.contains("message 8"), "{rendered}");

    let updated_snapshot = watched_session_with_timeline(
        session_view("chat", 1),
        timeline_message_events(1..=9),
    );
    let rendered = render_sessions_section(&section, &updated_snapshot, area);
    assert!(!rendered.contains("message 9"), "{rendered}");

    assert_eq!(
        section.handle_key(
            key_event(page_down_press()),
            Some(&updated_snapshot),
            Some(area),
        ),
        SectionAction::Continue
    );
    let rendered = render_sessions_section(&section, &updated_snapshot, area);
    assert!(rendered.contains("message 9"), "{rendered}");
}

fn runtime_editor_snapshot() -> ClientSnapshot {
    let mut session = session_view("chat", 1);
    session.runtime_config = Some(SessionRuntimeConfigView {
        provider: Some(SessionProviderRuntimeView {
            key: "openai".to_owned(),
            kind: ProviderKindView::OpenAiChatCompletions,
            url: "https://api.openai.com/v1".to_owned(),
            model_id: "gpt4o".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
            match_status: RuntimeConfigMatchView::Configured,
        }),
        mcp_servers: vec![
            SessionMcpServerRuntimeView {
                key: "files".to_owned(),
                kind: McpServerTypeView::Stdio,
                command: "files-mcp".to_owned(),
                url: String::new(),
                match_status: RuntimeConfigMatchView::Configured,
            },
            SessionMcpServerRuntimeView {
                key: "ghost".to_owned(),
                kind: McpServerTypeView::Stdio,
                command: "ghost-mcp".to_owned(),
                url: String::new(),
                match_status: RuntimeConfigMatchView::Missing,
            },
        ],
    });
    let mut snapshot = watched_session_with_timeline(session, Vec::new());
    snapshot.config.current = Some(ConfigView {
        etag: "etag-1".to_owned(),
        providers: std::collections::BTreeMap::from([
            (
                "anthropic".to_owned(),
                runtime_provider(
                    ProviderKindView::AnthropicMessages,
                    "Anthropic",
                    "https://api.anthropic.com/v1/messages",
                    "claude",
                ),
            ),
            (
                "openai".to_owned(),
                runtime_provider(
                    ProviderKindView::OpenAiChatCompletions,
                    "OpenAI",
                    "https://api.openai.com/v1",
                    "gpt4o",
                ),
            ),
        ]),
        mcp_servers: std::collections::BTreeMap::from([
            ("files".to_owned(), runtime_mcp_server("files")),
            ("git".to_owned(), runtime_mcp_server("git")),
        ]),
        runtime_defaults: None,
    });
    snapshot.config.load.status = "idle";
    snapshot.config.watch.status = "received";
    snapshot
}

fn active_runtime_editor_snapshot() -> ClientSnapshot {
    let mut snapshot = runtime_editor_snapshot();
    snapshot.sessions.current[0].status.phase = SessionPhaseView::RunningTool;
    snapshot
}

fn runtime_update_snapshot(status: &'static str, error: Option<&str>) -> ClientSnapshot {
    let mut snapshot = runtime_editor_snapshot();
    let session = snapshot.sessions.current[0].clone();
    snapshot.sessions.runtime_config_update = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some("op-runtime".to_owned()),
    };
    snapshot.operations.current.insert(
        "op-runtime".to_owned(),
        ClientOperationView::SessionRuntimeConfigUpdate {
            status,
            error: error.map(str::to_owned),
            session: Some(session),
        },
    );
    snapshot
}

fn runtime_provider(
    kind: ProviderKindView,
    name: &str,
    url: &str,
    model_id: &str,
) -> ProviderView {
    ProviderView {
        kind,
        name: name.to_owned(),
        url: url.to_owned(),
        headers: std::collections::BTreeMap::new(),
        oauth_compatible: false,
        oauth_settings: None,
        models: std::collections::BTreeMap::from([(
            model_id.to_owned(),
            ModelView {
                name: model_id.to_owned(),
                kind: ModelTypeView::Llm,
                options: ModelOptionsView {
                    temperature: None,
                    top_p: None,
                    top_k: None,
                    min_p: None,
                    presence_penalty: None,
                    repetition_penalty: None,
                    reasoning_efforts: vec![
                        "low".to_owned(),
                        "medium".to_owned(),
                        "high".to_owned(),
                    ],
                },
                limits: ModelLimitsView {
                    context: None,
                    output: None,
                },
                capabilities: ModelCapabilitiesView {
                    tools: true,
                    image: false,
                },
                cost: None,
                catalog_match: None,
            },
        )]),
    }
}

fn runtime_mcp_server(name: &str) -> McpServerView {
    McpServerView {
        name: name.to_owned(),
        kind: McpServerTypeView::Stdio,
        command: format!("{name}-mcp"),
        url: String::new(),
    }
}

fn key_event(event: AppEvent) -> KeyEvent {
    let AppEvent::Input(InputEvent::Key(key)) = event else {
        panic!("expected key event");
    };
    key
}

fn expected_request_older_timeline_action(session_id: &str, cursor: u64) -> AppAction {
    AppAction::ClientCommand(request_older_timeline_command(session_id, cursor))
}

fn request_older_timeline_command(session_id: &str, cursor: u64) -> ClientCommand {
    ClientCommand::RequestSessionTimeline {
        session_id: session_id.to_owned(),
        direction: ClientTimelineDirection::Backward,
        cursor: Some(cursor),
        limit: 64,
        channels: timeline_channels(),
    }
}

fn timeline_message_events(range: impl Iterator<Item = u64>) -> Vec<TimelineEventView> {
    range
        .map(|cursor| {
            timeline_text_event(
                cursor,
                "chat",
                "assistant_text",
                &format!("message {cursor}"),
            )
        })
        .collect()
}

fn render_sessions_section(
    section: &SessionsSection,
    snapshot: &ClientSnapshot,
    area: Rect,
) -> String {
    let mut terminal = test_terminal_with_size(area.width, area.height);
    terminal
        .draw(|frame| draw_sessions(frame, area, section, Some(snapshot)))
        .expect("sessions section should render");
    terminal.backend().to_string()
}

#[test]
fn sessions_detail_permission_prompt_allows_once() {
    let payload = r#"{"tool_call":{"name":"shell","arguments":{"command":"pwd"}}}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 1, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    let rendered = render_app(&app);
    assert!(rendered.contains("Permission required"));
    assert!(rendered.contains("Allow once"));
    assert!(!rendered.contains("type a message"));
    assert!(!rendered.contains("provider none"));

    assert_eq!(
        app.handle_event(&char_press('a'), now),
        AppAction::ClientCommand(ClientCommand::RespondExternalRequest {
            session_id: "chat".to_owned(),
            request_id: "req-1".to_owned(),
            response_json: r#"{"decision":"allow_once"}"#.to_owned(),
        })
    );
}

#[test]
fn sessions_detail_permission_prompt_selectable_deny_resume_includes_message() {
    let payload = r#"{"tool_call":{"name":"shell","arguments":{"command":"rm -rf target"}}}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 1, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    for character in "not safe".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    let rendered = render_app(&app);
    assert!(rendered.contains("Deny and resume"));
    assert!(rendered.contains("not safe"));

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::RespondExternalRequest {
            session_id: "chat".to_owned(),
            request_id: "req-1".to_owned(),
            response_json:
                r#"{"decision":"deny","message":"not safe","mode":"return_tool_result"}"#
                    .to_owned(),
        })
    );
}

#[test]
fn sessions_detail_permission_prompt_deny_interrupts() {
    let payload = r#"{"tool_call":{"name":"shell","arguments":{"command":"pwd"}}}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 1, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(
        app.handle_event(&char_press('x'), now),
        AppAction::ClientCommands(vec![
            ClientCommand::RespondExternalRequest {
                session_id: "chat".to_owned(),
                request_id: "req-1".to_owned(),
                response_json: r#"{"decision":"deny","message":null,"mode":"cancel_work"}"#
                    .to_owned(),
            },
            ClientCommand::InterruptSession {
                session_id: "chat".to_owned(),
            },
        ])
    );
}

#[test]
fn sessions_detail_permission_prompt_deny_interrupt_includes_message() {
    let payload = r#"{"tool_call":{"name":"shell","arguments":{"command":"pwd"}}}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 1, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    for character in "stop".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommands(vec![
            ClientCommand::RespondExternalRequest {
                session_id: "chat".to_owned(),
                request_id: "req-1".to_owned(),
                response_json: r#"{"decision":"deny","message":"stop","mode":"cancel_work"}"#
                    .to_owned(),
            },
            ClientCommand::InterruptSession {
                session_id: "chat".to_owned(),
            },
        ])
    );
}

#[test]
fn sessions_detail_question_prompt_submits_selected_answer() {
    let payload = r#"{"questions":[{"header":"Direction","question":"Pick one","options":[{"label":"North","description":"Up"}],"multiple":false,"custom":false}]}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 2, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&char_press(' '), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::RespondExternalRequest {
            session_id: "chat".to_owned(),
            request_id: "req-1".to_owned(),
            response_json: r#"{"answers":[{"header":"Direction","selections":["North"]}]}"#
                .to_owned(),
        })
    );
}

#[test]
fn sessions_detail_question_prompt_switches_multi_question_tabs() {
    let payload = r#"{"questions":[{"header":"Direction","question":"Pick one","options":["North"],"multiple":false,"custom":false},{"header":"Speed","question":"How fast?","options":["Slow","Fast"],"multiple":false,"custom":false}]}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 2, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    let rendered = render_app_with_size(&app, 100, 28);
    assert!(rendered.contains("1 Direction"));
    assert!(rendered.contains("2 Speed"));
    assert!(rendered.contains("Pick one"));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    let rendered = render_app_with_size(&app, 100, 28);
    assert!(rendered.contains("How fast?"));
    assert!(!rendered.contains("Pick one"));

    assert_eq!(app.handle_event(&back_tab_press(), now), AppAction::Continue);
    let rendered = render_app_with_size(&app, 100, 28);
    assert!(rendered.contains("Pick one"));
}

#[test]
fn sessions_detail_question_prompt_custom_response_supports_cursor_editing() {
    let payload = r#"{"questions":[{"header":"Details","question":"Anything else?","options":[],"multiple":false,"custom":true}]}"#;
    let mut app = open_sessions_app_with(watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 2, payload,
        )],
    ));
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    for character in "helo".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert_eq!(app.handle_event(&left_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('l'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&home_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('>'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&end_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('!'), now), AppAction::Continue);

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::RespondExternalRequest {
            session_id: "chat".to_owned(),
            request_id: "req-1".to_owned(),
            response_json:
                r#"{"answers":[{"custom":">hello!","header":"Details","selections":[]}]}"#
                    .to_owned(),
        })
    );
}

#[test]
fn sessions_detail_important_status_prioritizes_watch_error() {
    let session = session_with_phase("chat", 1, SessionPhaseView::Failed, "model failed");
    let mut snapshot = watched_session_with_timeline(session, Vec::new());
    snapshot
        .sessions
        .timelines
        .get_mut("chat")
        .expect("timeline should exist")
        .watch
        .error = Some("watch failed".to_owned());
    let mut app = open_sessions_app_with(snapshot);

    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("chat")
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("watch failed"));
    assert!(!rendered.contains("model failed"));
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
        app.handle_event(&ctrl_x_press(), Instant::now()),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(&char_press('d'), Instant::now()),
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

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('d'), now), AppAction::Continue);
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

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('d'), now), AppAction::Continue);
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
