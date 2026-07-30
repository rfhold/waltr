#![allow(
    clippy::future_not_send,
    clippy::unused_async,
    reason = "server-backed fixtures intentionally drive a local non-Send client stream"
)]

use std::{collections::BTreeMap, time::Duration};

use waltr_client::{
    ClientCommand, ClientCommandRequest, ClientOperationView, ClientSnapshot, ClientStateStream,
    ClientStateStreamEvent, ProviderKindView, ProviderView,
};
use waltr_tui::{
    app::{App, AppAction, AppScreen},
    event::AppEvent,
};

use super::input::{char_press, ctrl_x_press};

pub async fn drive_app_to_landing(stream: &mut ClientStateStream, app: &mut App) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);

    while std::time::Instant::now() < deadline {
        let snapshot = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("client stream should produce snapshots");

        assert_eq!(
            app.handle_event(
                &AppEvent::Client(Box::new(snapshot)),
                std::time::Instant::now()
            ),
            AppAction::Continue
        );

        if app.screen() == AppScreen::Landing {
            return;
        }
    }

    panic!("TUI did not leave the splash screen after connecting to the real server");
}

pub async fn open_config_screen(stream: &mut ClientStateStream, app: &mut App) {
    let now = std::time::Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let AppAction::ClientCommands(commands) = app.handle_event(&char_press('c'), now) else {
        panic!("config entry should register config watch interest");
    };
    assert_eq!(
        commands,
        vec![
            ClientCommand::RegisterConfigWatchInterest,
            ClientCommand::RequestAuthStatuses,
        ]
    );

    for command in commands {
        let wait_for_auth_statuses = command == ClientCommand::RequestAuthStatuses;
        let opened = dispatch_client_command(stream, command).await;
        apply_snapshot(stream, app, opened.clone()).await;
        if wait_for_auth_statuses && opened.auth.list.status == "loading" {
            drop(
                next_snapshot_until(stream, app, |snapshot| {
                    matches!(snapshot.auth.list.status, "ready" | "failed")
                })
                .await,
            );
        }
    }
}

pub async fn open_sessions_screen(stream: &mut ClientStateStream, app: &mut App) {
    let now = std::time::Instant::now();
    assert_eq!(app.handle_event(&ctrl_x_press(), now), AppAction::Continue);
    let AppAction::ClientCommand(register) = app.handle_event(&char_press('s'), now) else {
        panic!("sessions entry should register session watch interest");
    };
    assert_eq!(register, ClientCommand::RegisterSessionWatchInterest);

    let opened = dispatch_client_command(stream, register).await;
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(opened)),
            std::time::Instant::now()
        ),
        AppAction::Continue
    );
}

pub async fn dispatch_app_command(
    stream: &mut ClientStateStream,
    app: &mut App,
    action: AppAction,
) -> ClientSnapshot {
    let command = first_client_command(action);

    let snapshot = dispatch_client_command(stream, command).await;
    apply_snapshot(stream, app, snapshot.clone()).await;

    if let Some(key) = snapshot.config.patch.key.clone()
        && snapshot.config.patch.status == "loading"
    {
        return next_snapshot_until(stream, app, |snapshot| {
            matches!(
                snapshot.operations.current.get(&key),
                Some(ClientOperationView::ConfigPatch {
                    status: "ready" | "failed",
                    ..
                })
            )
        })
        .await;
    }

    if let Some(key) = snapshot.config.reset.key.clone()
        && snapshot.config.reset.status == "loading"
    {
        return next_snapshot_until(stream, app, |snapshot| {
            matches!(
                snapshot.operations.current.get(&key),
                Some(ClientOperationView::ConfigReset {
                    status: "ready" | "failed",
                    ..
                })
            )
        })
        .await;
    }

    if snapshot.config.load.status == "loading" {
        return next_snapshot_until(stream, app, |snapshot| snapshot.config.current.is_some())
            .await;
    }

    if let Some(key) = snapshot.sessions.create.key.clone()
        && snapshot.sessions.create.status == "loading"
    {
        return next_snapshot_until(stream, app, |snapshot| {
            matches!(
                snapshot.operations.current.get(&key),
                Some(ClientOperationView::SessionCreate {
                    status: "ready" | "failed",
                    ..
                })
            )
        })
        .await;
    }

    if let Some(key) = snapshot.sessions.delete.key.clone()
        && snapshot.sessions.delete.status == "loading"
    {
        return next_snapshot_until(stream, app, |snapshot| {
            matches!(
                snapshot.operations.current.get(&key),
                Some(ClientOperationView::SessionDelete {
                    status: "ready" | "failed",
                    ..
                })
            )
        })
        .await;
    }

    if let Some(key) = snapshot.sessions.runtime_config_update.key.clone()
        && snapshot.sessions.runtime_config_update.status == "loading"
    {
        return next_snapshot_until(stream, app, |snapshot| {
            matches!(
                snapshot.operations.current.get(&key),
                Some(ClientOperationView::SessionRuntimeConfigUpdate {
                    status: "ready" | "failed",
                    ..
                })
            )
        })
        .await;
    }

    snapshot
}

pub async fn dispatch_client_command(
    stream: &mut ClientStateStream,
    command: ClientCommand,
) -> ClientSnapshot {
    let event = stream
        .next_or_dispatch(async { Some(ClientCommandRequest::new(command)) })
        .await;
    let ClientStateStreamEvent::Snapshot(snapshot) = event else {
        panic!("client stream closed while dispatching command");
    };

    *snapshot
}

pub async fn next_snapshot_until(
    stream: &mut ClientStateStream,
    app: &mut App,
    mut matches: impl FnMut(&ClientSnapshot) -> bool,
) -> ClientSnapshot {
    let mut last_snapshot = None;

    for _ in 0..32 {
        let snapshot = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("client stream should produce snapshots");

        apply_snapshot(stream, app, snapshot.clone()).await;

        if matches(&snapshot) {
            return snapshot;
        }

        last_snapshot = Some(snapshot);
    }

    panic!("TUI stream did not reach expected snapshot; last snapshot: {last_snapshot:?}");
}

async fn apply_snapshot(stream: &mut ClientStateStream, app: &mut App, snapshot: ClientSnapshot) {
    let action = app.handle_event(
        &AppEvent::Client(Box::new(snapshot)),
        std::time::Instant::now(),
    );

    let commands = match action {
        AppAction::ClientCommand(command) => vec![command],
        AppAction::ClientCommands(commands) => commands,
        AppAction::NativeAuth(action) => panic!("unexpected native auth action: {action:?}"),
        AppAction::CopyAuthUrl(url) => panic!("unexpected auth URL copy: {url}"),
        AppAction::Continue => Vec::new(),
        AppAction::Exit => panic!("unexpected exit while applying snapshot"),
    };

    for command in commands {
        let followup = dispatch_client_command(stream, command).await;
        assert_eq!(
            app.handle_event(
                &AppEvent::Client(Box::new(followup)),
                std::time::Instant::now()
            ),
            AppAction::Continue
        );
    }
}

fn first_client_command(action: AppAction) -> ClientCommand {
    match action {
        AppAction::ClientCommand(command) => command,
        AppAction::ClientCommands(mut commands) if !commands.is_empty() => commands.remove(0),
        _ => panic!("expected app to emit client command, saw {action:?}"),
    }
}

pub fn unused_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("unused address should bind")
        .local_addr()
        .expect("unused address should be available")
}

pub fn provider_view(name: &str) -> ProviderView {
    ProviderView {
        kind: ProviderKindView::AnthropicMessages,
        name: name.to_owned(),
        url: String::new(),
        headers: BTreeMap::new(),
        oauth_compatible: false,
        oauth_settings: None,
        models: BTreeMap::new(),
    }
}

pub async fn no_sleep(_: u64) {}
