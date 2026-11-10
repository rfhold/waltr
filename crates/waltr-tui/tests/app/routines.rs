fn contract(json: &str) -> RoutineContract {
    RoutineContract {
        schema_version: "waltr.value/v1".into(),
        schema_json: RoutineJson::parse(json).unwrap(),
        schema: serde_json::from_str(json).unwrap(),
    }
}

fn routine_definition() -> ClientManualRoutineDefinition {
    ClientManualRoutineDefinition {
        routine_name: "branch".into(),
        description: "Branch work".into(),
        routine_revision: "opaque-revision".into(),
        api_version: "waltr.routine/v2".into(),
        engine: "javascript".into(),
        entrypoints: vec![RoutineEntrypoint {
            name: "manual".into(),
            trigger: ClientRoutineTriggerKind::Manual,
            selector: None,
            start_contexts: vec![RoutineStartContext::Global, RoutineStartContext::Session],
        }],
        parameters: contract(
            r#"{"type":"object","properties":{"direction":{"type":"string","maxLength":1000},"options":{"type":"nullable","schema":{"type":"array","maxItems":4,"items":{"type":"boolean"}}}},"required":["direction","options"]}"#,
        ),
        result: contract(r#"{"type":"null"}"#),
        ports: vec![],
        capabilities: vec![],
        budgets: RoutineBudgets {
            value_bytes: 65536,
            result_bytes: 65536,
            host_calls: 32,
            in_flight: 4,
            deadline_ms: RoutineRevision(30000),
        },
    }
}

fn routine_detail(
    id: &str,
    status: ClientRoutineInvocationStatus,
    results: Vec<RoutineJson>,
) -> ClientRoutineInvocationDetail {
    ClientRoutineInvocationDetail {
        summary: ClientRoutineInvocationSummary {
            version: 2,
            admission_sequence: RoutineRevision(9007199254740993),
            invocation_id: id.into(),
            routine_name: "branch".into(),
            routine_revision: "opaque-revision".into(),
            trigger_name: "manual".into(),
            trigger_kind: ClientRoutineTriggerKind::Manual,
            ownership: RoutineOwnership::Detached,
            correlation_id: "correlation".into(),
            snapshot: None,
            source_session: Some("source".into()),
            status,
            admitted_at_ms: 1,
            updated_at_ms: 2,
            result: results.into_iter().next(),
            text_results: vec![],
            binary_results: vec![],
            helpers: vec![],
            observation_revision: RoutineRevision(9),
        },
        pinned_definition: routine_definition(),
        operations: vec![RoutineOperation {
            version: 2,
            cursor: RoutineRevision(3),
            id: "operation".into(),
            invocation_id: id.into(),
            name: "create".into(),
            status: RoutineOperationStatus::Uncertain,
            arguments: None,
            result: None,
            updated_ms: 2,
            observation_revision: RoutineRevision(8),
        }],
        next_operation_cursor: None,
    }
}

fn routine_screen_snapshot() -> ClientSnapshot {
    let mut s = ready_snapshot();
    s.routines.definitions.watch.status = "received";
    s.routines.definitions.definitions = vec![routine_definition()];
    s.routines.history.watch.status = "received";
    let d = routine_detail(
        "invocation",
        ClientRoutineInvocationStatus::Uncertain,
        vec![],
    );
    s.routines.history.recent = vec![d.summary.clone()];
    s.routines.details.insert(
        "invocation".into(),
        RoutineDetailSnapshot {
            status: "ready",
            detail: Some(d),
            error: None,
        },
    );
    s
}

fn open_routines_app(snapshot: ClientSnapshot) -> App {
    let mut app = App::new();
    let now = Instant::now();
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    drop(app.handle_event(&ctrl_x_press(), now));
    assert_eq!(
        app.handle_event(&char_press('u'), now),
        AppAction::ClientCommands(vec![
            ClientCommand::RegisterRoutineDefinitionWatchInterest,
            ClientCommand::RegisterRoutineInvocationWatchInterest
        ])
    );
    app
}

fn keyed_request(action: AppAction) -> (String, String, RoutineRequest) {
    let AppAction::ClientCommands(commands) = action else {
        panic!("expected commands")
    };
    commands
        .into_iter()
        .find_map(|c| {
            if let ClientCommand::RoutineRequest {
                operation_key,
                request_id,
                command,
            } = c
            {
                Some((operation_key, request_id, command))
            } else {
                None
            }
        })
        .unwrap()
}

#[test]
fn routine_detail_end_and_down_reach_receipts_after_long_operation_history() {
    for navigation in [KeyCode::End, KeyCode::Down] {
        let mut snapshot = routine_screen_snapshot();
        let detail = snapshot
            .routines
            .details
            .get_mut("invocation")
            .unwrap()
            .detail
            .as_mut()
            .unwrap();
        detail.operations = (0..60)
            .map(|index| {
                let mut operation = detail.operations[0].clone();
                operation.id = format!("long-operation-{index}");
                operation
            })
            .collect();
        detail.next_operation_cursor = Some(RoutineRevision(61));
        let mut app = open_routines_app(snapshot.clone());
        let now = Instant::now();
        drop(app.handle_event(&tab_press(), now));
        drop(app.handle_event(&enter_press(), now));
        let (cancel_key, cancel_id, cancel_command) =
            keyed_request(app.handle_event(&char_press('c'), now));
        let (operations_key, operations_id, operations_command) =
            keyed_request(app.handle_event(&char_press('o'), now));
        for status in [
            RoutineRequestStatus::Loading,
            RoutineRequestStatus::Failed,
            RoutineRequestStatus::Expired,
        ] {
            for (key, id, command) in [
                (&cancel_key, &cancel_id, &cancel_command),
                (&operations_key, &operations_id, &operations_command),
            ] {
                snapshot.routines.requests.insert(
                    key.clone(),
                    RoutineRequestState {
                        request_id: id.clone(),
                        command: command.clone(),
                        status,
                        error: (status == RoutineRequestStatus::Failed)
                            .then(|| "receipt failure marker".into()),
                        next_cursor: None,
                        cancellation_status: None,
                        selected_invocations: vec![],
                    },
                );
            }
            drop(app.handle_event(&AppEvent::Client(Box::new(snapshot.clone())), now));
            drop(app.handle_event(&home_press(), now));
            assert!(!render_app_with_size(&app, 120, 18).contains("Cancellation receipt"));
            for _ in 0..200 {
                drop(app.handle_event(
                    &AppEvent::Input(InputEvent::Key(KeyEvent::new(
                        navigation,
                        KeyModifiers::NONE,
                    ))),
                    now,
                ));
            }
            let rendered = render_app_with_size(&app, 120, 18);
            assert!(
                rendered.contains(&format!("Cancellation receipt: {status:?}")),
                "{rendered}"
            );
            assert!(
                rendered.contains(&format!("Operations page: {status:?}")),
                "{rendered}"
            );
            if status == RoutineRequestStatus::Failed {
                assert!(rendered.contains("receipt failure marker"), "{rendered}");
            }
        }
        for detail_status in ["loading", "failed"] {
            let detail = snapshot.routines.details.get_mut("invocation").unwrap();
            detail.status = detail_status;
            detail.error = Some("detail failure ".repeat(300));
            drop(app.handle_event(&AppEvent::Client(Box::new(snapshot.clone())), now));
            drop(app.handle_event(
                &AppEvent::Input(InputEvent::Key(KeyEvent::new(
                    KeyCode::End,
                    KeyModifiers::NONE,
                ))),
                now,
            ));
            let rendered = render_app_with_size(&app, 120, 18);
            assert!(
                rendered.contains("Cancellation receipt: Expired"),
                "{rendered}"
            );
            assert!(rendered.contains("Operations page: Expired"), "{rendered}");
        }
        snapshot
            .routines
            .details
            .get_mut("invocation")
            .unwrap()
            .status = "ready";
        snapshot.routines.requests.clear();
        snapshot
            .routines
            .details
            .get_mut("invocation")
            .unwrap()
            .detail
            .as_mut()
            .unwrap()
            .operations
            .truncate(1);
        drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
        let rendered = render_app_with_size(&app, 120, 18);
        assert!(!rendered.contains("Operations page:"), "{rendered}");
        assert!(!rendered.contains("Cancellation receipt:"), "{rendered}");
        assert!(rendered.contains("long-operation-0"), "{rendered}");
    }
}

#[test]
fn routines_route_releases_watches_and_exact_selected_page_identity() {
    let mut app = open_routines_app(routine_screen_snapshot());
    let now = Instant::now();
    let (key, id, request) = keyed_request(app.handle_event(&char_press('r'), now));
    assert_eq!(
        request,
        RoutineRequest::Invocations {
            after_cursor: RoutineRevision(0),
            limit: 50,
            source_session: None
        }
    );
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommands(vec![
            ClientCommand::ReleaseRoutineRequest {
                operation_key: key,
                request_id: id
            },
            ClientCommand::ReleaseRoutineDefinitionWatchInterest,
            ClientCommand::ReleaseRoutineInvocationWatchInterest
        ])
    );
    assert_eq!(app.screen(), AppScreen::Landing);
}

#[test]
fn selected_page_survives_200_new_rows_and_expiry_is_visible() {
    let mut snapshot = routine_screen_snapshot();
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    let (key, id, request) = keyed_request(app.handle_event(&char_press('r'), now));
    snapshot.routines.requests.insert(
        key.clone(),
        RoutineRequestState {
            request_id: id.clone(),
            command: request,
            status: RoutineRequestStatus::Ready,
            error: None,
            next_cursor: Some(RoutineRevision(9007199254740995)),
            cancellation_status: None,
            selected_invocations: vec![
                routine_detail(
                    "selected-old",
                    ClientRoutineInvocationStatus::Succeeded,
                    vec![],
                )
                .summary,
            ],
        },
    );
    snapshot.routines.history.recent = (0..200)
        .map(|i| {
            routine_detail(
                &format!("new-{i}"),
                ClientRoutineInvocationStatus::Running,
                vec![],
            )
            .summary
        })
        .collect();
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot.clone())), now));
    let rendered = render_app_with_size(&app, 100, 30);
    assert!(rendered.contains("selected-old"), "{rendered}");
    assert!(!rendered.contains("new-199"));
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&enter_press(), now));
    assert!(render_app(&app).contains("selected-old"));
    drop(app.handle_event(&escape_press(), now));
    let receipt = snapshot.routines.requests.get_mut(&key).unwrap();
    receipt.status = RoutineRequestStatus::Expired;
    receipt.error = Some("Selected page budget exceeded".into());
    receipt.selected_invocations.clear();
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert!(render_app_with_size(&app, 100, 30).contains("Page expired"));
    let (newkey, newid, request) = keyed_request(app.handle_event(&char_press('r'), now));
    assert_ne!(key, newkey);
    assert_ne!(id, newid);
    assert!(matches!(
        request,
        RoutineRequest::Invocations {
            after_cursor: RoutineRevision(0),
            ..
        }
    ));
}

#[test]
fn pinned_schema_dynamic_uncertainty_and_cancel_receipt_are_not_task_history() {
    let mut snapshot = routine_screen_snapshot();
    snapshot.routines.definitions.definitions.clear();
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&enter_press(), now));
    let rendered = render_app_with_size(&app, 120, 40);
    assert!(
        rendered.contains("Pinned definition branch @ opaque-revision"),
        "{rendered}"
    );
    assert!(rendered.contains("[Uncertain] create"));
    assert!(rendered.contains("9007199254740993"));
    assert!(!rendered.contains("task"));
    let (key, id, command) = keyed_request(app.handle_event(&char_press('c'), now));
    assert_eq!(
        command,
        RoutineRequest::Cancel {
            invocation_id: "invocation".into()
        }
    );
    snapshot.routines.requests.insert(
        key.clone(),
        RoutineRequestState {
            request_id: id.clone(),
            command,
            status: RoutineRequestStatus::Ready,
            error: None,
            next_cursor: None,
            cancellation_status: Some(ClientRoutineInvocationStatus::Running),
            selected_invocations: vec![],
        },
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    let rendered = render_app_with_size(&app, 120, 40);
    assert!(rendered.contains("Cancellation receipt: Ready"));
    assert!(rendered.contains("Uncertain"));
    assert_eq!(
        app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::ReleaseRoutineRequest {
            operation_key: key,
            request_id: id
        })
    );
}

fn replace_json(app: &mut App, json: &str, now: Instant) {
    drop(app.handle_event(&home_press(), now));
    // Initial object contains both required fields, including an explicit null.
    for _ in 0..100 {
        drop(app.handle_event(
            &AppEvent::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            ))),
            now,
        ));
    }
    for c in json.chars() {
        drop(app.handle_event(&char_press(c), now));
    }
}

#[test]
fn global_schema_editor_submits_exact_json_and_rejects_stale_revision() {
    let mut snapshot = routine_screen_snapshot();
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    assert_eq!(
        app.handle_event(&char_press('n'), now),
        AppAction::ClientCommand(ClientCommand::RegisterRoutineDefinitionWatchInterest)
    );
    drop(app.handle_event(&enter_press(), now));
    let exact = "{\"direction\":\" a\\nb \",\"options\":[true,false]}";
    replace_json(&mut app, exact, now);
    let action = app.handle_event(&tab_press(), now);
    assert!(
        matches!(action,AppAction::ClientCommand(ClientCommand::StartManualRoutine {inputs,routine_revision,source_session_id:None,trigger_name,..}) if inputs.as_str()==exact && routine_revision=="opaque-revision" && trigger_name=="manual")
    );
    let mut second = open_routines_app(snapshot.clone());
    drop(second.handle_event(&char_press('n'), now));
    drop(second.handle_event(&enter_press(), now));
    snapshot.routines.definitions.definitions[0].routine_revision = "replacement".into();
    drop(second.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert_eq!(second.handle_event(&tab_press(), now), AppAction::Continue);
    assert!(render_app_with_size(&second, 120, 40).contains("changed. Select again"));
}

#[test]
fn session_entrypoints_use_exact_context_and_current_source() {
    let mut snapshot = watched_session_with_timeline(session_view("source", 1), vec![]);
    let mut definition = routine_definition();
    definition.entrypoints[0].start_contexts = vec![RoutineStartContext::Session];
    definition.entrypoints.push(RoutineEntrypoint {
        name: "not-manual".into(),
        trigger: ClientRoutineTriggerKind::Event,
        selector: None,
        start_contexts: vec![RoutineStartContext::Session],
    });
    snapshot.routines.definitions.watch.status = "received";
    snapshot.routines.definitions.definitions = vec![definition];
    let mut app = open_sessions_app_with(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&ctrl_p_press(), now));
    let rendered = render_app_with_size(&app, 120, 40);
    assert!(rendered.contains("branch / manual"));
    assert!(!rendered.contains("not-manual"));
    drop(app.handle_event(&enter_press(), now));
    replace_json(&mut app, r#"{"direction":"do it","options":null}"#, now);
    assert!(
        matches!(app.handle_event(&tab_press(),now),AppAction::ClientCommand(ClientCommand::StartManualRoutine {source_session_id:Some(id),..}) if id=="source")
    );
}

#[test]
fn source_session_route_pins_page_filter_and_releases_all_route_owners() {
    let snapshot = watched_session_with_timeline(session_view("source", 1), vec![]);
    let mut app = open_sessions_app_with(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&ctrl_x_press(), now));
    let (key, id, command) = keyed_request(app.handle_event(&char_press('u'), now));
    assert_eq!(
        command,
        RoutineRequest::Invocations {
            after_cursor: RoutineRevision(0),
            limit: 50,
            source_session: Some("source".into())
        }
    );
    let action = app.handle_event(&escape_press(), now);
    let AppAction::ClientCommands(commands) = action else {
        panic!("expected route disposal")
    };
    assert!(commands.contains(&ClientCommand::ReleaseRoutineRequest {
        operation_key: key,
        request_id: id
    }));
    assert!(commands.contains(
        &ClientCommand::ReleaseSessionRoutineInvocationWatchInterest {
            session_id: "source".into()
        }
    ));
    assert!(commands.contains(&ClientCommand::ReleaseRoutineDefinitionWatchInterest));
    assert!(commands.contains(&ClientCommand::ReleaseRoutineInvocationWatchInterest));
}

#[test]
fn structured_editor_rejects_unsafe_duplicate_and_incomplete_values() {
    let now = Instant::now();
    for invalid in [
        r#"{"direction":"x","options":null,"direction":"y"}"#,
        r#"{"direction":9007199254740993,"options":null}"#,
        r#"{"direction":"x"}"#,
        r#"{"direction":"x","options":[null]}"#,
    ] {
        let mut app = open_routines_app(routine_screen_snapshot());
        drop(app.handle_event(&char_press('n'), now));
        drop(app.handle_event(&enter_press(), now));
        replace_json(&mut app, invalid, now);
        assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
        assert!(render_app_with_size(&app, 120, 40).contains("Parameters must match"));
    }
}

#[test]
fn selected_page_ignores_foreign_request_and_uses_exact_opaque_cursor() {
    let mut snapshot = routine_screen_snapshot();
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    let (key, id, command) = keyed_request(app.handle_event(&char_press('r'), now));
    snapshot.routines.requests.insert(
        key.clone(),
        RoutineRequestState {
            request_id: "foreign".into(),
            command: command.clone(),
            status: RoutineRequestStatus::Ready,
            error: None,
            next_cursor: Some(RoutineRevision(u64::MAX)),
            cancellation_status: None,
            selected_invocations: vec![
                routine_detail("foreign", ClientRoutineInvocationStatus::Succeeded, vec![]).summary,
            ],
        },
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot.clone())), now));
    assert!(!render_app_with_size(&app, 120, 40).contains("foreign"));
    snapshot.routines.requests.get_mut(&key).unwrap().request_id = id.clone();
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    drop(app.handle_event(&tab_press(), now));
    let AppAction::ClientCommands(commands) = app.handle_event(&char_press('o'), now) else {
        panic!("expected next page")
    };
    assert_eq!(
        commands[0],
        ClientCommand::ReleaseRoutineRequest {
            operation_key: key,
            request_id: id
        }
    );
    assert!(matches!(
        commands.last(),
        Some(ClientCommand::RoutineRequest {
            command: RoutineRequest::Invocations {
                after_cursor: RoutineRevision(u64::MAX),
                ..
            },
            ..
        })
    ));
}

#[test]
fn removed_source_session_cannot_submit_a_previously_selected_entrypoint() {
    let mut snapshot = watched_session_with_timeline(session_view("source", 1), vec![]);
    snapshot.routines.definitions.definitions = vec![routine_definition()];
    let mut app = open_sessions_app_with(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&ctrl_p_press(), now));
    drop(app.handle_event(&enter_press(), now));
    replace_json(&mut app, r#"{"direction":"x","options":null}"#, now);
    snapshot.sessions.current.clear();
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert!(!matches!(
        app.handle_event(&tab_press(), now),
        AppAction::ClientCommand(ClientCommand::StartManualRoutine { .. })
    ));
}

fn media_submit() -> AppEvent {
    AppEvent::Input(InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    )))
}

#[test]
fn interactive_media_picker_preserves_business_bytes_and_separate_local_declarations() {
    let mut snapshot = routine_screen_snapshot();
    let mut interactive = routine_definition();
    interactive.entrypoints[0].name = "interactive".into();
    interactive.entrypoints[0].trigger = ClientRoutineTriggerKind::Interactive;
    snapshot
        .routines
        .definitions
        .definitions
        .push(interactive.clone());
    snapshot.routines.definitions.limits =
        Some(waltr_client::interaction::wire::ServerLimits::default().into());
    let mut app = open_routines_app(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&char_press('m'), now));
    let rendered = render_app_with_size(&app, 120, 40);
    assert!(rendered.contains("interactive"));
    assert!(!rendered.contains(" / manual @"));
    drop(app.handle_event(&enter_press(), now));
    let parameters = " {\"direction\":\"  keep this  \",\"options\":null} ";
    replace_json(&mut app, parameters, now);
    let AppAction::Media(waltr_tui::media::MediaRequest::Interaction {
        id,
        definition,
        entrypoint,
        parameters: exact,
        plan,
        source,
        ..
    }) = app.handle_event(&media_submit(), now)
    else {
        panic!("expected private broker request")
    };
    assert_eq!(*definition, interactive);
    assert_eq!(entrypoint, "interactive");
    assert_eq!(exact, parameters);
    assert!(plan.inputs.is_empty());
    assert!(source.is_none());
    drop(app.handle_event(
        &AppEvent::Media(waltr_tui::media::MediaEvent {
            id: id + 1,
            status: "foreign-owner".into(),
            preview: "foreign-preview".into(),
            truncated: false,
            done: true,
        }),
        now,
    ));
    assert!(!render_app_with_size(&app, 120, 40).contains("foreign-preview"));
    drop(app.handle_event(
        &AppEvent::Media(waltr_tui::media::MediaEvent {
            id,
            status: "Complete".into(),
            preview: "bounded-preview".into(),
            truncated: true,
            done: true,
        }),
        now,
    ));
    let rendered = render_app_with_size(&app, 120, 40);
    assert!(rendered.contains("PREVIEW TRUNCATED"));
    assert!(rendered.contains("bounded-preview"));
    assert!(matches!(
        app.handle_event(&char_press('y'), now),
        AppAction::Media(waltr_tui::media::MediaRequest::Copy {
            truncated: true,
            ..
        })
    ));
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
}

#[test]
fn interactive_media_submission_rechecks_exact_revision() {
    let mut snapshot = routine_screen_snapshot();
    snapshot.routines.definitions.definitions[0].entrypoints[0].trigger =
        ClientRoutineTriggerKind::Interactive;
    snapshot.routines.definitions.limits =
        Some(waltr_client::interaction::wire::ServerLimits::default().into());
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&char_press('m'), now));
    drop(app.handle_event(&enter_press(), now));
    replace_json(&mut app, r#"{"direction":"x","options":null}"#, now);
    snapshot.routines.definitions.definitions[0].routine_revision = "new-revision".into();
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert_eq!(app.handle_event(&media_submit(), now), AppAction::Continue);
    assert!(
        render_app_with_size(&app, 120, 40).contains("Revision, eligibility, or source changed")
    );
}

#[test]
fn session_interactive_media_uses_current_source_not_context_alone() {
    for remove_source in [false, true] {
        let mut snapshot = watched_session_with_timeline(session_view("source", 1), vec![]);
        let mut definition = routine_definition();
        definition.entrypoints[0].trigger = ClientRoutineTriggerKind::Interactive;
        definition.entrypoints[0].start_contexts = vec![RoutineStartContext::Session];
        snapshot.routines.definitions.definitions = vec![definition];
        snapshot.routines.definitions.limits =
            Some(waltr_client::interaction::wire::ServerLimits::default().into());
        let mut app = open_sessions_app_with(snapshot.clone());
        let now = Instant::now();
        drop(app.handle_event(&right_press(), now));
        drop(app.handle_event(&ctrl_x_press(), now));
        drop(app.handle_event(&char_press('u'), now));
        drop(app.handle_event(&char_press('m'), now));
        drop(app.handle_event(&enter_press(), now));
        replace_json(&mut app, r#"{"direction":"x","options":null}"#, now);
        if remove_source {
            snapshot.sessions.current.clear();
            drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
        }
        let action = app.handle_event(&media_submit(), now);
        if remove_source {
            assert_eq!(action, AppAction::Continue);
        } else {
            assert!(
                matches!(action, AppAction::Media(waltr_tui::media::MediaRequest::Interaction { source: Some(id), .. }) if id == "source")
            );
        }
    }
}

#[test]
fn durable_text_selection_uses_pinned_result_and_rejects_changed_descriptor() {
    let mut snapshot = routine_screen_snapshot();
    snapshot.routines.definitions.definitions.clear();
    let descriptor = RoutineTextResult {
        id: "text-result".into(),
        name: "answer".into(),
        sha256: "0".repeat(64),
        utf8_bytes: RoutineRevision(12),
        encoded_charge: RoutineRevision(16),
        helper_id: None,
    };
    snapshot
        .routines
        .details
        .get_mut("invocation")
        .unwrap()
        .detail
        .as_mut()
        .unwrap()
        .summary
        .text_results = vec![descriptor.clone()];
    let mut app = open_routines_app(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&enter_press(), now));
    drop(app.handle_event(&char_press('t'), now));
    let AppAction::Media(waltr_tui::media::MediaRequest::Text {
        invocation,
        descriptors,
        selected,
        destination,
        ..
    }) = app.handle_event(&media_submit(), now)
    else {
        panic!("expected selected durable reader")
    };
    assert_eq!(invocation, "invocation");
    assert_eq!(selected, "text-result");
    assert_eq!(descriptors, vec![descriptor]);
    assert!(destination.is_none());
    drop(app.handle_event(&escape_press(), now));
    drop(app.handle_event(&char_press('t'), now));
    snapshot
        .routines
        .details
        .get_mut("invocation")
        .unwrap()
        .detail
        .as_mut()
        .unwrap()
        .summary
        .text_results[0]
        .utf8_bytes = RoutineRevision(99);
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert_eq!(app.handle_event(&media_submit(), now), AppAction::Continue);
    assert!(render_app_with_size(&app, 120, 40).contains("Text descriptor selection changed"));
}
