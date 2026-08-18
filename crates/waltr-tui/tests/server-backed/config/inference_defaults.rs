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
    config_service::ConfigService,
    test_support::{TestServer, default_catalog_loader, setup_server_with_services, temp_root},
};
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{enter_press, tab_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_config_screen, seed_configuration,
    },
};

#[tokio::test]
async fn inference_defaults_snapshot_and_save_use_real_server() {
    let server = setup_inference_defaults_server().await;
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

    let action = app.handle_event(&enter_press(), now);
    let saved = dispatch_app_command(&mut stream, &mut app, action).await;
    let defaults = saved
        .config
        .current
        .as_ref()
        .and_then(|config| config.inference_defaults.as_ref())
        .expect("inference defaults should be persisted");
    assert_eq!(defaults.provider_key, "anthropic");
    assert_eq!(defaults.model_id, "sonnet");
    insta::assert_snapshot!("config_defaults_saved_real_server", render_app(&app));
}

async fn setup_inference_defaults_server() -> &'static TestServer {
    let root = temp_root();
    waltr_server::test_support::persist_test_self_mcp(&root);
    let config_service = ConfigService::new(root, default_catalog_loader())
        .await
        .expect("config service should start");
    let server = setup_server_with_services(config_service);
    seed_configuration(
        server,
        Configuration {
            etag: String::new(),
            providers: HashMap::from([(
                "anthropic".to_owned(),
                ProviderView {
                    kind: ProviderKindView::AnthropicMessages,
                    name: "anthropic".to_owned(),
                    url: "https://api.anthropic.com/v1/messages".to_owned(),
                    headers: BTreeMap::default(),
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
            inference_defaults: Some(
                InferenceDefaultsView {
                    provider_key: "anthropic".to_owned(),
                    model_id: "sonnet".to_owned(),
                    reasoning_effort: None,
                }
                .into(),
            ),
        },
    )
    .await
    .expect("inference defaults resources should save");

    server
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
        cost: None,
        catalog_match: None,
    }
}
