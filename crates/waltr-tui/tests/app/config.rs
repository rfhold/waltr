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
        expected_config_entry_action()
    );

    let rendered = render_app(&app);
    assert!(rendered.contains("Waltr"));
    assert!(rendered.contains("Configuration"));
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
    assert!(rendered.contains("R Reset configuration"));
    assert!(rendered.contains("> anthropic"));
}

#[test]
fn config_reset_is_discoverable_from_every_loaded_tab() {
    let mut app = open_config_app();
    let now = Instant::now();

    for expected_tab in [
        ConfigTab::Providers,
        ConfigTab::Mcp,
        ConfigTab::InferenceDefaults,
    ] {
        assert_eq!(app.active_config_tab(), expected_tab);
        assert!(render_app(&app).contains("R Reset configuration"));
        drop(app.handle_event(&tab_press(), now));
    }
}

#[test]
fn config_reset_confirmation_cancels_without_dispatch() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.handle_event(&char_press('R'), now), AppAction::Continue);
    insta::assert_snapshot!("config_reset_confirmation", render_app(&app));
    assert_eq!(app.handle_event(&char_press('n'), now), AppAction::Continue);
    assert!(!render_app(&app).contains("return to server defaults"));

    drop(app.handle_event(&tab_press(), now));
    assert_eq!(app.active_config_tab(), ConfigTab::Mcp);
    drop(app.handle_event(&char_press('R'), now));
    assert_eq!(app.handle_event(&escape_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Mcp);
    assert!(!render_app(&app).contains("return to server defaults"));
}

#[test]
fn config_reset_pending_blocks_duplicate_and_conflicting_input() {
    let mut app = open_config_app();
    let now = Instant::now();

    drop(app.handle_event(&char_press('R'), now));
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::ResetConfig)
    );
    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "reset-1",
            "loading",
            None,
        ))),
        now,
    ));
    insta::assert_snapshot!("config_reset_pending", render_app(&app));

    for input in [char_press('R'), enter_press(), escape_press(), tab_press()] {
        assert_eq!(app.handle_event(&input, now), AppAction::Continue);
    }
    assert_eq!(app.active_config_tab(), ConfigTab::Providers);
    assert!(render_app(&app).contains("Resetting configuration..."));
}

#[test]
fn config_reset_failure_retains_context_and_retry_uses_new_matching_key() {
    let mut app = open_config_app();
    let now = Instant::now();
    drop(app.handle_event(&char_press('R'), now));
    assert_eq!(
        app.handle_event(&char_press('y'), now),
        AppAction::ClientCommand(ClientCommand::ResetConfig)
    );

    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "reset-1",
            "loading",
            None,
        ))),
        now,
    ));
    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "reset-1",
            "failed",
            Some("catalog unavailable"),
        ))),
        now,
    ));
    insta::assert_snapshot!("config_reset_failure", render_app(&app));
    assert!(render_app(&app).contains("catalog unavailable"));

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::ResetConfig)
    );
    assert!(!render_app(&app).contains("catalog unavailable"));
    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "reset-2",
            "loading",
            None,
        ))),
        now,
    ));
    assert!(render_app(&app).contains("Resetting configuration..."));
}

#[test]
fn config_reset_ignores_stale_and_mismatched_operations_then_renders_matching_success() {
    let mut app = open_config_app_with_mcp();
    let now = Instant::now();
    drop(app.handle_event(&char_press('R'), now));
    drop(app.handle_event(&enter_press(), now));

    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "stale-reset",
            "ready",
            None,
        ))),
        now,
    ));
    assert!(render_app(&app).contains("Resetting configuration..."));

    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "reset-1",
            "loading",
            None,
        ))),
        now,
    ));
    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            watched_config_snapshot(),
            "other-reset",
            "ready",
            None,
        ))),
        now,
    ));
    assert!(render_app(&app).contains("Resetting configuration..."));

    let mut reset_config = watched_config_snapshot();
    reset_config
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap()
        .name = "server-default".to_owned();
    drop(app.handle_event(
        &AppEvent::Client(Box::new(config_reset_snapshot_from(
            reset_config,
            "reset-1",
            "ready",
            None,
        ))),
        now,
    ));

    let rendered = render_app(&app);
    assert!(!rendered.contains("Resetting configuration..."));
    assert!(rendered.contains("server-default"));
    assert!(rendered.contains("configuration reset to server defaults"));
    insta::assert_snapshot!("config_reset_success", rendered);
}

#[test]
fn config_tab_switch_cycles_between_providers_mcp_and_defaults() {
    let mut app = open_config_app();
    let now = Instant::now();

    assert_eq!(app.active_config_tab(), ConfigTab::Providers);
    assert!(app.provider_section().config_form().is_none());

    // Tab cycles Providers -> MCP -> Inference Defaults -> Providers.
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Mcp);
    assert!(render_app(&app).contains("No MCP servers configured."));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::InferenceDefaults);
    assert!(render_app(&app).contains("r reason"));

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Providers);

    // Provider section behavior is unchanged after switching tabs.
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('c'), now), AppAction::Continue);
    assert!(app.provider_section().config_form().is_some());
    assert!(render_app(&app).contains("Create provider"));
}

#[test]
fn config_defaults_renders_model_selector_and_saves_selected_model() {
    let mut app = open_config_app_with_model();
    let now = Instant::now();

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::InferenceDefaults);

    let rendered = render_app(&app);
    assert!(rendered.contains("Default model"));
    assert!(rendered.contains("m selects model"));
    assert!(!rendered.contains("> anthropic/sonnet"));

    assert_eq!(app.handle_event(&char_press('m'), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Select default model"));
    assert!(rendered.contains("Models"));
    assert!(rendered.contains("> anthropic/sonnet"));
    assert!(rendered.contains("[selected]"));

    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(!render_app(&app).contains("Select default model"));

    let AppAction::ClientCommand(ClientCommand::SetInferenceDefaults {
        defaults: Some(defaults),
    }) = app.handle_event(&enter_press(), now)
    else {
        panic!("expected SetInferenceDefaults command");
    };
    assert_eq!(defaults.provider_key, "anthropic");
    assert_eq!(defaults.model_id, "sonnet");
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
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert!(render_app(&app).contains("> Claude"));
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Edit model"));
    assert!(rendered.contains("catalog: anthropic / claude-sonnet-4-5 (manual)"));
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
    let model = provider
        .models
        .get("sonnet")
        .expect("edited model should still exist");
    assert_eq!(
        model.catalog_match.as_ref().map(|catalog_match| (
            catalog_match.provider_id.as_str(),
            catalog_match.model_id.as_str(),
            catalog_match.source,
        )),
        Some((
            "anthropic",
            "claude-sonnet-4-5",
            CatalogMatchSourceView::Manual,
        ))
    );
}

#[test]
fn config_provider_model_form_escape_returns_to_provider_draft() {
    let mut app = open_config_app_with_model();
    let now = Instant::now();

    assert_eq!(app.handle_event(&right_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
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
    assert!(rendered.contains("Tab field"));
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
fn config_provider_form_edits_all_public_client_oauth_fields() {
    let mut app = open_config_app();
    let now = Instant::now();
    drop(app.handle_event(&ctrl_x_press(), now));
    drop(app.handle_event(&char_press('c'), now));
    type_text(&mut app, "custom", now);
    for _ in 0..3 {
        drop(app.handle_event(&tab_press(), now));
    }
    drop(app.handle_event(&char_press(' '), now));

    let text_fields = [
        "Custom OAuth",
        "https://id.example/authorize",
        "https://id.example/token",
        "public-client",
        "http://127.0.0.1/callback",
        "openid, profile",
    ];
    for value in text_fields {
        drop(app.handle_event(&tab_press(), now));
        type_text(&mut app, value, now);
    }
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&right_press(), now));
    drop(app.handle_event(&tab_press(), now));
    type_text(
        &mut app,
        r#"[{"key":"audience","value":"api"}]"#,
        now,
    );
    drop(app.handle_event(&tab_press(), now));
    type_text(
        &mut app,
        r#"[{"token":"id","metadataKey":"tenant","claimNames":["tid","tenant"],"required":true}]"#,
        now,
    );

    let AppAction::ClientCommand(ClientCommand::AddProvider { provider }) =
        app.handle_event(&enter_press(), now)
    else {
        panic!("expected provider add");
    };
    let oauth = provider.oauth_settings.expect("OAuth settings");
    assert!(provider.oauth_compatible);
    assert_eq!(oauth.name, "Custom OAuth");
    assert_eq!(oauth.authorize_url, "https://id.example/authorize");
    assert_eq!(oauth.token_url, "https://id.example/token");
    assert_eq!(oauth.client_id, "public-client");
    assert_eq!(oauth.redirect_uri, "http://127.0.0.1/callback");
    assert_eq!(oauth.scopes, vec!["openid", "profile"]);
    assert_eq!(oauth.token_request_style, OAuthTokenRequestStyleView::Json);
    assert!(oauth.send_state_in_token_request);
    assert_eq!(
        oauth.auth_url_params,
        vec![OAuthUrlParamView {
            key: "audience".to_owned(),
            value: "api".to_owned(),
        }]
    );
    assert_eq!(
        oauth.jwt_metadata,
        vec![OAuthJwtMetadataView {
            token: OAuthTokenFieldView::Id,
            metadata_key: "tenant".to_owned(),
            claim_names: vec!["tid".to_owned(), "tenant".to_owned()],
            required: true,
        }]
    );
}

#[test]
fn config_provider_oauth_toggle_off_preserves_settings_until_explicit_clear() {
    let mut snapshot = watched_config_snapshot();
    let expected = OAuthProviderView {
        name: "Existing".to_owned(),
        authorize_url: "https://id.example/authorize".to_owned(),
        token_url: "https://id.example/token".to_owned(),
        client_id: "public".to_owned(),
        redirect_uri: "http://127.0.0.1/callback".to_owned(),
        scopes: vec!["openid".to_owned()],
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: vec![OAuthUrlParamView {
            key: "audience".to_owned(),
            value: "api".to_owned(),
        }],
        jwt_metadata: Vec::new(),
        send_state_in_token_request: false,
    };
    let provider = snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.oauth_compatible = true;
    provider.oauth_settings = Some(expected.clone());
    let mut app = open_config_app_with(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&enter_press(), now));
    for _ in 0..3 {
        drop(app.handle_event(&tab_press(), now));
    }
    drop(app.handle_event(&char_press(' '), now));
    let AppAction::ClientCommand(ClientCommand::UpdateProvider { provider, .. }) =
        app.handle_event(&enter_press(), now)
    else {
        panic!("expected provider update");
    };
    assert!(!provider.oauth_compatible);
    assert_eq!(provider.oauth_settings, Some(expected));

    let mut clear_snapshot = watched_config_snapshot();
    let provider = clear_snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.oauth_compatible = true;
    provider.oauth_settings = Some(OAuthProviderView {
        name: "Existing".to_owned(),
        authorize_url: String::new(),
        token_url: String::new(),
        client_id: "public".to_owned(),
        redirect_uri: String::new(),
        scopes: Vec::new(),
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: Vec::new(),
        jwt_metadata: Vec::new(),
        send_state_in_token_request: false,
    });
    let mut clear_app = open_config_app_with(clear_snapshot);
    drop(clear_app.handle_event(&enter_press(), now));
    for _ in 0..3 {
        drop(clear_app.handle_event(&tab_press(), now));
    }
    drop(clear_app.handle_event(&delete_press(), now));
    let AppAction::ClientCommand(ClientCommand::UpdateProvider { provider, .. }) =
        clear_app.handle_event(&enter_press(), now)
    else {
        panic!("expected provider update");
    };
    assert!(!provider.oauth_compatible);
    assert_eq!(provider.oauth_settings, None);
}

#[test]
fn config_auth_status_updates_preserve_provider_form_draft_and_focus() {
    let mut app = open_config_app();
    let now = Instant::now();
    drop(app.handle_event(&enter_press(), now));
    type_text(&mut app, "-draft", now);
    drop(app.handle_event(&tab_press(), now));
    let mut update = watched_config_snapshot();
    set_auth_status(&mut update, "provider", "anthropic", "oauthConfigured", true);
    drop(app.handle_event(&AppEvent::Client(Box::new(update)), now));
    let form = app.provider_section().config_form().expect("provider draft");
    assert_eq!(form.name, "anthropic-draft");
    assert_eq!(form.field, 1);
}

#[test]
fn config_statuses_render_all_secret_safe_kinds_and_mcp_has_no_registration_fields() {
    for (kind, label) in [
        ("oauthConfigured", "connected (OAuth)"),
        ("apiKeyConfigured", "connected (API key)"),
        ("expired", "expired"),
        ("authorizationRequired", "authorization required"),
        ("unsupported", "unsupported"),
    ] {
        let mut snapshot = watched_config_snapshot();
        snapshot
            .config
            .current
            .as_mut()
            .unwrap()
            .providers
            .get_mut("anthropic")
            .unwrap()
            .url
            .clear();
        set_auth_status(&mut snapshot, "provider", "anthropic", kind, false);
        let mut app = open_config_app_with(snapshot);
        let rendered = render_app(&app);
        assert!(
            label.split_whitespace().all(|part| rendered.contains(part)),
            "{kind}: {rendered}"
        );
        if kind == "oauthConfigured" {
            assert!(rendered.contains("a reauthorize"));
        }
        if kind == "authorizationRequired" {
            assert!(rendered.contains("a reauthorize"));
        }
        if kind == "unsupported" {
            assert_eq!(
                app.handle_event(&char_press('a'), Instant::now()),
                AppAction::ClientCommand(ClientCommand::BeginAuth {
                    target: ClientAuthTarget {
                        kind: "provider".to_owned(),
                        key: "anthropic".to_owned(),
                    }
                })
            );
        }
    }

    let mut app = open_config_app_with_mcp();
    drop(app.handle_event(&tab_press(), Instant::now()));
    drop(app.handle_event(&ctrl_x_press(), Instant::now()));
    drop(app.handle_event(&char_press('c'), Instant::now()));
    let rendered = render_app(&app);
    assert!(!rendered.contains("client ID"));
    assert!(!rendered.contains("authorize URL"));
    assert!(!rendered.contains("registration"));
}

#[test]
fn config_authorize_and_reauthorize_use_explicit_shared_attempt_overlay() {
    for kind in ["authorizationRequired", "oauthConfigured"] {
        let mut snapshot = provider_oauth_config_snapshot();
        set_auth_status(&mut snapshot, "provider", "anthropic", kind, true);
        let mut app = open_config_app_with(snapshot.clone());
        let now = Instant::now();
        assert_eq!(
            app.handle_event(&char_press('a'), now),
            AppAction::ClientCommand(ClientCommand::BeginAuth {
                target: ClientAuthTarget {
                    kind: "provider".to_owned(),
                    key: "anthropic".to_owned(),
                }
            })
        );
        assert!(render_app(&app).contains("OAuth authorization"));

        let mut loading = snapshot.clone();
        set_auth_begin_operation(&mut loading, "begin-1", "loading", None);
        drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
        let mut ready = snapshot;
        set_auth_begin_operation(&mut ready, "begin-1", "ready", None);
        ready.auth.active_attempt = Some(test_auth_attempt("provider", "anthropic", "pending"));
        drop(app.handle_event(&AppEvent::Client(Box::new(ready)), now));
        let rendered = render_app(&app);
        assert!(rendered.contains("Authorization requested from configuration."));
        assert!(rendered.contains("https://id.example/authorize"));
        assert_eq!(app.provider_section().selected_provider(), 0);
    }
}

#[test]
fn config_auth_modal_retries_through_the_shared_tracker() {
    let mut snapshot = provider_oauth_config_snapshot();
    set_auth_status(
        &mut snapshot,
        "provider",
        "anthropic",
        "authorizationRequired",
        false,
    );
    let mut app = open_config_app_with(snapshot.clone());
    let now = Instant::now();
    let target = matrix_target("provider");

    assert_eq!(
        app.handle_event(&char_press('a'), now),
        AppAction::ClientCommand(ClientCommand::BeginAuth {
            target: target.clone(),
        })
    );
    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);

    let mut loading = snapshot.clone();
    set_auth_begin_operation_for(
        &mut loading,
        "begin-retry",
        "loading",
        None,
        target.clone(),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
    set_auth_begin_operation_for(
        &mut snapshot,
        "begin-retry",
        "failed",
        Some("begin denied"),
        target.clone(),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert!(render_app(&app).contains("begin denied"));

    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::BeginAuth {
            target: target.clone(),
        })
    );
    assert!(app.config_auth_action_pending(&target));
    assert!(!render_app(&app).contains("begin denied"));
}

#[test]
fn config_manual_completion_closes_the_shared_modal() {
    let now = Instant::now();
    let target = matrix_target("provider");

    let mut complete_base = provider_oauth_config_snapshot();
    set_auth_status(
        &mut complete_base,
        "provider",
        "anthropic",
        "authorizationRequired",
        false,
    );
    let mut complete_app = open_config_app_with(complete_base.clone());
    drop(complete_app.handle_event(&char_press('a'), now));
    let mut complete_loading = complete_base.clone();
    set_auth_begin_operation_for(
        &mut complete_loading,
        "begin-complete",
        "loading",
        None,
        target.clone(),
    );
    drop(complete_app.handle_event(&AppEvent::Client(Box::new(complete_loading)), now));
    complete_base.auth.active_attempt = Some(test_auth_attempt(
        "provider",
        "anthropic",
        "pending",
    ));
    set_auth_begin_operation_for(
        &mut complete_base,
        "begin-complete",
        "ready",
        None,
        target,
    );
    drop(complete_app.handle_event(&AppEvent::Client(Box::new(complete_base.clone())), now));
    type_text(&mut complete_app, "completion-code", now);
    assert!(matches!(
        complete_app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::CompleteAuth { .. })
    ));
    let mut completing = complete_base.clone();
    set_auth_complete_operation(&mut completing, "complete-1", "loading", None);
    drop(complete_app.handle_event(&AppEvent::Client(Box::new(completing)), now));
    complete_base.auth.active_attempt = None;
    set_auth_complete_operation(&mut complete_base, "complete-1", "ready", None);
    drop(complete_app.handle_event(&AppEvent::Client(Box::new(complete_base)), now));
    assert!(complete_app.outbound_auth_target().is_none());
    assert!(!render_app(&complete_app).contains("OAuth authorization"));
}

#[test]
fn config_cancellation_closes_the_shared_modal() {
    let now = Instant::now();
    let mut cancel_base = provider_oauth_config_snapshot();
    set_auth_status(
        &mut cancel_base,
        "provider",
        "anthropic",
        "authorizationRequired",
        false,
    );
    cancel_base.auth.complete.error = Some("stale completion error".to_owned());
    let mut cancel_app = open_config_app_with(cancel_base.clone());
    drop(cancel_app.handle_event(&char_press('a'), now));
    let mut cancel_begin_loading = cancel_base.clone();
    set_auth_begin_operation(
        &mut cancel_begin_loading,
        "begin-cancel",
        "loading",
        None,
    );
    drop(cancel_app.handle_event(&AppEvent::Client(Box::new(cancel_begin_loading)), now));
    cancel_base.auth.active_attempt = Some(test_auth_attempt(
        "provider",
        "anthropic",
        "pending",
    ));
    set_auth_begin_operation(&mut cancel_base, "begin-cancel", "ready", None);
    drop(cancel_app.handle_event(&AppEvent::Client(Box::new(cancel_base.clone())), now));
    assert!(!render_app(&cancel_app).contains("stale completion error"));
    assert!(matches!(
        cancel_app.handle_event(&escape_press(), now),
        AppAction::ClientCommand(ClientCommand::CancelAuth { .. })
    ));
    let mut canceling = cancel_base.clone();
    set_auth_cancel_operation(&mut canceling, "cancel-1", "loading", None);
    drop(cancel_app.handle_event(&AppEvent::Client(Box::new(canceling)), now));
    cancel_base.auth.active_attempt = None;
    set_auth_cancel_operation(&mut cancel_base, "cancel-1", "ready", None);
    drop(cancel_app.handle_event(&AppEvent::Client(Box::new(cancel_base)), now));
    assert!(cancel_app.outbound_auth_target().is_none());
    assert!(!render_app(&cancel_app).contains("OAuth authorization"));
}

#[test]
fn config_explicit_oauth_attempt_renders_snapshot() {
    let mut snapshot = provider_oauth_config_snapshot();
    set_auth_status(
        &mut snapshot,
        "provider",
        "anthropic",
        "authorizationRequired",
        false,
    );
    let mut app = open_config_app_with(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&char_press('a'), now));
    let mut loading = snapshot.clone();
    set_auth_begin_operation(&mut loading, "begin-explicit", "loading", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
    snapshot.auth.active_attempt = Some(test_auth_attempt("provider", "anthropic", "pending"));
    set_auth_begin_operation(&mut snapshot, "begin-explicit", "ready", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));

    let rendered = render_app(&app);
    assert!(!rendered.contains("Registration:"));
    insta::assert_snapshot!("config_explicit_oauth_attempt", rendered);

    let mut succeeded = provider_oauth_config_snapshot();
    succeeded.auth.active_attempt = Some(test_auth_attempt("provider", "anthropic", "succeeded"));
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(succeeded)), now),
        AppAction::ClientCommand(ClientCommand::RequestAuthStatuses)
    );
    assert_eq!(app.provider_section().selected_provider(), 0);
    assert!(!render_app(&app).contains("OAuth authorization"));
}

#[test]
fn config_provider_oauth_fields_render_snapshot_without_secret_field() {
    let mut snapshot = watched_config_snapshot();
    let provider = snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.oauth_compatible = true;
    provider.oauth_settings = Some(OAuthProviderView {
        name: "Anthropic OAuth".to_owned(),
        authorize_url: "https://id.example/authorize".to_owned(),
        token_url: "https://id.example/token".to_owned(),
        client_id: "public-client".to_owned(),
        redirect_uri: "http://127.0.0.1/callback".to_owned(),
        scopes: vec!["openid".to_owned(), "profile".to_owned()],
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: vec![OAuthUrlParamView {
            key: "audience".to_owned(),
            value: "api".to_owned(),
        }],
        jwt_metadata: vec![OAuthJwtMetadataView {
            token: OAuthTokenFieldView::Id,
            metadata_key: "tenant".to_owned(),
            claim_names: vec!["tid".to_owned()],
            required: true,
        }],
        send_state_in_token_request: true,
    });
    let mut app = open_config_app_with(snapshot);
    drop(app.handle_event(&enter_press(), Instant::now()));
    let rendered = render_app(&app);
    assert!(!rendered.to_ascii_lowercase().contains("client secret"));
    assert!(rendered.contains("..."));
    insta::assert_snapshot!("config_provider_oauth_fields", rendered);
}

#[test]
fn config_provider_auth_status_actions_render_snapshot() {
    let app = open_config_app_with(provider_connected_status_snapshot());

    insta::assert_snapshot!("config_provider_auth_status_actions", render_app(&app));
}

#[test]
fn config_provider_oauth_fixture_has_complete_public_client_settings() {
    let snapshot = provider_oauth_config_snapshot();
    let provider = snapshot
        .config
        .current
        .as_ref()
        .unwrap()
        .providers
        .get("anthropic")
        .unwrap();
    let oauth = provider.oauth_settings.as_ref().expect("OAuth settings");
    assert!(provider.oauth_compatible);
    assert_eq!(oauth.authorize_url, "https://id.example/authorize");
    assert_eq!(oauth.token_url, "https://id.example/token");
    assert_eq!(oauth.client_id, "public-client");
    assert_eq!(oauth.redirect_uri, "http://127.0.0.1/callback");
    assert_eq!(
        oauth.token_request_style,
        OAuthTokenRequestStyleView::FormUrlencoded
    );
}

#[test]
fn config_provider_auth_reset_confirmation_renders_snapshot() {
    let mut app = open_config_app_with(provider_connected_status_snapshot());
    assert_eq!(
        app.handle_event(&char_press('r'), Instant::now()),
        AppAction::Continue
    );

    insta::assert_snapshot!("config_provider_auth_reset_confirmation", render_app(&app));
}

#[test]
fn config_provider_oauth_focus_scrolls_deterministically_on_narrow_terminal() {
    let mut snapshot = watched_config_snapshot();
    let provider = snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.oauth_compatible = true;
    provider.oauth_settings = Some(OAuthProviderView {
        name: "OAuth".to_owned(),
        authorize_url: String::new(),
        token_url: String::new(),
        client_id: "public".to_owned(),
        redirect_uri: String::new(),
        scopes: Vec::new(),
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: Vec::new(),
        jwt_metadata: vec![OAuthJwtMetadataView {
            token: OAuthTokenFieldView::Id,
            metadata_key: "tenant".to_owned(),
            claim_names: vec!["tid".to_owned()],
            required: true,
        }],
        send_state_in_token_request: false,
    });
    let mut app = open_config_app_with(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&enter_press(), now));
    for _ in 0..13 {
        drop(app.handle_event(&tab_press(), now));
    }
    let rendered = render_app_with_size(&app, 60, 18);
    assert!(rendered.contains("JWT mappings JSON"), "{rendered}");
    assert!(rendered.contains("* JWT mappings JSON"), "{rendered}");
}

#[test]
fn config_force_refresh_reconciles_matching_key_and_surfaces_failure() {
    let mut base = watched_config_snapshot();
    set_auth_status(&mut base, "provider", "anthropic", "oauthConfigured", true);
    let mut app = open_config_app_with(base.clone());
    let now = Instant::now();
    assert!(matches!(
        app.handle_event(&char_press('f'), now),
        AppAction::ClientCommand(ClientCommand::RefreshAuth { .. })
    ));

    let mut loading = base.clone();
    set_auth_refresh_operation(&mut loading, "refresh-1", "loading", None);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(loading)), now),
        AppAction::Continue
    );
    let mut unrelated = base.clone();
    set_auth_refresh_operation(&mut unrelated, "refresh-2", "ready", None);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(unrelated)), now),
        AppAction::Continue
    );
    let mut ready = base.clone();
    set_auth_refresh_operation(&mut ready, "refresh-1", "ready", None);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready)), now),
        AppAction::ClientCommand(ClientCommand::RequestAuthStatuses)
    );

    assert!(matches!(
        app.handle_event(&char_press('f'), now),
        AppAction::ClientCommand(ClientCommand::RefreshAuth { .. })
    ));
    let mut failed_loading = base.clone();
    set_auth_refresh_operation(&mut failed_loading, "refresh-3", "loading", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(failed_loading)), now));
    let mut failed = base;
    set_auth_refresh_operation(&mut failed, "refresh-3", "failed", Some("refresh denied"));
    drop(app.handle_event(&AppEvent::Client(Box::new(failed)), now));
    assert!(render_app(&app).contains("error: refresh denied"));
    drop(app.handle_event(&char_press('r'), now));
    drop(app.handle_event(&enter_press(), now));
    assert!(!render_app(&app).contains("error: refresh denied"));
}

#[test]
fn config_reset_requires_confirmation_and_supports_cancellation() {
    let mut snapshot = patched_mcp_snapshot();
    set_auth_status(&mut snapshot, "mcpServer", "files", "oauthConfigured", true);
    let base = snapshot.clone();
    let mut app = open_config_app_with(snapshot);
    let now = Instant::now();
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&char_press('r'), now));
    assert!(render_app(&app).contains("Reset credential?"));
    drop(app.handle_event(&char_press('n'), now));
    assert!(!render_app(&app).contains("Reset credential?"));
    drop(app.handle_event(&char_press('r'), now));
    assert_eq!(
        app.handle_event(&enter_press(), now),
        AppAction::ClientCommand(ClientCommand::LogoutAuth {
            target: ClientAuthTarget {
                kind: "mcpServer".to_owned(),
                key: "files".to_owned(),
            }
        })
    );
    let mut loading = base.clone();
    set_auth_logout_operation(&mut loading, "logout-1", "loading", None);
    drop(app.handle_event(&AppEvent::Client(Box::new(loading)), now));
    let mut ready = base;
    set_auth_logout_operation(&mut ready, "logout-1", "ready", None);
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(ready)), now),
        AppAction::ClientCommand(ClientCommand::RequestAuthStatuses)
    );
    assert!(!render_app(&app).contains("Reset credential?"));
}

#[test]
fn config_reset_confirmation_clears_when_its_target_is_removed() {
    let mut snapshot = patched_mcp_snapshot();
    set_auth_status(&mut snapshot, "mcpServer", "files", "oauthConfigured", true);
    let mut app = open_config_app_with(snapshot.clone());
    let now = Instant::now();
    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&char_press('r'), now));
    assert!(render_app(&app).contains("Reset credential?"));

    snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .mcp_servers
        .remove("files");
    drop(app.handle_event(&AppEvent::Client(Box::new(snapshot)), now));
    assert!(!render_app(&app).contains("Reset credential?"));
    drop(app.handle_event(&tab_press(), now));
    assert_eq!(app.active_config_tab(), ConfigTab::InferenceDefaults);
}

#[test]
fn config_provider_and_mcp_actions_follow_complete_status_matrix() {
    let cases = [
        (None, false, false, false, false),
        (Some("apiKeyConfigured"), true, false, false, false),
        (Some("notConfigured"), true, true, false, false),
        (Some("oauthConfigured"), true, true, true, true),
        (Some("expired"), true, true, false, true),
        (Some("authorizationRequired"), true, true, false, true),
        (Some("unsupported"), false, false, false, false),
    ];
    for target_kind in ["provider", "mcpServer"] {
        for (kind, oauth_supported, can_authorize, can_refresh, can_reset) in cases {
            let now = Instant::now();
            let expected_target = matrix_target(target_kind);

            let mut authorize_app = open_matrix_app(
                target_kind,
                kind,
                oauth_supported,
                can_refresh,
            );
            let authorize = authorize_app.handle_event(&char_press('a'), now);
            if can_authorize {
                assert_eq!(
                    authorize,
                    AppAction::ClientCommand(ClientCommand::BeginAuth {
                        target: expected_target.clone(),
                    }),
                    "authorize matrix failed for {target_kind} {kind:?}"
                );
            } else {
                assert_eq!(authorize, AppAction::Continue);
            }

            let mut refresh_app = open_matrix_app(
                target_kind,
                kind,
                oauth_supported,
                can_refresh,
            );
            let refresh = refresh_app.handle_event(&char_press('f'), now);
            if can_refresh {
                assert_eq!(
                    refresh,
                    AppAction::ClientCommand(ClientCommand::RefreshAuth {
                        target: expected_target.clone(),
                    }),
                    "refresh matrix failed for {target_kind} {kind:?}"
                );
            } else {
                assert_eq!(refresh, AppAction::Continue);
            }

            let mut reset_app = open_matrix_app(
                target_kind,
                kind,
                oauth_supported,
                can_refresh,
            );
            assert_eq!(
                reset_app.handle_event(&char_press('r'), now),
                AppAction::Continue
            );
            assert_eq!(
                render_app(&reset_app).contains("Reset credential?"),
                can_reset,
                "reset matrix failed for {target_kind} {kind:?}"
            );
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "out-of-order snapshots document the complete cross-target operation sequence"
)]
fn config_cross_target_refresh_overlap_binds_target_and_key() {
    let mut base = patched_mcp_snapshot();
    set_auth_status(&mut base, "provider", "anthropic", "oauthConfigured", true);
    set_auth_status(&mut base, "mcpServer", "files", "oauthConfigured", true);
    let mut app = open_config_app_with(base.clone());
    let now = Instant::now();

    assert!(matches!(
        app.handle_event(&char_press('f'), now),
        AppAction::ClientCommand(ClientCommand::RefreshAuth {
            target: ClientAuthTarget { ref kind, .. }
        }) if kind == "provider"
    ));
    drop(app.handle_event(&tab_press(), now));
    assert!(matches!(
        app.handle_event(&char_press('f'), now),
        AppAction::ClientCommand(ClientCommand::RefreshAuth {
            target: ClientAuthTarget { ref kind, .. }
        }) if kind == "mcpServer"
    ));

    let mut provider_ready_early = base.clone();
    set_auth_refresh_operation_for(
        &mut provider_ready_early,
        "provider-refresh",
        "ready",
        None,
        matrix_target("provider"),
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(provider_ready_early)), now),
        AppAction::Continue
    );

    let mut mcp_loading = base.clone();
    set_auth_refresh_operation_for(
        &mut mcp_loading,
        "mcp-refresh",
        "loading",
        None,
        matrix_target("mcpServer"),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(mcp_loading)), now));
    let mut mcp_ready = base.clone();
    set_auth_refresh_operation_for(
        &mut mcp_ready,
        "mcp-refresh",
        "ready",
        None,
        matrix_target("mcpServer"),
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(mcp_ready)), now),
        AppAction::ClientCommand(ClientCommand::RequestAuthStatuses)
    );

    drop(app.handle_event(&tab_press(), now));
    drop(app.handle_event(&tab_press(), now));
    assert_eq!(app.active_config_tab(), ConfigTab::Providers);
    assert!(app.config_auth_action_pending(&matrix_target("provider")));

    let mut provider_loading = base.clone();
    set_auth_refresh_operation_for(
        &mut provider_loading,
        "provider-refresh",
        "loading",
        None,
        matrix_target("provider"),
    );
    drop(app.handle_event(&AppEvent::Client(Box::new(provider_loading)), now));
    let mut provider_ready = base;
    set_auth_refresh_operation_for(
        &mut provider_ready,
        "provider-refresh",
        "ready",
        None,
        matrix_target("provider"),
    );
    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(provider_ready)), now),
        AppAction::ClientCommand(ClientCommand::RequestAuthStatuses)
    );
    assert!(!app.config_auth_action_pending(&matrix_target("provider")));
}

#[test]
fn config_targeted_begin_and_reset_failures_settle_only_the_initiator() {
    let now = Instant::now();
    let mut mcp_base = patched_mcp_snapshot();
    set_auth_status(
        &mut mcp_base,
        "mcpServer",
        "files",
        "authorizationRequired",
        false,
    );
    let mut mcp_app = open_config_app_with(mcp_base.clone());
    drop(mcp_app.handle_event(&tab_press(), now));
    drop(mcp_app.handle_event(&char_press('a'), now));
    let mut begin_loading = mcp_base.clone();
    set_auth_begin_operation_for(
        &mut begin_loading,
        "mcp-begin",
        "loading",
        None,
        matrix_target("mcpServer"),
    );
    drop(mcp_app.handle_event(&AppEvent::Client(Box::new(begin_loading)), now));
    let mut begin_failed = mcp_base;
    set_auth_begin_operation_for(
        &mut begin_failed,
        "mcp-begin",
        "failed",
        Some("MCP authorization rejected"),
        matrix_target("mcpServer"),
    );
    drop(mcp_app.handle_event(&AppEvent::Client(Box::new(begin_failed)), now));
    assert!(!mcp_app.config_auth_action_pending(&matrix_target("mcpServer")));
    assert!(render_app(&mcp_app).contains("MCP authorization rejected"));

    let mut provider_base = watched_config_snapshot();
    set_auth_status(
        &mut provider_base,
        "provider",
        "anthropic",
        "authorizationRequired",
        false,
    );
    let mut provider_app = open_config_app_with(provider_base.clone());
    drop(provider_app.handle_event(&char_press('r'), now));
    drop(provider_app.handle_event(&enter_press(), now));
    let mut logout_loading = provider_base.clone();
    set_auth_logout_operation_for(
        &mut logout_loading,
        "provider-logout",
        "loading",
        None,
        matrix_target("provider"),
    );
    drop(provider_app.handle_event(&AppEvent::Client(Box::new(logout_loading)), now));
    let mut logout_failed = provider_base;
    set_auth_logout_operation_for(
        &mut logout_failed,
        "provider-logout",
        "failed",
        Some("provider reset rejected"),
        matrix_target("provider"),
    );
    drop(provider_app.handle_event(&AppEvent::Client(Box::new(logout_failed)), now));
    assert!(!provider_app.config_auth_action_pending(&matrix_target("provider")));
    assert!(render_app(&provider_app).contains("provider reset rejected"));
}

#[test]
fn config_malformed_oauth_json_preserves_mappings_and_keeps_focus() {
    for (field, expected_error) in [
        (12, "authorization parameters must be a valid JSON array"),
        (13, "JWT metadata mappings must be a valid JSON array"),
    ] {
        let mut app = open_config_app_with(oauth_mapping_snapshot());
        let now = Instant::now();
        drop(app.handle_event(&enter_press(), now));
        for _ in 0..field {
            drop(app.handle_event(&tab_press(), now));
        }
        for _ in 0..256 {
            drop(app.handle_event(&backspace_press(), now));
        }
        type_text(&mut app, "{malformed", now);
        assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
        let form = app.provider_section().config_form().expect("form stays open");
        assert_eq!(form.field, field);
        let rendered = render_app(&app);
        let visible_error = if field == 12 {
            "error: authorization parameters"
        } else {
            "error: JWT metadata mappings"
        };
        assert!(rendered.contains(visible_error), "{expected_error}: {rendered}");

        for _ in 0..32 {
            drop(app.handle_event(&backspace_press(), now));
        }
        let valid = if field == 12 {
            r#"[{"key":"audience","value":"api"}]"#
        } else {
            r#"[{"token":"id","metadataKey":"tenant","claimNames":["tid"],"required":true}]"#
        };
        type_text(&mut app, valid, now);
        let AppAction::ClientCommand(ClientCommand::UpdateProvider { provider, .. }) =
            app.handle_event(&enter_press(), now)
        else {
            panic!("valid structured edit should save");
        };
        let oauth = provider.oauth_settings.expect("OAuth settings remain");
        assert_eq!(oauth.auth_url_params.len(), 1);
        assert_eq!(oauth.jwt_metadata.len(), 1);
    }
}

#[test]
fn config_mcp_status_details_are_safe_bounded_and_render_registration_methods() {
    for (method, label) in [
        ("preregistered", "Preregistered"),
        ("clientMetadata", "Client metadata"),
        ("dynamic", "Dynamic"),
    ] {
        let mut snapshot = patched_mcp_snapshot();
        let config = snapshot.config.current.as_mut().unwrap();
        let mut server = config.mcp_servers.remove("files").unwrap();
        server.name = "remote".to_owned();
        server.kind = McpServerTypeView::Sse;
        server.command.clear();
        server.url = "https://mcp.example/sse".to_owned();
        config.mcp_servers.insert("remote".to_owned(), server);
        let mut status = auth_status("mcpServer", "remote", "authorizationRequired", false);
        status.message = if method == "dynamic" {
            "Authorization is required before this MCP server can be used.".to_owned()
        } else {
            format!("credential\u{1b}[31m {}", "x".repeat(600))
        };
        status.expires_at_ms = 1_800_000_000_000;
        status.registration_method = method.to_owned();
        snapshot.auth.statuses.insert("mcp:remote".to_owned(), status);
        let mut app = open_config_app_with(snapshot);
        drop(app.handle_event(&tab_press(), Instant::now()));
        let rendered = render_app(&app);
        assert!(!rendered.contains('\u{1b}'));
        if method == "dynamic" {
            assert!(rendered.contains("message: Authorization is required"));
        } else {
            assert!(rendered.contains("message: credential [31m"));
            assert!(rendered.contains("..."));
        }
        assert!(rendered.contains("expires: 2027-01-15 08:00 UTC"));
        assert!(rendered.contains(&format!("registration: {label}")));
        if method == "dynamic" {
            insta::assert_snapshot!("config_mcp_auth_status_details", rendered);
        }
    }
}

fn open_matrix_app(
    target_kind: &str,
    kind: Option<&str>,
    oauth_supported: bool,
    can_refresh: bool,
) -> App {
    let mut snapshot = patched_mcp_snapshot();
    let target = matrix_target(target_kind);
    if let Some(kind) = kind {
        set_auth_status_with_methods(
            &mut snapshot,
            target_kind,
            &target.key,
            kind,
            can_refresh,
            if oauth_supported {
                vec!["oauthManual".to_owned()]
            } else {
                Vec::new()
            },
        );
    }
    let mut app = open_config_app_with(snapshot);
    if target_kind == "mcpServer" {
        drop(app.handle_event(&tab_press(), Instant::now()));
    }
    app
}

fn matrix_target(target_kind: &str) -> ClientAuthTarget {
    ClientAuthTarget {
        kind: target_kind.to_owned(),
        key: if target_kind == "mcpServer" {
            "files".to_owned()
        } else {
            "anthropic".to_owned()
        },
    }
}

fn oauth_mapping_snapshot() -> ClientSnapshot {
    let mut snapshot = watched_config_snapshot();
    let provider = snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.oauth_compatible = true;
    provider.oauth_settings = Some(OAuthProviderView {
        name: "OAuth".to_owned(),
        authorize_url: String::new(),
        token_url: String::new(),
        client_id: "public".to_owned(),
        redirect_uri: String::new(),
        scopes: Vec::new(),
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: vec![OAuthUrlParamView {
            key: "audience".to_owned(),
            value: "api".to_owned(),
        }],
        jwt_metadata: vec![OAuthJwtMetadataView {
            token: OAuthTokenFieldView::Id,
            metadata_key: "tenant".to_owned(),
            claim_names: vec!["tid".to_owned()],
            required: true,
        }],
        send_state_in_token_request: false,
    });
    snapshot
}

fn provider_connected_status_snapshot() -> ClientSnapshot {
    let mut snapshot = provider_oauth_config_snapshot();
    let provider = snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.url.clear();
    let mut status = auth_status("provider", "anthropic", "oauthConfigured", true);
    "OAuth credential is ready for provider requests.".clone_into(&mut status.message);
    status.expires_at_ms = 1_800_000_000_000;
    snapshot
        .auth
        .statuses
        .insert("provider:anthropic".to_owned(), status);
    snapshot
}

fn provider_oauth_config_snapshot() -> ClientSnapshot {
    let mut snapshot = watched_config_snapshot();
    let provider = snapshot
        .config
        .current
        .as_mut()
        .unwrap()
        .providers
        .get_mut("anthropic")
        .unwrap();
    provider.oauth_compatible = true;
    provider.oauth_settings = Some(OAuthProviderView {
        name: "Example public OAuth".to_owned(),
        authorize_url: "https://id.example/authorize".to_owned(),
        token_url: "https://id.example/token".to_owned(),
        client_id: "public-client".to_owned(),
        redirect_uri: "http://127.0.0.1/callback".to_owned(),
        scopes: vec!["openid".to_owned(), "profile".to_owned()],
        token_request_style: OAuthTokenRequestStyleView::FormUrlencoded,
        auth_url_params: Vec::new(),
        jwt_metadata: Vec::new(),
        send_state_in_token_request: false,
    });
    snapshot
}

fn type_text(app: &mut App, value: &str, now: Instant) {
    for character in value.chars() {
        drop(app.handle_event(&char_press(character), now));
    }
}

fn test_auth_attempt(kind: &str, key: &str, lifecycle: &str) -> AuthAttemptSnapshot {
    AuthAttemptSnapshot {
        target: ClientAuthTarget {
            kind: kind.to_owned(),
            key: key.to_owned(),
        },
        attempt_id: "attempt-1".to_owned(),
        auth_url: "https://id.example/authorize".to_owned(),
        verification_url: String::new(),
        user_code: String::new(),
        expires_at_ms: 0,
        poll_interval_seconds: 0,
        messages: Vec::new(),
        completion_mode: "manual".to_owned(),
        registration_method: "unknown".to_owned(),
        lifecycle: lifecycle.to_owned(),
        failure_reason: String::new(),
    }
}

fn set_auth_begin_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) {
    set_auth_begin_operation_for(
        snapshot,
        key,
        status,
        error,
        matrix_target("provider"),
    );
}

fn set_auth_begin_operation_for(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
    target: ClientAuthTarget,
) {
    snapshot.auth.begin = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::AuthBegin {
            status,
            error: error.map(str::to_owned),
            target,
            attempt: snapshot.auth.active_attempt.clone(),
        },
    );
}

fn set_auth_refresh_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) {
    set_auth_refresh_operation_for(
        snapshot,
        key,
        status,
        error,
        matrix_target("provider"),
    );
}

fn set_auth_refresh_operation_for(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
    target: ClientAuthTarget,
) {
    snapshot.auth.refresh = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::AuthRefresh {
            status,
            error: error.map(str::to_owned),
            target,
        },
    );
}

fn set_auth_logout_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) {
    set_auth_logout_operation_for(
        snapshot,
        key,
        status,
        error,
        matrix_target("mcpServer"),
    );
}

fn set_auth_logout_operation_for(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
    target: ClientAuthTarget,
) {
    snapshot.auth.logout = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::AuthLogout {
            status,
            error: error.map(str::to_owned),
            target,
        },
    );
}

fn set_auth_complete_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) {
    snapshot.auth.complete = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::AuthComplete {
            status,
            error: error.map(str::to_owned),
        },
    );
}

fn set_auth_cancel_operation(
    snapshot: &mut ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) {
    snapshot.auth.cancel = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::AuthCancel {
            status,
            error: error.map(str::to_owned),
        },
    );
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
    assert_eq!(provider.kind, ProviderKindView::OpenAiChatCompletions);
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
        expected_config_entry_action()
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
        expected_config_entry_action()
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
        expected_config_entry_action()
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
        expected_config_entry_action()
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
            expected_config_entry_action()
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
        expected_config_entry_action()
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
        expected_config_entry_action()
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
    let logo_cell = &buffer[(24, 7)];

    assert_eq!(logo_cell.fg, Color::Rgb(190, 132, 242));
    assert_eq!(logo_cell.bg, THEME.panel());
}
