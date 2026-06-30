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
    assert!(render_app(&app).contains("Space MCP"));

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
    assert_eq!(app.active_config_tab(), ConfigTab::Defaults);

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

    let AppAction::ClientCommand(ClientCommand::SetRuntimeDefaults {
        defaults: Some(defaults),
    }) = app.handle_event(&enter_press(), now)
    else {
        panic!("expected SetRuntimeDefaults command");
    };
    assert_eq!(defaults.provider_key, "anthropic");
    assert_eq!(defaults.model_id, "sonnet");
}

#[test]
fn config_defaults_opens_mcp_selector_modal_and_stages_toggle() {
    let mut app = open_config_app_with_mcp();
    let now = Instant::now();

    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.active_config_tab(), ConfigTab::Defaults);

    assert_eq!(app.handle_event(&char_press(' '), now), AppAction::Continue);
    let rendered = render_app(&app);
    assert!(rendered.contains("Select default MCP"));
    assert!(rendered.contains("MCP Servers"));
    assert!(rendered.contains("> [ ] files"));

    assert_eq!(app.handle_event(&char_press(' '), now), AppAction::Continue);
    assert!(render_app(&app).contains("> [x] files"));

    assert_eq!(app.handle_event(&enter_press(), now), AppAction::Continue);
    assert!(!render_app(&app).contains("Select default MCP"));

    let AppAction::ClientCommand(ClientCommand::SetRuntimeDefaults {
        defaults: Some(defaults),
    }) = app.handle_event(&enter_press(), now)
    else {
        panic!("expected SetRuntimeDefaults command");
    };
    assert!(!defaults.mcp_default_enabled);
    assert_eq!(defaults.enabled_mcp_server_keys, vec!["files"]);
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
    let logo_cell = &buffer[(24, 7)];

    assert_eq!(logo_cell.fg, Color::Rgb(190, 132, 242));
}
