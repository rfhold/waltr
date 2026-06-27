use std::{
    collections::{BTreeMap, HashMap},
    time::Instant,
};

use waltr_client::{
    ClientRuntime, Configuration, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView, ProviderKindView, ProviderView,
    RuntimeDefaultsView,
};
use waltr_server::{
    config::ConfigStore as _,
    config_service::ConfigService,
    test_support::{TestServer, setup_server_with_services, temp_root},
};
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{char_press, enter_press, tab_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_config_screen,
    },
};

#[tokio::test]
async fn runtime_defaults_snapshot_and_save_use_real_server() {
    let server = setup_runtime_defaults_server().await;
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_config_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.config.watch.status == "received" && snapshot.config.current.is_some()
    })
    .await;

    let now = Instant::now();
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&tab_press(), now), AppAction::Continue);
    insta::assert_snapshot!("config_defaults_seeded_real_server", render_app(&app));

    assert_eq!(app.handle_event(&char_press('m'), now), AppAction::Continue);
    let action = app.handle_event(&enter_press(), now);
    let saved = dispatch_app_command(&mut stream, &mut app, action).await;
    let defaults = saved
        .config
        .current
        .as_ref()
        .and_then(|config| config.runtime_defaults.as_ref())
        .expect("runtime defaults should be persisted");
    assert_eq!(defaults.provider_key, "anthropic");
    assert_eq!(defaults.model_id, "sonnet");
    assert!(!defaults.mcp_default_enabled);
    assert_eq!(defaults.enabled_mcp_server_keys, vec!["files"]);
    insta::assert_snapshot!("config_defaults_saved_real_server", render_app(&app));
}

async fn setup_runtime_defaults_server() -> &'static TestServer {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    config_service
        .store()
        .save(Configuration {
            etag: String::new(),
            providers: HashMap::from([(
                "anthropic".to_owned(),
                ProviderView {
                    kind: ProviderKindView::AnthropicMessages,
                    name: "anthropic".to_owned(),
                    url: "https://api.anthropic.com/v1/messages".to_owned(),
                    headers: Default::default(),
                    oauth_compatible: false,
                    oauth_settings: None,
                    models: BTreeMap::from([("sonnet".to_owned(), model("Claude"))]),
                }
                .into(),
            )]),
            mcp_servers: HashMap::from([(
                "files".to_owned(),
                McpServerView {
                    name: "files".to_owned(),
                    kind: McpServerTypeView::Stdio,
                    command: "fs-server".to_owned(),
                    url: String::new(),
                }
                .into(),
            )]),
            runtime_defaults: Some(
                RuntimeDefaultsView {
                    provider_key: "anthropic".to_owned(),
                    model_id: "sonnet".to_owned(),
                    reasoning_effort: None,
                    mcp_default_enabled: false,
                    enabled_mcp_server_keys: Vec::new(),
                }
                .into(),
            ),
        })
        .await
        .expect("runtime defaults seed should save");

    setup_server_with_services(config_service)
}

fn model(name: &str) -> ModelView {
    ModelView {
        name: name.to_owned(),
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
        catalog_match: None,
    }
}
