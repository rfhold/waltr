use std::{
    collections::{BTreeMap, HashMap},
    time::Instant,
};

use waltr_client::{
    ClientOperationView, ClientRuntime, Configuration, McpServerTypeView, McpServerView,
    ModelCapabilitiesView, ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView,
    ProviderKindView, ProviderView, RuntimeDefaultsView,
};
use waltr_server::{
    config::ConfigStore as _,
    config_service::ConfigService,
    test_support::{TestServer, setup_server, setup_server_with_services, temp_root},
};
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{char_press, ctrl_t_press, ctrl_x_press, enter_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_sessions_screen,
    },
};

#[tokio::test]
async fn client_stream_creates_opens_and_deletes_session_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_sessions_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.sessions.watch.status == "received"
    })
    .await;

    let rendered = render_app(&app);
    assert!(rendered.contains("Sessions"));
    assert!(rendered.contains("No sessions yet."));
    insta::assert_snapshot!("sessions_empty_real_server", rendered);

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let create_action = app.handle_event(&char_press('s'), now);
    let created = dispatch_app_command(&mut stream, &mut app, create_action).await;
    let create_key = created
        .sessions
        .create
        .key
        .as_ref()
        .expect("create should expose operation key");
    let session_id = match created.operations.current.get(create_key) {
        Some(ClientOperationView::SessionCreate {
            status: "ready",
            session: Some(session),
            ..
        }) => session.id.clone(),
        operation => panic!("expected ready session create operation, saw {operation:?}"),
    };
    assert_eq!(
        app.sessions_section().detail_session_id(),
        Some(session_id.as_str())
    );

    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot
            .sessions
            .current
            .iter()
            .any(|session| session.id == session_id)
    })
    .await;
    let rendered = render_app(&app);
    assert!(rendered.contains("provider none (C-x p)"));
    assert!(rendered.contains("ctx 0"));
    insta::assert_snapshot!(
        "sessions_detail_real_server",
        normalize_session_render(&rendered, &session_id)
    );

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('d'), now), AppAction::Continue);
    let delete_action = app.handle_event(&enter_press(), now);
    let deleted = dispatch_app_command(&mut stream, &mut app, delete_action).await;
    let delete_key = deleted
        .sessions
        .delete
        .key
        .as_ref()
        .expect("delete should expose operation key");
    assert!(matches!(
        deleted.operations.current.get(delete_key),
        Some(ClientOperationView::SessionDelete {
            status: "ready",
            id,
            ..
        }) if id == &session_id
    ));
    assert_eq!(app.sessions_section().detail_session_id(), None);

    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot
            .sessions
            .current
            .iter()
            .all(|session| session.id != session_id)
    })
    .await;
}

fn normalize_session_render(rendered: &str, session_id: &str) -> String {
    rendered
        .replace(session_id, "[session-id]")
        .lines()
        .map(|line| {
            let Some(index) = line.find("updated ") else {
                return line.to_owned();
            };
            let prefix = &line[..index];
            let rest = &line[index + "updated ".len()..];
            rest.split_once(' ').map_or_else(
                || line.to_owned(),
                |(_timestamp, suffix)| format!("{prefix}updated [timestamp] {suffix}"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn session_detail_updates_runtime_config_with_real_server() {
    let server = setup_runtime_configured_server().await;
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_sessions_screen(&mut stream, &mut app).await;

    let now = Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let create_action = app.handle_event(&char_press('s'), now);
    let created = dispatch_app_command(&mut stream, &mut app, create_action).await;
    let create_key = created
        .sessions
        .create
        .key
        .as_ref()
        .expect("create should expose operation key");
    let session_id = match created.operations.current.get(create_key) {
        Some(ClientOperationView::SessionCreate {
            status: "ready",
            session: Some(session),
            ..
        }) => session.id.clone(),
        operation => panic!("expected ready session create operation, saw {operation:?}"),
    };

    assert_runtime_provider(&created, &session_id, "openai", "gpt4o", None);

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let open_provider = app.handle_event(&char_press('p'), now);
    dispatch_app_command(&mut stream, &mut app, open_provider).await;
    let provider_editor = render_app(&app);
    assert!(provider_editor.contains("Provider/model"));
    assert!(provider_editor.contains("Search"));
    assert!(provider_editor.contains("anthropic/claude"));
    assert!(provider_editor.contains("openai/gpt4o"));
    assert!(!provider_editor.contains("Ctrl-T"));
    assert!(!provider_editor.contains("Thinking"));

    let provider_update = app.handle_event(&enter_press(), now);
    let updated_provider = dispatch_app_command(&mut stream, &mut app, provider_update).await;
    assert_runtime_provider(&updated_provider, &session_id, "anthropic", "claude", None);

    for expected_effort in ["low", "medium", "high"] {
        let thinking_update = app.handle_event(&ctrl_t_press(), now);
        let updated_thinking = dispatch_app_command(&mut stream, &mut app, thinking_update).await;
        assert_runtime_provider(
            &updated_thinking,
            &session_id,
            "anthropic",
            "claude",
            Some(expected_effort),
        );
    }

    let rendered = render_app(&app);
    assert!(rendered.contains("(Ctrl-T)"));
    assert_runtime_provider(&updated_provider, &session_id, "anthropic", "claude", None);

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('m'), now), AppAction::Continue);
    assert!(render_app(&app).contains("MCP servers"));
    assert_eq!(app.handle_event(&char_press(' '), now), AppAction::Continue);
    let mcp_update = app.handle_event(&enter_press(), now);
    let updated_mcp = dispatch_app_command(&mut stream, &mut app, mcp_update).await;
    let runtime = updated_mcp
        .sessions
        .current
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.runtime_config.as_ref())
        .expect("updated session should expose runtime config");
    assert!(
        runtime
            .mcp_servers
            .iter()
            .any(|server| server.key == "files")
    );
    assert!(runtime.mcp_servers.iter().any(|server| server.key == "git"));
    assert_eq!(
        runtime
            .provider
            .as_ref()
            .and_then(|provider| provider.reasoning_effort.as_deref()),
        Some("high")
    );
}

async fn setup_runtime_configured_server() -> &'static TestServer {
    let config_service = ConfigService::new(temp_root()).expect("config service should start");
    config_service
        .store()
        .save(Configuration {
            etag: String::new(),
            providers: HashMap::from([
                ("openai".to_owned(), provider("openai", "gpt4o").into()),
                (
                    "anthropic".to_owned(),
                    provider("anthropic", "claude").into(),
                ),
            ]),
            mcp_servers: HashMap::from([
                ("files".to_owned(), mcp_server("files").into()),
                ("git".to_owned(), mcp_server("git").into()),
            ]),
            runtime_defaults: Some(
                RuntimeDefaultsView {
                    provider_key: "openai".to_owned(),
                    model_id: "gpt4o".to_owned(),
                    reasoning_effort: None,
                    mcp_default_enabled: false,
                    enabled_mcp_server_keys: vec!["git".to_owned()],
                }
                .into(),
            ),
        })
        .await
        .expect("runtime config seed should save");

    setup_server_with_services(config_service)
}

fn assert_runtime_provider(
    snapshot: &waltr_client::ClientSnapshot,
    session_id: &str,
    provider_key: &str,
    model_id: &str,
    reasoning_effort: Option<&str>,
) {
    let provider = snapshot
        .sessions
        .current
        .iter()
        .find(|session| session.id == session_id)
        .and_then(|session| session.runtime_config.as_ref())
        .and_then(|runtime| runtime.provider.as_ref())
        .unwrap_or_else(|| {
            panic!(
                "updated session should expose provider runtime; sessions={:?}; operations={:?}",
                snapshot.sessions.current, snapshot.operations.current
            )
        });
    assert_eq!(provider.key, provider_key);
    assert_eq!(provider.model_id, model_id);
    assert_eq!(provider.reasoning_effort.as_deref(), reasoning_effort);
}

fn provider(name: &str, model_id: &str) -> ProviderView {
    ProviderView {
        kind: ProviderKindView::OpenAiChatCompletions,
        name: name.to_owned(),
        url: "https://example.invalid/v1".to_owned(),
        headers: BTreeMap::new(),
        oauth_compatible: false,
        oauth_settings: None,
        models: BTreeMap::from([(model_id.to_owned(), model(model_id))]),
    }
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
            reasoning_efforts: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
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

fn mcp_server(name: &str) -> McpServerView {
    McpServerView {
        name: name.to_owned(),
        kind: McpServerTypeView::Stdio,
        command: format!("{name}-mcp"),
        url: String::new(),
    }
}
