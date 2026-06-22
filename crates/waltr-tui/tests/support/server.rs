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
    let AppAction::ClientCommand(register) = app.handle_event(&char_press('c'), now) else {
        panic!("config entry should register config watch interest");
    };
    assert_eq!(register, ClientCommand::RegisterConfigWatchInterest);

    let opened = dispatch_client_command(stream, register).await;
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(opened)),
            std::time::Instant::now()
        ),
        AppAction::Continue
    );
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
    let AppAction::ClientCommand(command) = action else {
        panic!("expected app to emit client command, saw {action:?}");
    };

    let snapshot = dispatch_client_command(stream, command).await;
    assert_eq!(
        app.handle_event(
            &AppEvent::Client(Box::new(snapshot.clone())),
            std::time::Instant::now()
        ),
        AppAction::Continue
    );

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

        assert_eq!(
            app.handle_event(
                &AppEvent::Client(Box::new(snapshot.clone())),
                std::time::Instant::now()
            ),
            AppAction::Continue
        );

        if matches(&snapshot) {
            return snapshot;
        }

        last_snapshot = Some(snapshot);
    }

    panic!("TUI stream did not reach expected snapshot; last snapshot: {last_snapshot:?}");
}

pub fn unused_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("unused address should bind")
        .local_addr()
        .expect("unused address should be available")
}

pub fn provider_view(name: &str) -> ProviderView {
    ProviderView {
        kind: ProviderKindView::Anthropic,
        name: name.to_owned(),
        url: String::new(),
        headers: BTreeMap::new(),
        models: BTreeMap::new(),
    }
}

pub async fn no_sleep(_: u64) {}
