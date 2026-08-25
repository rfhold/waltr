fn routine_screen_snapshot() -> ClientSnapshot {
    let mut snapshot = ready_snapshot();
    snapshot.routines.definitions.watch.status = "received";
    snapshot.routines.definitions.digest = "definitions-1".to_owned();
    snapshot.routines.definitions.definitions = vec![
        routine_definition(
            "branch",
            "Branch work",
            "Continue in another session",
            Vec::new(),
        ),
        routine_definition("archive", "Archive", "Store completed work", Vec::new()),
    ];
    snapshot.routines.history.watch.status = "received";

    let mut manual = routine_detail(
        "manual-invocation",
        ClientRoutineInvocationStatus::Running,
        Vec::new(),
    );
    manual.summary.admission_sequence = 8;
    manual.summary.routine_name = "branch".to_owned();
    manual.tasks = vec![
        ClientRoutineTask {
            name: "create".to_owned(),
            r#type: "sessionCreate".to_owned(),
            status: ClientRoutineTaskStatus::Succeeded,
            results: vec![ClientRoutineResult {
                name: "created".to_owned(),
                value: ClientRoutineValue::Boolean(true),
            }],
            session_references: BTreeMap::from([(
                "destination".to_owned(),
                "session-safe".to_owned(),
            )]),
            error: None,
        },
        ClientRoutineTask {
            name: "notify".to_owned(),
            r#type: "message".to_owned(),
            status: ClientRoutineTaskStatus::Running,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        },
    ];

    let mut event = routine_detail(
        "event-invocation",
        ClientRoutineInvocationStatus::Failed,
        Vec::new(),
    );
    event.summary.admission_sequence = 7;
    event.summary.trigger_kind = ClientRoutineTriggerKind::Event;
    event.summary.metadata.clear();
    event.summary.source_cursor = Some("event-42".to_owned());
    event.summary.terminal_error = Some(ClientRoutineFailure {
        code: "failed".to_owned(),
        message: "safe failure\nwithout control leaks".to_owned(),
    });
    event.tasks[0].status = ClientRoutineTaskStatus::Blocked;
    event.tasks[0].error = Some(ClientRoutineFailure {
        code: "blocked".to_owned(),
        message: "upstream failed".to_owned(),
    });

    snapshot.routines.history.recent = vec![manual.summary.clone(), event.summary.clone()];
    snapshot.routines.history.older_page_token = Some(7);
    snapshot.routines.details.insert(
        manual.summary.invocation_id.clone(),
        RoutineDetailSnapshot {
            status: "ready",
            detail: Some(manual),
            error: None,
        },
    );
    snapshot.routines.details.insert(
        event.summary.invocation_id.clone(),
        RoutineDetailSnapshot {
            status: "ready",
            detail: Some(event),
            error: None,
        },
    );
    snapshot
}

fn open_routines_app(snapshot: ClientSnapshot) -> App {
    let mut app = App::new();
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('u'), now),
        AppAction::ClientCommands(vec![
            ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ClientCommand::RegisterRoutineInvocationWatchInterest,
        ])
    );
    app
}

#[test]
fn routines_route_registers_and_escape_releases_both_watches() {
    let mut app = open_routines_app(ready_snapshot());
    let now = Instant::now();
    assert_eq!(app.screen(), AppScreen::Routines);
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseRoutineDefinitionWatchInterest,
            ClientCommand::ReleaseRoutineInvocationWatchInterest,
        ])
    );
    assert_eq!(app.screen(), AppScreen::Landing);
}

#[test]
fn routines_route_releases_session_ownership_and_does_not_reuse_palette_chord() {
    let mut app =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("source", 1)]));
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("source")
    );
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(
        app.handle_event(&char_press('u'), now),
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseSessionTimelineWatchInterest {
                session_id: "source".to_owned(),
            },
            ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
                session_id: "source".to_owned(),
            },
            ClientCommand::ReleaseSessionWatchInterest,
            ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ClientCommand::RegisterRoutineInvocationWatchInterest,
        ])
    );
    assert_eq!(app.screen(), AppScreen::Routines);

    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('r'), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Routines);
}

#[test]
fn routines_list_and_detail_are_separate_read_only_views() {
    let mut app = open_routines_app(routine_screen_snapshot());
    let now = Instant::now();
    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Branch work"), "{rendered}");
    assert!(rendered.contains("manual"), "{rendered}");
    assert!(rendered.contains("event"), "{rendered}");
    assert!(rendered.contains("Manual definitions"), "{rendered}");
    assert!(rendered.contains("Recent invocations"), "{rendered}");
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("Manual definitions") && line.contains("Recent invocations")),
        "{rendered}"
    );
    assert!(!rendered.contains("[Succeeded] create"), "{rendered}");
    assert!(!rendered.contains(['┌', '┐', '└', '┘', '│']), "{rendered}");
    assert!(!rendered.contains("Run Branch work"), "{rendered}");
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);

    drop(app.handle_event(&tab_press(), now));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Routine invocation"), "{rendered}");
    assert!(rendered.contains("[Succeeded] create"), "{rendered}");
    assert!(rendered.contains("source session source"), "{rendered}");
    assert!(rendered.contains("destination: session-safe"), "{rendered}");
    assert!(!rendered.contains("Manual definitions"), "{rendered}");
    assert!(!rendered.contains("Recent invocations"), "{rendered}");
    assert!(!rendered.contains(['┌', '┐', '└', '┘', '│']), "{rendered}");
    insta::assert_snapshot!("routines_history_detail", rendered);

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Routines);
    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Manual definitions"), "{rendered}");
    assert!(rendered.contains("Recent invocations"), "{rendered}");
}

#[test]
fn routines_history_selection_survives_updates_and_loads_nearby_detail_and_older_page() {
    let snapshot = routine_screen_snapshot();
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&down_press(), now));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("event-invocation"));

    let mut removed = snapshot;
    removed.routines.history.recent.remove(1);
    removed.routines.details.remove("event-invocation");
    drop(app.handle_event(&AppEvent::Client(Box::new(removed)), now));
    assert!(render_app(&app).contains("event-invocation"));
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("manual-invocation"));
    assert!(matches!(
        app.handle_event(&end_press(), now),
        AppAction::ClientCommand(ClientCommand::LoadOlderRoutineInvocations { page_size: 50, .. })
    ));
}

#[test]
fn routines_enter_opens_detail_and_loads_it_when_needed() {
    let mut snapshot = routine_screen_snapshot();
    snapshot.routines.details.remove("manual-invocation");
    let mut app = open_routines_app(snapshot);
    let now = Instant::now();

    drop(app.handle_event(&tab_press(), now));
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::LoadRoutineInvocationDetail {
            invocation_id,
            ..
        }) if invocation_id == "manual-invocation"
    ));
    let rendered = render_app(&app);
    assert!(rendered.contains("Routine invocation"), "{rendered}");
    assert!(rendered.contains("Enter load task detail"), "{rendered}");
    assert!(!rendered.contains("Manual definitions"), "{rendered}");

    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Routines);
}

#[test]
fn routines_watch_loading_reconnecting_errors_and_narrow_layout_remain_useful() {
    let mut snapshot = ready_snapshot();
    snapshot.routines.definitions.watch.status = "failed";
    snapshot.routines.definitions.watch.error = Some("definition offline".to_owned());
    snapshot.routines.history.watch.status = "waiting";
    snapshot.routines.history.watch.error = Some("history reconnect".to_owned());
    let app = open_routines_app(snapshot);
    let rendered = render_app_with_size(&app, 48, 20);
    assert!(rendered.contains("definition offline"), "{rendered}");
    assert!(rendered.contains("Reconnecting definitions"), "{rendered}");
    assert!(rendered.contains("history reconnect"), "{rendered}");
    assert!(rendered.contains("Recent invocations"), "{rendered}");
    assert!(!rendered.lines().any(|line| {
        line.contains("Manual definitions") && line.contains("Recent invocations")
    }));
    assert!(!rendered.contains("Routine invocation"), "{rendered}");
    assert!(!rendered.contains(['┌', '┐', '└', '┘', '│']), "{rendered}");
    insta::assert_snapshot!("routines_reconnecting_narrow", rendered);
}

#[test]
fn routines_reconnect_keeps_retained_definitions_history_and_selection_visible() {
    let mut snapshot = routine_screen_snapshot();
    snapshot.routines.definitions.watch.status = "failed";
    snapshot.routines.definitions.watch.error = Some("definition reconnect".to_owned());
    snapshot.routines.history.watch.status = "waiting";
    snapshot.routines.history.watch.error = Some("history reconnect".to_owned());
    let mut app = open_routines_app(snapshot);
    let now = Instant::now();

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("definition reconnect"), "{rendered}");
    assert!(rendered.contains("Branch work"), "{rendered}");
    assert!(rendered.contains("history reconnect"), "{rendered}");
    assert!(rendered.contains("manual-invocation"), "{rendered}");

    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&down_press(), now));
    assert!(render_app_with_size(&app, 100, 30).contains("event-invocation"));
}

#[test]
fn routines_overflow_lists_keep_selected_rows_visible_on_short_narrow_terminals() {
    let mut snapshot = ready_snapshot();
    snapshot.routines.definitions.watch.status = "received";
    snapshot.routines.definitions.definitions = (0..12)
        .map(|index| {
            routine_definition(
                &format!("definition-{index:02}"),
                &format!("Definition {index:02}"),
                &format!("Description {index:02}"),
                Vec::new(),
            )
        })
        .collect();
    snapshot.routines.history.watch.status = "received";
    snapshot.routines.history.recent = (0..12)
        .map(|index| {
            let mut detail = routine_detail(
                &format!("invocation-{index:02}"),
                ClientRoutineInvocationStatus::Succeeded,
                Vec::new(),
            );
            detail.summary.routine_name = format!("routine-{index:02}");
            detail.summary
        })
        .collect();
    let mut app = open_routines_app(snapshot);
    let now = Instant::now();

    for _ in 0..11 {
        drop(app.handle_event(&down_press(), now));
    }
    let rendered = render_app_with_size(&app, 48, 16);
    assert!(rendered.contains("> Definition 11"), "{rendered}");
    assert!(!rendered.contains("Definition 00"), "{rendered}");

    drop(app.handle_event(&tab_press(), now));
    for _ in 0..11 {
        drop(app.handle_event(&down_press(), now));
    }
    let rendered = render_app_with_size(&app, 48, 16);
    assert!(rendered.contains("> routine-11"), "{rendered}");
    assert!(rendered.contains("invocation-11"), "{rendered}");
    assert!(!rendered.contains("invocation-00"), "{rendered}");
}

#[test]
fn routines_detail_scroll_reaches_all_tasks_and_results_on_short_narrow_terminals() {
    let mut snapshot = routine_screen_snapshot();
    let detail = snapshot
        .routines
        .details
        .get_mut("manual-invocation")
        .and_then(|detail| detail.detail.as_mut())
        .expect("manual detail");
    detail.tasks = (0..18)
        .map(|index| ClientRoutineTask {
            name: format!("task-{index:02}"),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Succeeded,
            results: vec![ClientRoutineResult {
                name: format!("task-result-{index:02}"),
                value: ClientRoutineValue::Boolean(true),
            }],
            session_references: BTreeMap::new(),
            error: None,
        })
        .collect();
    detail.results = vec![ClientRoutineResult {
        name: "final-result".to_owned(),
        value: ClientRoutineValue::String("complete".to_owned()),
    }];
    let mut app = open_routines_app(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&enter_press(), now));

    let rendered = render_app_with_size(&app, 48, 14);
    assert!(rendered.contains("task-00"), "{rendered}");
    assert!(!rendered.contains("task-17"), "{rendered}");

    drop(app.handle_event(&page_down_press(), now));
    let rendered = render_app_with_size(&app, 48, 14);
    assert!(rendered.contains("task-02"), "{rendered}");
    assert!(!rendered.contains("id manual-invocation"), "{rendered}");

    drop(app.handle_event(&end_press(), now));
    let rendered = render_app_with_size(&app, 48, 14);
    assert!(rendered.contains("task-17"), "{rendered}");
    assert!(rendered.contains("final-result: complete"), "{rendered}");

    drop(app.handle_event(&home_press(), now));
    let rendered = render_app_with_size(&app, 48, 14);
    assert!(rendered.contains("admission sequence 8"), "{rendered}");
    assert!(!rendered.contains("task-17"), "{rendered}");
}
