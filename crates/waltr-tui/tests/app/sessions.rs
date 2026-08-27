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
fn sessions_use_metadata_title_only_in_list() {
    let mut session = session_view("stable-id", 1);
    session
        .metadata
        .insert("title".to_owned(), serde_json::json!("Initial title"));
    let mut app = open_sessions_app_with(watched_session_with_timeline(session, Vec::new()));
    let now = Instant::now();

    let rendered = render_app(&app);
    assert!(rendered.contains("Initial title"), "{rendered}");
    assert!(!rendered.contains("stable-id"), "{rendered}");
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("stable-id")
    );
    assert_eq!(
        app.sessions_section().detail_session_id(),
        Some("stable-id")
    );

    let mut updated = session_view("stable-id", 2);
    updated
        .metadata
        .insert("title".to_owned(), serde_json::json!("Live title"));
    drop(app.handle_event(
        &AppEvent::Client(Box::new(watched_session_with_timeline(updated, Vec::new()))),
        now,
    ));

    let rendered = render_app(&app);
    assert!(!rendered.contains("Live title"), "{rendered}");
    assert!(!rendered.contains("stable-id"), "{rendered}");
    assert_eq!(
        app.sessions_section().detail_session_id(),
        Some("stable-id")
    );
}

#[test]
fn sessions_create_dispatches_and_completion_opens_detail() {
    let mut app = open_sessions_app_with(with_agents(
        watched_sessions_snapshot(Vec::new()),
        "reviewer",
        &[("builder", None), ("reviewer", Some("Reviews changes"))],
    ));
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('s'), now), AppAction::Continue);
    let rendered = render_app_with_size(&app, 100, 24);
    assert!(rendered.contains("Create session"));
    assert!(rendered.contains("reviewer  default"));
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::CreateSession {
            agent_name: "reviewer".to_owned(),
        })
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
    assert!(rendered.contains("agent default (C-x a)"));
    assert!(rendered.contains("provider none (C-x p)"));
    assert!(rendered.contains("Context  0"));
}

#[test]
fn sessions_create_selects_alternate_and_escape_cancels() {
    let snapshot = with_agents(
        watched_sessions_snapshot(Vec::new()),
        "builder",
        &[("builder", None), ("reviewer", None)],
    );
    let mut app = open_sessions_app_with(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('s'), now), AppAction::Continue);
    assert_eq!(app.handle_event(&down_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::CreateSession {
            agent_name: "reviewer".to_owned(),
        })
    );

    let mut app = open_sessions_app_with(with_agents(
        watched_sessions_snapshot(Vec::new()),
        "builder",
        &[("builder", None)],
    ));
    drop(app.handle_event(&ctrl_x_press(), now));
    drop(app.handle_event(&char_press('s'), now));
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert!(!render_app(&app).contains("Choose the exact agent"));
}

#[test]
fn sessions_agent_catalog_loading_error_empty_and_retry_are_actionable() {
    let now = Instant::now();
    let mut app = open_sessions_app_with(watched_sessions_snapshot(Vec::new()));
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::RequestSessionAgents)
    );
    let rendered = render_app(&app);
    assert!(rendered.contains("Loading agents"), "{rendered}");

    let mut failed = watched_sessions_snapshot(Vec::new());
    failed.sessions.agents.load.status = "failed";
    failed.sessions.agents.load.error = Some("catalog unavailable".to_owned());
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    let rendered = render_app(&app);
    assert!(rendered.contains("catalog unavailable"));
    assert_eq!(
        app.handle_event(&char_press('r'), now),
        AppAction::ClientCommand(ClientCommand::RequestSessionAgents)
    );

    let empty = with_agents(watched_sessions_snapshot(Vec::new()), "", &[]);
    drop(app.handle_event(&AppEvent::Client(Box::new(empty)), now));
    assert!(render_app(&app).contains("No agents are available"));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
}

#[test]
fn sessions_switch_agent_success_failure_and_stale_operation() {
    let mut session = session_view("chat", 1);
    session.agent_name = "builder".to_owned();
    let base = with_agents(
        watched_session_with_timeline(session, Vec::new()),
        "builder",
        &[("builder", None), ("reviewer", None)],
    );
    let mut app = open_sessions_app_with(base.clone());
    let now = Instant::now();
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(app.handle_event(&char_press('a'), now), AppAction::Continue);
    drop(app.handle_event(&down_press(), now));
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionAgent {
            id: "chat".to_owned(),
            agent_name: "reviewer".to_owned(),
        })
    );

    let stale = session_agent_update_snapshot(base.clone(), "stale", "ready", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(stale)), now));
    assert!(render_app(&app).contains("Switch agent"));
    let loading = session_agent_update_snapshot(base.clone(), "switch", "loading", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
    assert!(render_app(&app).contains("Saving"));
    let failed = session_agent_update_snapshot(base.clone(), "switch", "failed", Some("denied"));
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    assert!(render_app(&app).contains("denied"));
    assert!(render_app(&app).contains("Switch agent"));

    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionAgent { .. })
    ));
    let loading = session_agent_update_snapshot(base.clone(), "switch-2", "loading", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
    let ready = session_agent_update_snapshot(base, "switch-2", "ready", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(ready)), now));
    assert!(!render_app(&app).contains("Switch agent"));
}

#[test]
fn sessions_active_agent_switch_explains_next_cycle_and_escape_focus() {
    let mut session = session_with_phase("chat", 1, SessionPhaseView::StreamingProvider, "");
    session.agent_name = "builder".to_owned();
    let snapshot = with_agents(
        watched_session_with_timeline(session, Vec::new()),
        "builder",
        &[("builder", None), ("reviewer", None)],
    );
    let mut app = open_sessions_app_with(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&ctrl_x_press(), now));
    drop(app.handle_event(&char_press('a'), now));
    let rendered = render_app_with_size(&app, 70, 18);
    assert!(rendered.contains("Active work continues"), "{rendered}");
    assert!(rendered.contains("applies next cycle"), "{rendered}");
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert!(!app.session_interrupt_armed());
}

fn routine_definition(
    routine_name: &str,
    display_name: &str,
    description: &str,
    inputs: Vec<ClientRoutineInputDefinition>,
) -> ClientManualRoutineDefinition {
    ClientManualRoutineDefinition {
        routine_name: routine_name.to_owned(),
        trigger_name: "manual".to_owned(),
        display_name: display_name.to_owned(),
        description: description.to_owned(),
        scope: ClientRoutineScope::Session,
        inputs,
        results: vec![ClientRoutineResultDefinition {
            name: "session-id".to_owned(),
            r#type: ClientRoutineValueType::String,
        }],
        routine_revision: "revision-1".to_owned(),
    }
}

fn string_routine_input(name: &str, required: bool) -> ClientRoutineInputDefinition {
    ClientRoutineInputDefinition {
        name: name.to_owned(),
        r#type: ClientRoutineValueType::String,
        required,
        default_value: None,
        minimum_string_length: None,
        string_pattern: None,
    }
}

fn boolean_routine_input(name: &str) -> ClientRoutineInputDefinition {
    ClientRoutineInputDefinition {
        name: name.to_owned(),
        r#type: ClientRoutineValueType::Boolean,
        required: false,
        default_value: None,
        minimum_string_length: None,
        string_pattern: None,
    }
}

fn routine_catalog_snapshot(
    session_id: &str,
    status: &'static str,
    definitions: Vec<ClientManualRoutineDefinition>,
    error: Option<&str>,
) -> ClientSnapshot {
    let mut snapshot = watched_session_with_timeline(session_view(session_id, 1), Vec::new());
    snapshot.routines.definitions.watch.status = if status == "ready" {
        "received"
    } else {
        status
    };
    snapshot.routines.definitions.watch.error = error.map(str::to_owned);
    snapshot.routines.definitions.definitions = definitions;
    snapshot
}

#[allow(
    clippy::too_many_arguments,
    reason = "the fixture exposes every keyed invocation fact varied by palette tests"
)]
fn routine_invocation_snapshot(
    mut snapshot: ClientSnapshot,
    operation_key: &str,
    request_id: &str,
    session_id: &str,
    status: &'static str,
    validation_failure: Option<ClientRoutineValidationFailure>,
    error: Option<&str>,
    results: Vec<ClientRoutineResult>,
) -> ClientSnapshot {
    let invocation_id = "server-invocation".to_owned();
    snapshot.routines.starts.insert(
        operation_key.to_owned(),
        RoutineStartOperationView {
            operation_key: operation_key.to_owned(),
            request_id: request_id.to_owned(),
            routine_name: "generic".to_owned(),
            trigger_name: "manual".to_owned(),
            source_session_id: session_id.to_owned(),
            inputs: BTreeMap::new(),
            status,
            invocation_id: (status == "ready").then_some(invocation_id.clone()),
            validation_failure,
            error: error.map(str::to_owned),
        },
    );
    if status == "ready" {
        let detail = routine_detail(
            &invocation_id,
            ClientRoutineInvocationStatus::Succeeded,
            results,
        );
        snapshot.routines.history.watch.status = "received";
        snapshot.routines.history.recent = vec![detail.summary.clone()];
        snapshot.routines.session_histories.insert(
            session_id.to_owned(),
            snapshot.routines.history.clone(),
        );
        snapshot.routines.details.insert(
            invocation_id,
            RoutineDetailSnapshot {
                status: "ready",
                detail: Some(detail),
                error: None,
            },
        );
    }
    snapshot
}

fn routine_detail(
    invocation_id: &str,
    status: ClientRoutineInvocationStatus,
    results: Vec<ClientRoutineResult>,
) -> ClientRoutineInvocationDetail {
    ClientRoutineInvocationDetail {
        summary: ClientRoutineInvocationSummary {
            admission_sequence: 1,
            invocation_id: invocation_id.to_owned(),
            routine_name: "generic".to_owned(),
            routine_revision: "revision-1".to_owned(),
            trigger_name: "manual".to_owned(),
            trigger_kind: ClientRoutineTriggerKind::Manual,
            correlation_id: "request".to_owned(),
            metadata: BTreeMap::from([(
                waltr_client::ROUTINE_SOURCE_SESSION_ID_METADATA_KEY.to_owned(),
                "source".to_owned(),
            )]),
            source_cursor: None,
            status,
            admitted_at_ms: 1,
            started_at_ms: Some(2),
            completed_at_ms: status.is_terminal().then_some(3),
            updated_at_ms: 3,
            terminal_error: None,
            latest_sequence: 3,
        },
        tasks: vec![ClientRoutineTask {
            name: "create".to_owned(),
            r#type: "sessionCreate".to_owned(),
            status: if status.is_terminal() {
                ClientRoutineTaskStatus::Succeeded
            } else {
                ClientRoutineTaskStatus::Running
            },
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        }],
        results,
    }
}

fn open_routine_palette(app: &mut App, now: Instant) -> AppAction {
    drop(app.handle_event(&right_press(), now));
    app.handle_event(&ctrl_p_press(), now)
}

fn invoke_selected_string_routine(
    app: &mut App,
    now: Instant,
    first_line: &str,
    second_line: &str,
) -> AppAction {
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    for character in first_line.chars() {
        drop(app.handle_event(&char_press(character), now));
    }
    drop(app.handle_event(&enter_press(), now));
    for character in second_line.chars() {
        drop(app.handle_event(&char_press(character), now));
    }
    drop(app.handle_event(&tab_press(), now));
    app.handle_event(&enter_press(), now)
}

#[test]
fn session_commands_ctrl_p_is_gated_to_unowned_session_detail_and_old_chord_is_removed() {
    let now = Instant::now();
    let mut landing = App::new();
    drop(landing.handle_event(&AppEvent::Client(Box::new(ready_snapshot())), now));
    assert_eq!(landing.handle_event(&ctrl_p_press(), now), AppAction::Continue);

    let snapshot = routine_catalog_snapshot("source", "ready", Vec::new(), None);
    let mut list = open_sessions_app_with(snapshot.clone());
    assert_eq!(list.handle_event(&ctrl_p_press(), now), AppAction::Continue);
    assert!(!render_app(&list).contains("Session commands"));

    let mut detail = open_sessions_app_with(snapshot);
    assert!(matches!(
        open_routine_palette(&mut detail, now),
        AppAction::ClientCommands(_)
    ));
    assert!(render_app(&detail).contains("Session commands"));

    drop(detail.handle_event(&escape_press(), now));
    drop(detail.handle_event(&ctrl_x_press(), now));
    assert_eq!(detail.handle_event(&char_press('r'), now), AppAction::Continue);
    assert!(!render_app(&detail).contains("Session commands"));

    drop(detail.handle_event(&ctrl_x_press(), now));
    drop(detail.handle_event(&char_press('p'), now));
    assert_eq!(detail.handle_event(&ctrl_p_press(), now), AppAction::Continue);
    assert!(!render_app(&detail).contains("Session commands"));
}

#[test]
fn session_commands_definition_watch_loading_empty_and_failure() {
    let now = Instant::now();
    let base = watched_session_with_timeline(session_view("source", 1), Vec::new());
    let mut app = open_sessions_app_with(base);
    assert!(matches!(
        open_routine_palette(&mut app, now),
        AppAction::ClientCommands(_)
    ));
    assert!(render_app(&app).contains("Loading session commands..."));

    let failed = routine_catalog_snapshot("source", "failed", Vec::new(), Some("offline"));
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    assert!(render_app(&app).contains("offline"));
    assert_eq!(app.handle_event(&char_press('r'), now), AppAction::Continue);

    let retained = routine_catalog_snapshot(
        "source",
        "failed",
        vec![routine_definition(
            "generic",
            "Retained command",
            "Still available while reconnecting",
            Vec::new(),
        )],
        Some("reconnecting"),
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(retained)), now),
        AppAction::Continue
    );
    let rendered = render_app(&app);
    assert!(rendered.contains("reconnecting"), "{rendered}");
    assert!(rendered.contains("Retained command"), "{rendered}");
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            routine_name,
            ..
        }) if routine_name == "generic"
    ));

    let mut app = open_sessions_app_with(routine_catalog_snapshot(
        "source",
        "ready",
        Vec::new(),
        None,
    ));
    drop(open_routine_palette(&mut app, now));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);

    let empty = routine_catalog_snapshot("source", "ready", Vec::new(), None);
    drop(app.handle_event(&AppEvent::Client(Box::new(empty)), now));
    assert!(render_app(&app).contains("No session commands available."));
}

#[test]
fn session_commands_searches_dynamic_definitions_and_toggles_boolean() {
    let definitions = vec![
        routine_definition("archive", "Archive", "Store this conversation", Vec::new()),
        routine_definition(
            "generic",
            "Branch work",
            "Continue in another session",
            vec![boolean_routine_input("include-context")],
        ),
    ];
    let mut app = open_sessions_app_with(routine_catalog_snapshot(
        "source",
        "ready",
        definitions,
        None,
    ));
    let now = Instant::now();
    assert!(matches!(
        open_routine_palette(&mut app, now),
        AppAction::ClientCommands(_)
    ));
    for character in "another".chars() {
        drop(app.handle_event(&char_press(character), now));
    }
    let rendered = render_app(&app);
    assert!(rendered.contains("Branch work"), "{rendered}");
    assert!(!rendered.contains("Archive"), "{rendered}");
    drop(app.handle_event(&enter_press(), now));
    assert!(render_app(&app).contains("[ ] false"));
    drop(app.handle_event(&char_press(' '), now));
    drop(app.handle_event(&tab_press(), now));
    let action = app.handle_event(&enter_press(), now);
    assert!(matches!(
        action,
        AppAction::ClientCommand(ClientCommand::StartManualRoutine { inputs, .. })
            if inputs.get("include-context") == Some(&ClientRoutineValue::Boolean(true))
    ));
}

#[test]
fn session_commands_initialize_defaults_and_preserve_edits() {
    let mut input = boolean_routine_input("continue");
    input.default_value = Some(ClientRoutineValue::Boolean(true));
    let definition = routine_definition(
        "generic",
        "Compact context",
        "Compact and optionally continue",
        vec![input],
    );
    let mut app = open_sessions_app_with(routine_catalog_snapshot(
        "source",
        "ready",
        vec![definition],
        None,
    ));
    let now = Instant::now();
    drop(open_routine_palette(&mut app, now));
    drop(app.handle_event(&enter_press(), now));
    assert!(render_app(&app).contains("[x] true"));
    drop(app.handle_event(&char_press(' '), now));
    drop(app.handle_event(&tab_press(), now));
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::StartManualRoutine { inputs, .. })
            if inputs.get("continue") == Some(&ClientRoutineValue::Boolean(false))
    ));
}

#[test]
fn session_commands_validate_and_submit_exact_multiline_bytes() {
    let definition = routine_definition(
        "generic",
        "Generic command",
        "Runs declared work",
        vec![string_routine_input("direction", true)],
    );
    let mut app = open_sessions_app_with(routine_catalog_snapshot(
        "source",
        "ready",
        vec![definition],
        None,
    ));
    let now = Instant::now();
    assert!(matches!(
        open_routine_palette(&mut app, now),
        AppAction::ClientCommands(_)
    ));
    drop(app.handle_event(&enter_press(), now));
    drop(app.handle_event(&char_press(' '), now));
    drop(app.handle_event(&tab_press(), now));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("Required."));
    drop(app.handle_event(&back_tab_press(), now));
    drop(app.handle_event(&end_press(), now));
    drop(app.handle_event(&char_press('a'), now));
    drop(app.handle_event(&enter_press(), now));
    drop(app.handle_event(&char_press('b'), now));
    drop(app.handle_event(&char_press(' '), now));
    drop(app.handle_event(&tab_press(), now));
    let action = app.handle_event(&enter_press(), now);
    assert!(matches!(
        action,
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            source_session_id: session_id,
            inputs,
            ..
        }) if session_id == "source"
            && inputs.get("direction")
                == Some(&ClientRoutineValue::String(" a\nb ".to_owned()))
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario proves the complete keyed invocation lifecycle"
)]
fn session_commands_pending_stale_failure_retry_and_result_are_isolated() {
    let definition = routine_definition(
        "generic",
        "Generic command",
        "Runs declared work",
        vec![string_routine_input("direction", true)],
    );
    let base = routine_catalog_snapshot("source", "ready", vec![definition], None);
    let mut app = open_sessions_app_with(base.clone());
    let now = Instant::now();
    assert!(matches!(
        open_routine_palette(&mut app, now),
        AppAction::ClientCommands(_)
    ));
    let first = invoke_selected_string_routine(&mut app, now, "keep", "draft");
    let (first_key, first_request) = match first {
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            ..
        }) => (operation_key, request_id),
        action => panic!("expected invocation, saw {action:?}"),
    };
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&AppEvent::Input(InputEvent::CtrlC), now),
        AppAction::Continue
    );
    assert!(render_app(&app).contains("Running command..."));

    let stale = routine_invocation_snapshot(
        base.clone(),
        "foreign-key",
        "foreign-request",
        "other-source",
        "ready",
        None,
        None,
        vec![ClientRoutineResult {
            name: "session-id".to_owned(),
            value: ClientRoutineValue::String("foreign-result".to_owned()),
        }],
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(stale)), now));
    assert!(render_app(&app).contains("Running command..."));

    let failed = routine_invocation_snapshot(
        base.clone(),
        &first_key,
        &first_request,
        "source",
        "failed",
        Some(ClientRoutineValidationFailure {
            message: "Check command inputs.".to_owned(),
            violations: vec![ClientRoutineFieldViolation {
                field_path: "inputs.direction".to_owned(),
                message: "Direction was rejected.".to_owned(),
            }],
        }),
        None,
        Vec::new(),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    let rendered = render_app(&app);
    assert!(rendered.contains("keep\\ndraft"), "{rendered}");
    assert!(rendered.contains("Direction was rejected."), "{rendered}");
    let retry = app.handle_event(&enter_press(), now);
    let (retry_key, retry_request) = match retry {
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            ..
        }) => (operation_key, request_id),
        action => panic!("expected retry, saw {action:?}"),
    };
    assert_ne!(retry_key, first_key);
    assert_ne!(retry_request, first_request);

    let ready = routine_invocation_snapshot(
        base,
        &retry_key,
        &retry_request,
        "source",
        "ready",
        None,
        None,
        vec![ClientRoutineResult {
            name: "session-id".to_owned(),
            value: ClientRoutineValue::String("destination".to_owned()),
        }],
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(ready)), now));
    assert_eq!(app.sessions_section().detail_session_id(), Some("source"));
    let rendered = render_app(&app);
    assert!(rendered.contains("Command accepted"), "{rendered}");
    assert!(rendered.contains("Invocation server-invocation"), "{rendered}");
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseRoutineDefinitionWatchInterest)
    );
    assert!(!render_app(&app).contains("Session commands"));
    assert_eq!(app.sessions_section().detail_session_id(), Some("source"));
}

#[test]
fn session_commands_navigates_exact_accepted_id_and_loads_detail_outside_recent_history() {
    let definition = routine_definition(
        "generic",
        "Generic command",
        "Runs declared work",
        Vec::new(),
    );
    let base = routine_catalog_snapshot("source", "ready", vec![definition], None);
    let mut app = open_sessions_app_with(base.clone());
    let now = Instant::now();
    drop(open_routine_palette(&mut app, now));
    drop(app.handle_event(&enter_press(), now));
    let action = app.handle_event(&enter_press(), now);
    let (operation_key, request_id) = match action {
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            ..
        }) => (operation_key, request_id),
        action => panic!("expected durable start, saw {action:?}"),
    };

    let mut changed = base.clone();
    changed.routines.definitions.definitions = vec![routine_definition(
        "generic",
        "Changed live command",
        "A replacement must not mutate the open form",
        Vec::new(),
    )];
    changed.routines.starts.insert(
        operation_key.clone(),
        RoutineStartOperationView {
            operation_key: operation_key.clone(),
            request_id: request_id.clone(),
            routine_name: "generic".to_owned(),
            trigger_name: "manual".to_owned(),
            source_session_id: "source".to_owned(),
            inputs: BTreeMap::new(),
            status: "loading",
            invocation_id: None,
            validation_failure: None,
            error: None,
        },
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(changed)), now),
        AppAction::Continue
    );
    let rendered = render_app(&app);
    assert!(rendered.contains("Generic command"), "{rendered}");
    assert!(!rendered.contains("Changed live command"), "{rendered}");

    let mut running = base.clone();
    running.routines.definitions.definitions.clear();
    running.routines.starts.insert(
        operation_key.clone(),
        RoutineStartOperationView {
            operation_key: operation_key.clone(),
            request_id: request_id.clone(),
            routine_name: "generic".to_owned(),
            trigger_name: "manual".to_owned(),
            source_session_id: "source".to_owned(),
            inputs: BTreeMap::new(),
            status: "ready",
            invocation_id: Some("server-issued-id".to_owned()),
            validation_failure: None,
            error: None,
        },
    );
    let mut detail = routine_detail(
        "server-issued-id",
        ClientRoutineInvocationStatus::Running,
        Vec::new(),
    );
    detail.tasks = vec![
        ClientRoutineTask {
            name: "pending".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Pending,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        },
        ClientRoutineTask {
            name: "active".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Running,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        },
    ];
    running.routines.history.watch.status = "received";
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(running.clone())), now),
        AppAction::Continue
    );
    let rendered = render_app(&app);
    assert!(
        rendered.contains("Invocation server-issued-id"),
        "{rendered}"
    );
    let action = app.handle_event(&enter_press(), now);
    let initial_detail_request = match &action {
        AppAction::ClientCommands(commands) => match commands.last() {
            Some(ClientCommand::LoadRoutineInvocationDetail { request_id, .. }) => {
                request_id.clone()
            }
            command => panic!("expected initial detail request, saw {command:?}"),
        },
        action => panic!("expected navigation commands, saw {action:?}"),
    };
    assert!(matches!(
        &action,
        AppAction::ClientCommands(commands)
            if commands.len() == 7
                && commands[0] == ClientCommand::ReleaseRoutineDefinitionWatchInterest
                && commands[1] == ClientCommand::ReleaseSessionTimelineWatchInterest {
                    session_id: "source".to_owned(),
                }
                && commands[2] == ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
                    session_id: "source".to_owned(),
                }
                && commands[3] == ClientCommand::ReleaseSessionWatchInterest
                && commands[4] == ClientCommand::RegisterRoutineDefinitionWatchInterest
                && commands[5] == ClientCommand::RegisterRoutineInvocationWatchInterest
                && matches!(
                    &commands[6],
                    ClientCommand::LoadRoutineInvocationDetail { invocation_id, .. }
                        if invocation_id == "server-issued-id"
                )
    ));
    assert_eq!(app.screen(), AppScreen::Routines);
    let rendered = render_app(&app);
    assert!(rendered.contains("server-issued-id"), "{rendered}");
    assert!(rendered.contains("Enter load task detail"), "{rendered}");

    let mut failed = running.clone();
    failed.routines.details.insert(
        "server-issued-id".to_owned(),
        RoutineDetailSnapshot {
            status: "failed",
            detail: None,
            error: Some("detail unavailable".to_owned()),
        },
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(failed)), now),
        AppAction::Continue
    );
    let rendered = render_app(&app);
    assert!(rendered.contains("detail unavailable"), "{rendered}");
    assert!(rendered.contains("Enter retry"), "{rendered}");
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::LoadRoutineInvocationDetail {
            request_id,
            invocation_id,
        }) if invocation_id == "server-issued-id" && request_id != initial_detail_request
    ));

    detail.summary.status = ClientRoutineInvocationStatus::Interrupted;
    detail.summary.terminal_error = Some(ClientRoutineFailure {
        code: "restart".to_owned(),
        message: "server restart\ninterrupted work".to_owned(),
    });
    detail.tasks = vec![
        ClientRoutineTask {
            name: "done".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Succeeded,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        },
        ClientRoutineTask {
            name: "skipped".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Skipped,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        },
        ClientRoutineTask {
            name: "blocked".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Blocked,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: Some(ClientRoutineFailure {
                code: "blocked".to_owned(),
                message: "dependency failed".to_owned(),
            }),
        },
        ClientRoutineTask {
            name: "failed".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Failed,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: Some(ClientRoutineFailure {
                code: "failed".to_owned(),
                message: "safe failure".to_owned(),
            }),
        },
        ClientRoutineTask {
            name: "interrupted".to_owned(),
            r#type: "action".to_owned(),
            status: ClientRoutineTaskStatus::Interrupted,
            results: Vec::new(),
            session_references: BTreeMap::new(),
            error: None,
        },
    ];
    running.routines.history.recent = vec![detail.summary.clone()];
    running.routines.details.insert(
        "server-issued-id".to_owned(),
        RoutineDetailSnapshot {
            status: "ready",
            detail: Some(detail),
            error: None,
        },
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(running)), now));
    let rendered = render_app(&app);
    for status in ["Succeeded", "Skipped", "Blocked", "Failed", "Interrupted"] {
        assert!(rendered.contains(status), "missing {status}: {rendered}");
    }
    assert!(
        rendered.contains("server restart interrupted work"),
        "{rendered}"
    );
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(app.screen(), AppScreen::Routines);
    assert!(render_app(&app).contains("Recent invocations"));
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseRoutineDefinitionWatchInterest,
            ClientCommand::ReleaseRoutineInvocationWatchInterest,
        ])
    );
}

#[test]
fn session_commands_execution_failure_preserves_draft_and_edit_clears_error() {
    let definition = routine_definition(
        "generic",
        "Generic command",
        "Runs declared work",
        vec![string_routine_input("direction", true)],
    );
    let base = routine_catalog_snapshot("source", "ready", vec![definition], None);
    let mut app = open_sessions_app_with(base.clone());
    let now = Instant::now();
    drop(open_routine_palette(&mut app, now));
    let action = invoke_selected_string_routine(&mut app, now, "keep", "draft");
    let (key, request) = match action {
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            ..
        }) => (operation_key, request_id),
        action => panic!("expected invocation, saw {action:?}"),
    };
    let failed = routine_invocation_snapshot(
        base,
        &key,
        &request,
        "source",
        "failed",
        None,
        Some("transport unavailable"),
        Vec::new(),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    let rendered = render_app(&app);
    assert!(
        rendered.contains("Unable to run session command."),
        "{rendered}"
    );
    assert!(rendered.contains("keep\\ndraft"), "{rendered}");
    drop(app.handle_event(&back_tab_press(), now));
    drop(app.handle_event(&char_press('!'), now));
    assert!(!render_app(&app).contains("Unable to run session command."));
}

#[test]
fn session_commands_access_loss_clears_palette_and_tracker() {
    let definition = routine_definition(
        "generic",
        "Generic command",
        "Runs declared work",
        vec![string_routine_input("direction", true)],
    );
    let mut app = open_sessions_app_with(routine_catalog_snapshot(
        "source",
        "ready",
        vec![definition],
        None,
    ));
    let now = Instant::now();
    drop(open_routine_palette(&mut app, now));
    drop(invoke_selected_string_routine(
        &mut app, now, "pending", "work",
    ));
    drop(app.handle_event(
        &AppEvent::Client(Box::new(login_required_snapshot("expired"))),
        now,
    ));
    assert_eq!(app.screen(), AppScreen::Access);
    assert!(!app.sessions_section().session_commands_open());
    assert!(!app.sessions_section().session_commands_pending());
}

#[test]
fn session_commands_render_list_form_pending_error_and_result_snapshots() {
    let definition = routine_definition(
        "generic",
        "Generic command",
        "Runs declared work",
        vec![string_routine_input("direction", true)],
    );
    let base = routine_catalog_snapshot("source", "ready", vec![definition], None);
    let mut app = open_sessions_app_with(base.clone());
    let now = Instant::now();
    drop(open_routine_palette(&mut app, now));
    insta::assert_snapshot!("session_commands_list", render_app(&app));
    drop(app.handle_event(&enter_press(), now));
    insta::assert_snapshot!("session_commands_form", render_app(&app));
    drop(app.handle_event(&char_press('x'), now));
    drop(app.handle_event(&tab_press(), now));
    let action = app.handle_event(&enter_press(), now);
    let (key, request) = match action {
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            ..
        }) => (operation_key, request_id),
        action => panic!("expected invocation, saw {action:?}"),
    };
    insta::assert_snapshot!("session_commands_pending", render_app(&app));
    let failed = routine_invocation_snapshot(
        base.clone(),
        &key,
        &request,
        "source",
        "failed",
        None,
        Some("offline"),
        Vec::new(),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    insta::assert_snapshot!("session_commands_error", render_app(&app));
    let retry = app.handle_event(&enter_press(), now);
    let (retry_key, retry_request) = match retry {
        AppAction::ClientCommand(ClientCommand::StartManualRoutine {
            operation_key,
            request_id,
            ..
        }) => (operation_key, request_id),
        action => panic!("expected retry, saw {action:?}"),
    };
    let ready = routine_invocation_snapshot(
        base,
        &retry_key,
        &retry_request,
        "source",
        "ready",
        None,
        None,
        vec![ClientRoutineResult {
            name: "session-id".to_owned(),
            value: ClientRoutineValue::String("destination".to_owned()),
        }],
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(ready)), now));
    insta::assert_snapshot!("session_commands_result", render_app(&app));
}

#[test]
fn sessions_inference_editor_renders_layered_snapshot() {
    let mut app = open_sessions_app_with(inference_editor_snapshot());
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
fn explicit_logout_clears_inference_editor_and_session_confirmation_state() {
    let mut app = open_sessions_app_with(inference_editor_snapshot());
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert!(render_app(&app).contains("Models"));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert_eq!(app.sessions_section(), &SessionsSection::new());

    let mut confirmation =
        open_sessions_app_with(watched_sessions_snapshot(vec![session_view("doomed", 1)]));
    assert_eq!(
        confirmation.handle_event(&ctrl_x_press(), now),
        AppAction::Continue
    );
    assert_eq!(
        confirmation.handle_event(&char_press('d'), now),
        AppAction::Continue
    );
    assert!(render_app(&confirmation).contains("Delete?"));
    drop(confirmation.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert_eq!(confirmation.sessions_section(), &SessionsSection::new());
}

#[test]
fn sessions_inference_search_supports_middle_editing() {
    let mut app = open_sessions_app_with(inference_editor_snapshot());
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
fn sessions_detail_ctrl_t_cycles_inference_reasoning() {
    let mut app = open_sessions_app_with(inference_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(
        app.handle_event(&ctrl_t_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionInferenceSettings {
            id: "chat".to_owned(),
            provider_key: "openai".to_owned(),
            model_id: "gpt4o".to_owned(),
            reasoning_effort: Some("high".to_owned()),
        })
    );

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("(Ctrl-T)"));
}

#[test]
fn sessions_inference_editor_enter_saves_selected_model() {
    let mut app = open_sessions_app_with(inference_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionInferenceSettings {
            id: "chat".to_owned(),
            provider_key: "anthropic".to_owned(),
            model_id: "claude".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
        })
    );
}

#[test]
fn sessions_inference_editor_requests_config_then_opens_when_config_arrives() {
    let mut snapshot = inference_editor_snapshot();
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
        app.handle_event(
            &AppEvent::Client(Box::new(inference_editor_snapshot())),
            now
        ),
        AppAction::Continue
    );

    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Models"), "{rendered}");
}

#[test]
fn sessions_inference_editor_escape_closes_overlay_before_interrupting_active_session() {
    let mut app = open_sessions_app_with(active_inference_editor_snapshot());
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
fn sessions_inference_editor_update_failure_preserves_editor() {
    let mut app = open_sessions_app_with(inference_editor_snapshot());
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&right_press(), now),
        expected_register_timeline_command("chat")
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('p'), now), AppAction::Continue);
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::UpdateSessionInferenceSettings { .. })
    ));
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(inference_update_snapshot("loading", None))),
            now,
        ),
        AppAction::Continue
    );
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(inference_update_snapshot(
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
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseSessionTimelineWatchInterest {
                session_id: "doomed".to_owned(),
            },
            ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
                session_id: "doomed".to_owned(),
            },
        ])
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
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseSessionTimelineWatchInterest {
                session_id: "one".to_owned(),
            },
            ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
                session_id: "one".to_owned(),
            },
        ])
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
            ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
                session_id: "one".to_owned(),
            },
            ClientCommand::ReleaseSessionWatchInterest,
            ClientCommand::RegisterConfigWatchInterest,
            ClientCommand::RequestAuthStatuses,
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

    let rendered = render_app_with_size(&app, 80, 30);
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
        SectionAction::ClientCommands(vec![
            ClientCommand::RegisterSessionTimelineWatchInterest {
                session_id: "chat".to_owned(),
                after_cursor: None,
                channels: timeline_channels(),
                initial_limit: Some(64),
            },
            ClientCommand::RegisterSessionRoutineInvocationWatchInterest {
                session_id: "chat".to_owned(),
            },
        ])
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
    assert!(rendered.contains("Context  170"));
    assert!(!rendered.contains("Context  999"));
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
    let normalized = rendered
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("Context  230"));
    assert!(normalized.contains("Cache read 45.5% write 9.1%"));
}

#[test]
fn sessions_detail_context_is_right_on_wide_and_capped_above_composer_on_narrow() {
    let mut session = session_view("parent", 10);
    session.metrics.total_input_tokens = 100;
    session.metrics.cache_read_tokens = 25;
    session.metrics.cache_write_tokens = 10;
    session.metrics.total_cost_micros = 12_345;
    session.metrics.cost_currency = Some("USD".to_owned());
    let mut snapshot = watched_session_with_timeline(
        session,
        vec![timeline_metrics_event(1, "parent", 1_200, 34)],
    );
    let mut child = session_with_phase(
        "child",
        1,
        SessionPhaseView::Suspended,
        "",
    );
    child.associations.task_parent_session_id = Some("parent".to_owned());
    child.associations.task_description = Some("Wait for review".to_owned());
    child.status.suspension_kind = Some(SessionSuspensionKindView::AwaitingExternalInput);
    snapshot.sessions.current.push(child);

    let mut app = open_sessions_app_with(snapshot);
    assert_eq!(
        app.handle_event(&right_press(), Instant::now()),
        expected_register_timeline_command("parent")
    );

    let wide = render_app_with_size(&app, 140, 30);
    let wide_context_column = wide
        .lines()
        .find_map(|line| line.find("Session Context"))
        .expect("wide Context heading");
    assert!(wide_context_column > 100, "{wide}");
    let wide_lines = wide.lines().collect::<Vec<_>>();
    for section in ["Usage", "Subtasks", "Routines"] {
        let row = wide_lines
            .iter()
            .position(|line| line[wide_context_column..].contains(section))
            .expect("Context section heading");
        assert!(
            wide_lines[row - 1][wide_context_column..]
                .trim()
                .trim_matches('"')
                .is_empty(),
            "{wide}"
        );
    }
    let normalized_wide = wide
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(wide.contains("Context  1.2k"), "{wide}");
    assert!(
        normalized_wide.contains("Cache read 25.0% write 10.0%"),
        "{wide}"
    );
    assert!(wide.contains("Cost  $0.012345"), "{wide}");
    assert!(normalized_wide.contains("awaiting external input"), "{wide}");
    let composer_metadata = wide
        .lines()
        .find(|line| line.contains("agent default"))
        .expect("composer metadata");
    assert!(!composer_metadata.contains("ctx"), "{wide}");
    assert!(!composer_metadata.contains("cache"), "{wide}");
    assert!(!composer_metadata.contains("cost"), "{wide}");

    let narrow = render_app_with_size(&app, 100, 30);
    let lines = narrow.lines().collect::<Vec<_>>();
    let context_row = lines
        .iter()
        .position(|line| line.contains("Session Context"))
        .expect("narrow Context heading");
    let composer_row = lines
        .iter()
        .position(|line| line.contains("type a message"))
        .expect("narrow composer");
    assert!(context_row < composer_row, "{narrow}");
    assert!(lines[context_row].find("Session Context").unwrap_or_default() < 10);
    assert!(narrow.contains("Subtasks"), "{narrow}");
    assert!(narrow.contains("Routines"), "{narrow}");
}

#[test]
fn sessions_detail_context_uses_authoritative_scoped_routine_history_in_snapshot_order() {
    let mut selected = session_view("parent", 10);
    selected.associations.routine_invocation_id = Some("helper".to_owned());
    let mut snapshot = watched_session_with_timeline(selected, Vec::new());

    let mut described = session_with_phase("described", 3, SessionPhaseView::RunningTool, "");
    described.associations.task_parent_session_id = Some("parent".to_owned());
    described.associations.task_description = Some("Compile workspace".to_owned());
    let mut fallback = session_with_phase("fallback-id", 2, SessionPhaseView::Failed, "failed");
    fallback.associations.task_parent_session_id = Some("parent".to_owned());
    fallback
        .metadata
        .insert("title".to_owned(), serde_json::json!("Fallback title"));
    let mut indirect = session_view("indirect", 1);
    indirect.associations.task_parent_session_id = Some("described".to_owned());
    indirect.associations.task_description = Some("Must not render".to_owned());
    snapshot.sessions.current.extend([described, fallback, indirect]);

    let mut newest = routine_detail(
        "source-new",
        ClientRoutineInvocationStatus::Running,
        Vec::new(),
    )
    .summary;
    newest.routine_name = "newest".to_owned();
    let mut unrelated = routine_detail(
        "unrelated",
        ClientRoutineInvocationStatus::Succeeded,
        Vec::new(),
    )
    .summary;
    unrelated.routine_name = "server-row".to_owned();
    let mut helper = routine_detail(
        "helper",
        ClientRoutineInvocationStatus::Interrupted,
        Vec::new(),
    )
    .summary;
    helper.routine_name = "helper-routine".to_owned();
    let mut duplicate = newest.clone();
    duplicate.routine_name = "duplicate".to_owned();
    let mut scoped_history = snapshot.routines.history.clone();
    scoped_history.recent = vec![newest, unrelated, helper, duplicate];
    snapshot
        .routines
        .session_histories
        .insert("parent".to_owned(), scoped_history);

    let mut app = open_sessions_app_with(snapshot);
    drop(app.handle_event(&right_press(), Instant::now()));
    let rendered = render_app_with_size(&app, 160, 36);

    assert!(rendered.contains("Compile workspace"), "{rendered}");
    assert!(rendered.contains("Fallback title"), "{rendered}");
    assert!(!rendered.contains("Must not render"), "{rendered}");
    assert!(rendered.contains("server-row [unrelated]"), "{rendered}");
    assert!(rendered.contains("duplicate"), "{rendered}");
    let newest_index = rendered.find("newest [source-new]").expect("source Routine");
    let helper_index = rendered
        .find("helper-routine [helper]")
        .expect("helper Routine");
    assert!(newest_index < helper_index, "{rendered}");
    assert!(rendered.contains("running"), "{rendered}");
    assert!(rendered.contains("interrupted"), "{rendered}");
}

#[test]
fn sessions_detail_narrow_context_summarizes_association_overflow() {
    let selected = session_view("parent", 10);
    let mut snapshot = watched_session_with_timeline(selected, Vec::new());
    for index in 0..5 {
        let mut child = session_view(&format!("child-{index}"), i64::from(index));
        child.associations.task_parent_session_id = Some("parent".to_owned());
        child.associations.task_description = Some(format!("Task {index}"));
        snapshot.sessions.current.push(child);
    }
    let mut scoped_history = snapshot.routines.history.clone();
    for index in 0..3 {
        let summary = routine_detail(
            &format!("routine-{index}"),
            ClientRoutineInvocationStatus::Succeeded,
            Vec::new(),
        )
        .summary;
        scoped_history.recent.push(summary);
    }
    snapshot
        .routines
        .session_histories
        .insert("parent".to_owned(), scoped_history);

    let mut app = open_sessions_app_with(snapshot);
    drop(app.handle_event(&right_press(), Instant::now()));
    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("Task 0"), "{rendered}");
    assert!(rendered.contains("routine-0"), "{rendered}");
    assert!(rendered.contains("+6 more"), "{rendered}");
    assert!(rendered.contains("type a message"), "{rendered}");
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
fn sessions_submit_auth_failure_is_actionable_without_emitting_oauth() {
    let session = session_view("chat", 1);
    let mut app =
        open_sessions_app_with(watched_session_with_timeline(session.clone(), Vec::new()));
    let now = Instant::now();
    drop(app.handle_event(&right_press(), now));
    for character in "retry me".chars() {
        drop(app.handle_event(&char_press(character), now));
    }
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::SubmitUserMessage { .. })
    ));

    let loading = session_submit_snapshot(session.clone(), "op-submit-auth", "loading", None);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(loading)), now),
        AppAction::Continue
    );
    let mut failed = session_submit_snapshot(
        session,
        "op-submit-auth",
        "failed",
        Some("authentication failed"),
    );
    let ClientOperationView::SessionSubmitUserMessage {
        authentication_failure: failure_slot,
        ..
    } = failed
        .operations
        .current
        .get_mut("op-submit-auth")
        .expect("submit operation")
    else {
        panic!("expected submit operation");
    };
    *failure_slot = Some(authentication_failure(
        "provider",
        "anthropic",
        "credentialRequired",
        None,
    ));
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(failed)), now),
        AppAction::Continue
    );

    let rendered = render_app_with_size(&app, 120, 30);
    assert!(rendered.contains("Authentication failed for provider anthropic"));
    assert!(rendered.contains("Open Config (Ctrl-X c)"));
    assert!(rendered.contains("resend the message"));
    assert!(rendered.contains("retry me"));
    assert!(!rendered.contains("OAuth authorization"));
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::SubmitUserMessage { .. })
    ));
}

#[test]
fn sessions_failed_status_renders_mcp_context_and_all_reason_guidance() {
    let reasons = [
        ("credentialRequired", "credentials are required"),
        ("credentialRejected", "stored credentials were rejected"),
        ("networkFailed", "authentication network request failed"),
        ("tokenExchangeFailed", "OAuth token exchange failed"),
        ("responseInvalid", "authentication response was invalid"),
        (
            "configurationRequired",
            "authentication configuration is required",
        ),
    ];

    for (reason, expected) in reasons {
        let mut session = session_with_phase("chat", 1, SessionPhaseView::Failed, "safe failure");
        session.status.authentication_failure = Some(authentication_failure(
            "mcpServer",
            "github",
            reason,
            Some(McpAuthorizationContextSnapshot {
                endpoint: Some("https://mcp.example/rpc".to_owned()),
                http_status: 401,
                resource_metadata_url: Some(
                    "https://mcp.example/.well-known/oauth-protected-resource".to_owned(),
                ),
                scope: Some("repo:read".to_owned()),
                error: Some("invalid_token".to_owned()),
                error_description: Some("authorization is required".to_owned()),
                error_uri: Some("https://mcp.example/auth-help".to_owned()),
                realm: Some("github-mcp".to_owned()),
            }),
        ));
        let mut app = open_sessions_app_with(watched_session_with_timeline(session, Vec::new()));
        assert_eq!(
            app.handle_event(&right_press(), Instant::now()),
            expected_register_timeline_command("chat")
        );

        let rendered = render_app_with_size(&app, 140, 32);
        assert!(rendered.contains("MCP server github"), "{rendered}");
        assert!(rendered.contains(expected), "{rendered}");
        assert!(
            rendered.contains("MCP endpoint: https://mcp.example/rpc"),
            "{rendered}"
        );
        assert!(rendered.contains("MCP HTTP status: 401"), "{rendered}");
        assert!(rendered.contains("MCP scope: repo:read"), "{rendered}");
        assert!(rendered.contains("Open Config (Ctrl-X c)"), "{rendered}");
        assert!(rendered.contains("rerun the failed work"), "{rendered}");
        assert!(!rendered.contains("OAuth authorization"), "{rendered}");
    }
}

#[test]
fn sessions_respond_and_continue_auth_failures_stay_inline() {
    let now = Instant::now();
    let payload = r#"{"tool_call":{"name":"shell","arguments":{"command":"pwd"}}}"#;
    let base = watched_session_with_timeline(
        session_view("chat", 1),
        vec![external_request_event(
            1, "chat", "req-item", "req-1", 1, payload,
        )],
    );
    let mut app = open_sessions_app_with(base.clone());
    drop(app.handle_event(&right_press(), now));
    assert!(matches!(
        app.handle_event(&char_press('a'), now),
        AppAction::ClientCommand(ClientCommand::RespondExternalRequest { .. })
    ));
    let mut loading = base.clone();
    set_respond_operation(&mut loading, "op-respond", "loading", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
    let mut failed = base;
    set_respond_operation(
        &mut failed,
        "op-respond",
        "failed",
        Some(authentication_failure(
            "provider",
            "anthropic",
            "credentialRejected",
            None,
        )),
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(failed)), now),
        AppAction::Continue
    );
    let rendered = render_app_with_size(&app, 120, 30);
    assert!(rendered.contains("respond again"), "{rendered}");
    assert!(!rendered.contains("OAuth authorization"), "{rendered}");

    let base = watched_session_with_timeline(
        session_view("chat", 1),
        vec![timeline_text_event(42, "chat", "user_text", "try this")],
    );
    let mut section = open_detail_section(&base);
    let area = Rect::new(0, 0, 120, 30);
    drop(section.handle_detail_click(Some(&base), area, 1, 0));
    assert!(matches!(
        section.handle_key(KeyEvent::from(KeyCode::Enter), Some(&base), Some(area)),
        SectionAction::ClientCommand(ClientCommand::ContinueSessionFromCursor { .. })
    ));
    let mut loading = base.clone();
    set_continue_operation(&mut loading, "op-continue", "loading", None);
    section.reconcile_operations(&loading);
    let mut failed = base;
    set_continue_operation(
        &mut failed,
        "op-continue",
        "failed",
        Some(authentication_failure(
            "mcpServer",
            "github",
            "configurationRequired",
            None,
        )),
    );
    section.reconcile_operations(&failed);
    let rendered = render_section(&section, &failed, area);
    assert!(rendered.contains("MCP server github"), "{rendered}");
    assert!(rendered.contains("rerun the failed work"), "{rendered}");
    assert!(!rendered.contains("OAuth authorization"), "{rendered}");
}

fn set_respond_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    authentication_failure: Option<AuthenticationFailureSnapshot>,
) {
    snapshot.sessions.respond_external_request = OperationSnapshot {
        status,
        error: (status == "failed").then(|| "authentication failed".to_owned()),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionRespondExternalRequest {
            status,
            error: (status == "failed").then(|| "authentication failed".to_owned()),
            session_id: "chat".to_owned(),
            request_id: "req-1".to_owned(),
            authentication_failure,
        },
    );
}

fn set_continue_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    authentication_failure: Option<AuthenticationFailureSnapshot>,
) {
    snapshot.sessions.continue_session = OperationSnapshot {
        status,
        error: (status == "failed").then(|| "authentication failed".to_owned()),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionContinue {
            status,
            error: (status == "failed").then(|| "authentication failed".to_owned()),
            session_id: "chat".to_owned(),
            target: RollbackTargetView {
                turn: None,
                cursor: Some(42),
            },
            authentication_failure,
        },
    );
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
    let snapshot =
        watched_session_with_timeline(session_view("chat", 1), timeline_message_events(1..=8));

    assert!(matches!(
        section.handle_key(key_event(right_press()), Some(&snapshot), Some(area)),
        SectionAction::ClientCommands(_)
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
    let snapshot =
        watched_session_with_timeline(session_view("chat", 1), timeline_message_events(1..=8));

    assert!(matches!(
        section.handle_key(key_event(right_press()), Some(&snapshot), Some(area)),
        SectionAction::ClientCommands(_)
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

    let updated_snapshot =
        watched_session_with_timeline(session_view("chat", 1), timeline_message_events(1..=9));
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

fn inference_editor_snapshot() -> ClientSnapshot {
    let mut session = session_view("chat", 1);
    session.inference_settings = Some(InferenceSettingsView {
        provider: Some(InferenceProviderSettingsView {
            key: "openai".to_owned(),
            kind: ProviderKindView::OpenAiChatCompletions,
            url: "https://api.openai.com/v1".to_owned(),
            model_id: "gpt4o".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
            match_status: InferenceSettingsMatchView::Configured,
        }),
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
        inference_defaults: None,
    });
    snapshot.config.load.status = "idle";
    snapshot.config.watch.status = "received";
    snapshot
}

fn active_inference_editor_snapshot() -> ClientSnapshot {
    let mut snapshot = inference_editor_snapshot();
    snapshot.sessions.current[0].status.phase = SessionPhaseView::RunningTool;
    snapshot
}

fn inference_update_snapshot(status: &'static str, error: Option<&str>) -> ClientSnapshot {
    let mut snapshot = inference_editor_snapshot();
    let session = snapshot.sessions.current[0].clone();
    snapshot.sessions.inference_settings_update = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some("op-runtime".to_owned()),
    };
    snapshot.operations.current.insert(
        "op-runtime".to_owned(),
        ClientOperationView::SessionInferenceSettingsUpdate {
            status,
            error: error.map(str::to_owned),
            session: Some(session),
        },
    );
    snapshot
}

fn runtime_provider(kind: ProviderKindView, name: &str, url: &str, model_id: &str) -> ProviderView {
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

#[allow(
    clippy::needless_pass_by_value,
    reason = "fixture consumes freshly constructed app events"
)]
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
                r#"{"decision":"deny","message":"not safe","mode":"return_tool_result"}"#.to_owned(),
        })
    );
}

#[test]
fn explicit_logout_clears_prompt_draft_and_focus() {
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
    for character in "private denial".chars() {
        assert_eq!(
            app.handle_event(&char_press(character), now),
            AppAction::Continue
        );
    }
    assert!(render_app(&app).contains("private denial"));
    drop(app.handle_event(
        &AppEvent::NativeLogin(waltr_tui::event::NativeLoginEvent::LoggedOut),
        now,
    ));
    assert_eq!(app.sessions_section(), &SessionsSection::new());
    assert!(!render_app(&app).contains("private denial"));
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

    assert_eq!(
        app.handle_event(&back_tab_press(), now),
        AppAction::Continue
    );
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
    let mut app = open_sessions_app_with(with_agents(
        watched_sessions_snapshot(Vec::new()),
        "default",
        &[("default", None)],
    ));
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('s'), now), AppAction::Continue);
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::CreateSession { .. })
    ));

    insta::assert_snapshot!(render_app(&app));
}

#[test]
fn sessions_create_error_renders_snapshot() {
    let mut app = open_sessions_app_with(with_agents(
        watched_sessions_snapshot(Vec::new()),
        "default",
        &[("default", None)],
    ));
    let now = Instant::now();

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('s'), now), AppAction::Continue);
    assert!(matches!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::CreateSession { .. })
    ));
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
