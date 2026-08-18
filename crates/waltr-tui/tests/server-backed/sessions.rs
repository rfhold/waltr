use std::{
    collections::{BTreeMap, HashMap},
    time::Instant,
};

use waltr_client::{
    ClientOperationView, ClientRuntime, Configuration, InferenceDefaultsView, McpServerTypeView,
    McpServerView, ModelCapabilitiesView, ModelLimitsView, ModelOptionsView, ModelTypeView,
    ModelView, ProviderKindView, ProviderView,
};
use waltr_server::{
    config_service::ConfigService,
    test_support::{
        TestServer, default_catalog_loader, setup_server, setup_server_with_services, temp_root,
    },
};
use waltr_tui::app::{App, AppAction};

use crate::support::{
    input::{char_press, ctrl_t_press, ctrl_x_press, enter_press, right_press},
    render::render_app,
    server::{
        dispatch_app_command, drive_app_to_landing, next_snapshot_until, no_sleep,
        open_sessions_screen, seed_configuration,
    },
};

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the test covers one complete session lifecycle against a real server"
)]
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
    let load_agents = app.handle_event(&char_press('s'), now);
    if !matches!(load_agents, AppAction::Continue) {
        drop(dispatch_app_command(&mut stream, &mut app, load_agents).await);
        next_snapshot_until(&mut stream, &mut app, |snapshot| {
            (snapshot.sessions.agents.load.status == "ready"
                && !snapshot.sessions.agents.entries.is_empty())
                || snapshot.sessions.agents.load.status == "failed"
        })
        .await;
    }
    let selector = render_app(&app);
    assert!(selector.contains("Create session"));
    let create_action = app.handle_event(&enter_press(), now);
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
    assert!(rendered.contains("agent "));
    assert!(rendered.contains("(C-x a)"));
    assert!(rendered.contains("provider codex/gpt-5.6-sol (C-x p)"));
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

#[tokio::test]
async fn session_data_screen_wipes_all_sessions_with_real_server() {
    let server = setup_server();
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_sessions_screen(&mut stream, &mut app).await;
    next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot.sessions.watch.status == "received"
    })
    .await;

    let now = Instant::now();
    for _ in 0..2 {
        assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
        let load_agents = app.handle_event(&char_press('s'), now);
        if !matches!(load_agents, AppAction::Continue) {
            drop(dispatch_app_command(&mut stream, &mut app, load_agents).await);
        }
        let create = app.handle_event(&enter_press(), now);
        drop(dispatch_app_command(&mut stream, &mut app, create).await);
        let session_id = app
            .sessions_section()
            .detail_session_id()
            .expect("created session detail should be open")
            .to_owned();
        let leave_detail = app.handle_event(&crate::support::input::escape_press(), now);
        assert_eq!(
            leave_detail,
            AppAction::ClientCommand(
                waltr_client::ClientCommand::ReleaseSessionTimelineWatchInterest { session_id }
            )
        );
        drop(dispatch_app_command(&mut stream, &mut app, leave_detail).await);
    }

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(app.handle_event(&char_press('w'), now), AppAction::Continue);
    let wipe = app.handle_event(&enter_press(), now);
    let wiped = dispatch_app_command(&mut stream, &mut app, wipe).await;
    let key = wiped
        .sessions
        .wipe
        .key
        .as_ref()
        .expect("wipe should expose operation key");
    assert!(matches!(
        wiped.operations.current.get(key),
        Some(ClientOperationView::SessionWipe {
            status: "ready",
            error: None,
        })
    ));
    assert!(wiped.sessions.current.is_empty());
    assert_eq!(app.screen(), waltr_tui::app::AppScreen::SessionData);
    let rendered = render_app(&app);
    assert!(rendered.contains("No sessions"));
    assert!(rendered.contains("remain."));
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
async fn session_detail_updates_inference_settings_with_real_server() {
    let server = setup_inference_configured_server().await;
    let mut stream = ClientRuntime::for_endpoint(server.base_url()).into_state_stream(no_sleep);
    let mut app = App::new();

    drive_app_to_landing(&mut stream, &mut app).await;
    open_sessions_screen(&mut stream, &mut app).await;

    let now = Instant::now();
    let create_action = AppAction::ClientCommand(waltr_client::ClientCommand::CreateSession {
        agent_name: "coding".to_owned(),
    });
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

    let created = next_snapshot_until(&mut stream, &mut app, |snapshot| {
        snapshot
            .sessions
            .current
            .iter()
            .any(|session| session.id == session_id)
    })
    .await;

    assert_inference_provider(&created, &session_id, "openai", "gpt4o", None);
    let open_detail = app.handle_event(&right_press(), now);
    drop(dispatch_app_command(&mut stream, &mut app, open_detail).await);

    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let open_provider = app.handle_event(&char_press('p'), now);
    dispatch_app_command(&mut stream, &mut app, open_provider).await;
    let provider_editor = render_app(&app);
    assert!(provider_editor.contains("Inference settings"));
    assert!(provider_editor.contains("Search"));
    assert!(!provider_editor.contains("Ctrl-T"));
    assert!(!provider_editor.contains("Thinking"));

    let provider_update = app.handle_event(&enter_press(), now);
    let updated_provider = dispatch_app_command(&mut stream, &mut app, provider_update).await;
    assert_inference_provider(&updated_provider, &session_id, "openai", "gpt4o", None);

    for expected_effort in ["low", "medium", "high"] {
        let thinking_update = app.handle_event(&ctrl_t_press(), now);
        drop(dispatch_app_command(&mut stream, &mut app, thinking_update).await);
        let updated_thinking = next_snapshot_until(&mut stream, &mut app, |snapshot| {
            snapshot
                .sessions
                .current
                .iter()
                .find(|session| session.id == session_id)
                .and_then(|session| session.inference_settings.as_ref())
                .and_then(|settings| settings.provider.as_ref())
                .and_then(|provider| provider.reasoning_effort.as_deref())
                == Some(expected_effort)
        })
        .await;
        assert_inference_provider(
            &updated_thinking,
            &session_id,
            "openai",
            "gpt4o",
            Some(expected_effort),
        );
    }

    let rendered = render_app(&app);
    assert!(rendered.contains("agent coding (C-x a)"));
    assert_inference_provider(&updated_provider, &session_id, "openai", "gpt4o", None);
}

async fn setup_inference_configured_server() -> &'static TestServer {
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
            inference_defaults: Some(
                InferenceDefaultsView {
                    provider_key: "openai".to_owned(),
                    model_id: "gpt4o".to_owned(),
                    reasoning_effort: None,
                }
                .into(),
            ),
        },
    )
    .await
    .expect("inference settings resources should save");

    server
}

fn assert_inference_provider(
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
        .and_then(|session| session.inference_settings.as_ref())
        .and_then(|settings| settings.provider.as_ref())
        .unwrap_or_else(|| {
            panic!(
                "updated session should expose inference provider; sessions={:?}; operations={:?}",
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
