use std::{
    collections::{BTreeMap, HashMap},
    time::Instant,
};

use waltr_client::{
    ClientRuntime, Configuration, InferenceDefaultsView, McpServerTypeView, McpServerView,
    ModelCapabilitiesView, ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView,
    ProviderKindView, ProviderView,
};
use waltr_server::{
    config::ConfigStore as _,
    config_service::ConfigService,
    test_support::{default_config_builder, setup_server_with_services, temp_root},
};
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{char_press, enter_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_config_screen,
    },
};

#[tokio::test]
async fn reset_replaces_all_config_sections_with_real_server_defaults() {
    let config_service = ConfigService::new(temp_root(), default_config_builder())
        .expect("config service should start");
    config_service
        .store()
        .save(custom_configuration())
        .await
        .expect("custom configuration should save");
    let server = setup_server_with_services(config_service);
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_config_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "received"
            && snapshot
                .config
                .current
                .as_ref()
                .is_some_and(|config| config.providers.contains_key("custom"))
    })
    .await;

    let now = Instant::now();
    assert!(render_app(&app).contains("R Reset configuration"));
    assert_eq!(app.handle_event(&char_press('R'), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    assert_eq!(
        action,
        AppAction::ClientCommand(waltr_client::ClientCommand::ResetConfig)
    );
    let reset = dispatch_app_command(&mut stream, &mut app, action).await;

    let config = reset.config.current.expect("reset configuration");
    for provider in ["anthropic", "claude-code", "openai", "codex"] {
        assert!(
            config.providers.contains_key(provider),
            "missing {provider}"
        );
    }
    assert!(!config.providers.contains_key("custom"));
    let waltr = config
        .mcp_servers
        .get("waltr")
        .expect("ordinary Waltr MCP entry");
    assert_eq!(waltr.kind, McpServerTypeView::Sse);
    assert_eq!(waltr.name, "waltr");
    assert!(waltr.url.ends_with("/mcp"));
    assert!(!config.mcp_servers.contains_key("custom"));
    assert!(config.inference_defaults.is_none());
    assert!(render_app(&app).contains("configuration reset to server defaults"));
}

fn custom_configuration() -> Configuration {
    Configuration {
        etag: String::new(),
        providers: HashMap::from([(
            "custom".to_owned(),
            ProviderView {
                kind: ProviderKindView::AnthropicMessages,
                name: "custom".to_owned(),
                url: "https://provider.example/messages".to_owned(),
                headers: BTreeMap::new(),
                oauth_compatible: false,
                oauth_settings: None,
                models: BTreeMap::from([(
                    "custom-model".to_owned(),
                    ModelView {
                        name: "Custom model".to_owned(),
                        kind: ModelTypeView::Llm,
                        options: ModelOptionsView {
                            temperature: None,
                            top_p: None,
                            top_k: None,
                            min_p: None,
                            presence_penalty: None,
                            repetition_penalty: None,
                            reasoning_efforts: Vec::new(),
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
            .into(),
        )]),
        mcp_servers: HashMap::from([(
            "custom".to_owned(),
            McpServerView {
                name: "custom".to_owned(),
                kind: McpServerTypeView::Sse,
                command: String::new(),
                url: "https://mcp.example/sse".to_owned(),
            }
            .into(),
        )]),
        inference_defaults: Some(
            InferenceDefaultsView {
                provider_key: "custom".to_owned(),
                model_id: "custom-model".to_owned(),
                reasoning_effort: None,
            }
            .into(),
        ),
    }
}
