use std::{collections::BTreeMap, time::Instant};

use waltr_client::{
    CatalogMatchSourceView, ClientCommand, ClientOperationView, ClientSnapshot, ConfigSnapshot,
    ConfigView, ExternalRequestView, McpServerTypeView, McpServerView, ModelCapabilitiesView,
    ModelCatalogMatchView, ModelLimitsView, ModelOptionsView, ModelTypeView, ModelView,
    OperationCollectionSnapshot, OperationSnapshot, ProviderKindView, ProviderView,
    SessionPhaseView, SessionSnapshot, SessionStatusView, SessionTimelineView, SessionView,
    StatusSnapshot, TimelineContentView, TimelineEventKindView, TimelineEventView,
    TimelineStatusView, TimestampView,
};
use waltr_tui::{
    app::{App, AppAction},
    event::AppEvent,
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;

#[allow(unused_imports)]
pub(crate) use super::input::{
    back_tab_press, backspace_press, char_press, ctrl_t_press, ctrl_x_press, delete_press,
    down_press, end_press, enter_press, escape_press, home_press, left_press, page_down_press,
    page_up_press, right_press, scroll_down, scroll_up, shift_enter_press, tab_press,
};

pub(crate) fn render_app(app: &App) -> String {
    render_app_with_size(app, 80, 24)
}

pub(crate) fn render_app_with_size(app: &App, width: u16, height: u16) -> String {
    let mut terminal = test_terminal_with_size(width, height);

    terminal
        .draw(|frame| app.draw(frame))
        .expect("app should render");

    terminal.backend().to_string()
}

pub(crate) fn test_terminal() -> Terminal<TestBackend> {
    test_terminal_with_size(80, 24)
}

pub(crate) fn test_terminal_with_size(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).expect("test backend should initialize")
}

pub(crate) fn open_config_app() -> App {
    open_config_app_with(watched_config_snapshot())
}

pub(crate) fn open_config_app_with_mcp() -> App {
    open_config_app_with(patched_mcp_snapshot())
}

pub(crate) fn patched_mcp_snapshot() -> ClientSnapshot {
    let mut snapshot = watched_config_snapshot();
    if let Some(config) = snapshot.config.current.as_mut() {
        config.mcp_servers.insert(
            "files".to_owned(),
            McpServerView {
                name: "files".to_owned(),
                kind: McpServerTypeView::Stdio,
                command: "fs-server".to_owned(),
                url: String::new(),
            },
        );
    }
    snapshot.config.patch.status = "ready";
    snapshot
}

pub(crate) fn open_config_app_with_model() -> App {
    let mut snapshot = watched_config_snapshot();
    if let Some(config) = snapshot.config.current.as_mut()
        && let Some(provider) = config.providers.get_mut("anthropic")
    {
        provider.models.insert(
            "sonnet".to_owned(),
            ModelView {
                name: "Claude".to_owned(),
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
                    tools: false,
                    image: false,
                },
                cost: None,
                catalog_match: Some(ModelCatalogMatchView {
                    provider_id: "anthropic".to_owned(),
                    model_id: "claude-sonnet-4-5".to_owned(),
                    upstream_provider_id: None,
                    upstream_model_id: None,
                    source: CatalogMatchSourceView::Manual,
                }),
            },
        );
    }
    open_config_app_with(snapshot)
}

pub(crate) fn open_config_app_with(snapshot: ClientSnapshot) -> App {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('c'), now),
        AppAction::ClientCommand(ClientCommand::RegisterConfigWatchInterest)
    );

    app
}

pub(crate) fn open_sessions_app_with(snapshot: ClientSnapshot) -> App {
    let mut app = App::new();
    let now = Instant::now();

    assert_eq!(
        app.handle_event(&AppEvent::Client(Box::new(snapshot)), now),
        AppAction::Continue
    );
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    assert_eq!(
        app.handle_event(&char_press('s'), now),
        AppAction::ClientCommand(ClientCommand::RegisterSessionWatchInterest)
    );

    app
}

pub(crate) fn config_patch_snapshot(
    key: &str,
    status: &'static str,
    error: Option<&str>,
) -> ClientSnapshot {
    config_patch_snapshot_from(watched_config_snapshot(), key, status, error)
}

pub(crate) fn config_patch_snapshot_from(
    mut snapshot: ClientSnapshot,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) -> ClientSnapshot {
    snapshot.config.patch = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::ConfigPatch {
            status,
            error: error.map(str::to_owned),
            config: (status == "ready")
                .then(|| snapshot.config.current.clone())
                .flatten(),
        },
    );
    snapshot
}

pub(crate) fn watched_sessions_snapshot(sessions: Vec<SessionView>) -> ClientSnapshot {
    let mut snapshot = ready_snapshot();
    snapshot.sessions.current = sessions;
    snapshot.sessions.list.status = "ready";
    snapshot.sessions.watch.status = "received";
    snapshot
}

pub(crate) fn watched_session_with_timeline(
    session: SessionView,
    events: Vec<TimelineEventView>,
) -> ClientSnapshot {
    let session_id = session.id.clone();
    let mut snapshot = watched_sessions_snapshot(vec![session]);
    snapshot.sessions.timelines.insert(
        session_id,
        SessionTimelineView {
            events,
            load: idle_operation(),
            watch: OperationSnapshot {
                status: "received",
                error: None,
                key: None,
            },
        },
    );
    snapshot
}

pub(crate) fn expected_register_timeline_command(session_id: &str) -> AppAction {
    AppAction::ClientCommand(ClientCommand::RegisterSessionTimelineWatchInterest {
        session_id: session_id.to_owned(),
        after_cursor: None,
        channels: timeline_channels(),
        initial_limit: Some(64),
    })
}

pub(crate) fn timeline_channels() -> Vec<String> {
    [
        "user_text",
        "assistant_text",
        "thinking",
        "tool_call",
        "tool_progress_info",
        "tool_progress_error",
        "tool_result",
        "external_request",
        "metrics",
    ]
    .iter()
    .map(|channel| (*channel).to_owned())
    .collect()
}

pub(crate) fn timeline_text_event(
    cursor: u64,
    session_id: &str,
    channel: &str,
    text: &str,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: 1,
        item_id: format!("item-{cursor}"),
        parent_id: None,
        kind: TimelineEventKindView::ItemFinished,
        channel: Some(channel.to_owned()),
        content: TimelineContentView::TextSnapshot {
            text: text.to_owned(),
        },
        status: None,
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) fn timeline_metrics_event(
    cursor: u64,
    session_id: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: cursor,
        item_id: format!("metrics-{cursor}"),
        parent_id: None,
        kind: TimelineEventKindView::ItemFinished,
        channel: Some("metrics".to_owned()),
        content: TimelineContentView::Json {
            value_json: json!({
                "kind": "turn_metrics",
                "usage": {
                    "inputTokens": input_tokens,
                    "outputTokens": output_tokens,
                    "totalInputTokens": input_tokens,
                    "contextTokens": input_tokens + output_tokens,
                },
            })
            .to_string(),
        },
        status: None,
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) fn timeline_cached_metrics_event(
    cursor: u64,
    session_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> TimelineEventView {
    let total_input_tokens = input_tokens + cache_read_tokens + cache_write_tokens;
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: cursor,
        item_id: format!("metrics-{cursor}"),
        parent_id: None,
        kind: TimelineEventKindView::ItemFinished,
        channel: Some("metrics".to_owned()),
        content: TimelineContentView::Json {
            value_json: json!({
                "kind": "turn_metrics",
                "usage": {
                    "inputTokens": input_tokens,
                    "outputTokens": output_tokens,
                    "cacheReadTokens": cache_read_tokens,
                    "cacheWriteTokens": cache_write_tokens,
                    "totalInputTokens": total_input_tokens,
                    "contextTokens": total_input_tokens + output_tokens,
                },
            })
            .to_string(),
        },
        status: None,
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) fn timeline_tool_call_event(
    cursor: u64,
    session_id: &str,
    item_id: &str,
    call_id: &str,
    name: &str,
    arguments_json: &str,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: 1,
        item_id: item_id.to_owned(),
        parent_id: None,
        kind: TimelineEventKindView::ItemStarted,
        channel: Some("tool_call".to_owned()),
        content: TimelineContentView::ToolCall {
            id: call_id.to_owned(),
            name: name.to_owned(),
            arguments_json: arguments_json.to_owned(),
        },
        status: None,
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) fn timeline_tool_progress_event(
    cursor: u64,
    session_id: &str,
    item_id: &str,
    parent_id: &str,
    channel: &str,
    text: &str,
    snapshot: bool,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: 1,
        item_id: item_id.to_owned(),
        parent_id: Some(parent_id.to_owned()),
        kind: if snapshot {
            TimelineEventKindView::ItemUpdated
        } else {
            TimelineEventKindView::ContentDelta
        },
        channel: Some(channel.to_owned()),
        content: if snapshot {
            TimelineContentView::TextSnapshot {
                text: text.to_owned(),
            }
        } else {
            TimelineContentView::TextDelta {
                delta: text.to_owned(),
            }
        },
        status: None,
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) fn timeline_tool_result_event(
    cursor: u64,
    session_id: &str,
    item_id: &str,
    parent_id: Option<&str>,
    call_id: &str,
    name: &str,
    content: &str,
    error: bool,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: 1,
        item_id: item_id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        kind: TimelineEventKindView::ItemFinished,
        channel: Some("tool_result".to_owned()),
        content: TimelineContentView::ToolResult {
            id: call_id.to_owned(),
            name: name.to_owned(),
            content: content.to_owned(),
            error,
        },
        status: None,
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) fn external_request_event(
    cursor: u64,
    session_id: &str,
    item_id: &str,
    request_id: &str,
    kind: i32,
    payload_json: &str,
) -> TimelineEventView {
    TimelineEventView {
        cursor,
        session_id: session_id.to_owned(),
        turn: 1,
        item_id: item_id.to_owned(),
        parent_id: None,
        kind: TimelineEventKindView::ExternalRequestOpened,
        channel: Some("external_request".to_owned()),
        content: TimelineContentView::ExternalRequest {
            request: Some(ExternalRequestView {
                id: request_id.to_owned(),
                kind,
                payload_json: payload_json.to_owned(),
            }),
        },
        status: Some(TimelineStatusView::AwaitingExternalInput),
        metadata_json: "{}".to_owned(),
    }
}

pub(crate) const fn idle_operation() -> OperationSnapshot {
    OperationSnapshot {
        status: "idle",
        error: None,
        key: None,
    }
}

pub(crate) fn session_create_snapshot(
    key: &str,
    status: &'static str,
    session: Option<SessionView>,
) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(session.iter().cloned().collect());
    snapshot.sessions.create = OperationSnapshot {
        status,
        error: None,
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionCreate {
            status,
            error: None,
            session,
        },
    );
    snapshot
}

pub(crate) fn session_create_snapshot_with_error(
    key: &str,
    status: &'static str,
    error: &str,
) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(Vec::new());
    snapshot.sessions.create = OperationSnapshot {
        status,
        error: Some(error.to_owned()),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionCreate {
            status,
            error: Some(error.to_owned()),
            session: None,
        },
    );
    snapshot
}

pub(crate) fn session_delete_snapshot(key: &str, status: &'static str, id: &str) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(Vec::new());
    snapshot.sessions.delete = OperationSnapshot {
        status,
        error: None,
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionDelete {
            status,
            error: None,
            id: id.to_owned(),
        },
    );
    snapshot
}

pub(crate) fn session_delete_snapshot_with_error(
    key: &str,
    status: &'static str,
    id: &str,
    error: &str,
) -> ClientSnapshot {
    let mut snapshot = watched_sessions_snapshot(vec![session_view(id, 1)]);
    snapshot.sessions.delete = OperationSnapshot {
        status,
        error: Some(error.to_owned()),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionDelete {
            status,
            error: Some(error.to_owned()),
            id: id.to_owned(),
        },
    );
    snapshot
}

pub(crate) fn session_submit_snapshot(
    session: SessionView,
    key: &str,
    status: &'static str,
    error: Option<&str>,
) -> ClientSnapshot {
    let session_id = session.id.clone();
    let mut snapshot = watched_session_with_timeline(session, Vec::new());
    snapshot.sessions.submit_user_message = OperationSnapshot {
        status,
        error: error.map(str::to_owned),
        key: Some(key.to_owned()),
    };
    snapshot.operations.current.insert(
        key.to_owned(),
        ClientOperationView::SessionSubmitUserMessage {
            status,
            error: error.map(str::to_owned),
            session_id,
        },
    );
    snapshot
}

pub(crate) fn session_view(id: &str, seconds: i64) -> SessionView {
    session_with_phase(id, seconds, SessionPhaseView::Idle, "")
}

pub(crate) fn session_with_phase(
    id: &str,
    seconds: i64,
    phase: SessionPhaseView,
    failed_message: &str,
) -> SessionView {
    SessionView {
        id: id.to_owned(),
        revision: 1,
        status: SessionStatusView {
            phase,
            failed_message: failed_message.to_owned(),
        },
        current_prompt: None,
        created_at: Some(TimestampView { seconds, nanos: 0 }),
        updated_at: Some(TimestampView { seconds, nanos: 0 }),
        last_activity_at: None,
        runtime_config: None,
        metrics: Default::default(),
    }
}

pub(crate) const fn ready_snapshot() -> ClientSnapshot {
    ClientSnapshot {
        operations: OperationCollectionSnapshot {
            current: BTreeMap::new(),
        },
        config: ConfigSnapshot {
            current: None,
            load: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            patch: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            watch: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
        },
        status: StatusSnapshot {
            ready: true,
            watch: OperationSnapshot {
                status: "ready",
                error: None,
                key: None,
            },
        },
        sessions: SessionSnapshot {
            current: Vec::new(),
            create: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            list: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            delete: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            runtime_config_update: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            submit_user_message: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            respond_external_request: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            interrupt: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            watch: OperationSnapshot {
                status: "idle",
                error: None,
                key: None,
            },
            timelines: BTreeMap::new(),
        },
    }
}

pub(crate) fn loaded_config_snapshot() -> ClientSnapshot {
    let mut snapshot = ready_snapshot();
    snapshot.config.current = Some(ConfigView {
        etag: "etag-1".to_owned(),
        providers: BTreeMap::from([(
            "anthropic".to_owned(),
            ProviderView {
                kind: ProviderKindView::AnthropicMessages,
                name: "anthropic".to_owned(),
                url: "https://api.anthropic.com/v1/messages".to_owned(),
                headers: BTreeMap::new(),
                oauth_compatible: false,
                oauth_settings: None,
                models: BTreeMap::new(),
            },
        )]),
        mcp_servers: BTreeMap::new(),
        runtime_defaults: None,
    });
    snapshot.config.load.status = "ready";
    snapshot
}

pub(crate) fn watched_config_snapshot() -> ClientSnapshot {
    let mut snapshot = loaded_config_snapshot();
    snapshot.config.watch.status = "received";
    snapshot.config.load.status = "idle";
    snapshot
}
