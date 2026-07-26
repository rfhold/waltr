//! Async runtime loop for the terminal UI.

use std::{cell::Cell, env, io, rc::Rc, time::Duration};

use waltr_client::{
    AccessStatus, ClientCommand, ClientCommandRequest, ClientRuntime, ClientStateStreamEvent,
    NativeCredentialProvider, NativeGrpcTransport,
};
use ratatui::DefaultTerminal;
use tokio::{sync::mpsc, time};

use crate::{
    app::{App, AppAction},
    event::{AppEvent, NativeLoginEvent},
    input::{self, InputEvent},
    native_auth::{self, NativeAuthAction},
};

const EVENT_BUFFER: usize = 256;
const TICK_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_CLIENT_ENDPOINT: &str = "http://127.0.0.1:14333";
const CLIENT_ENDPOINT_ENV: &str = "WALTR_SERVER_ENDPOINT";

type RuntimeChannels = (
    mpsc::Receiver<io::Result<AppEvent>>,
    mpsc::Sender<RuntimeClientCommand>,
    mpsc::Sender<NativeAuthAction>,
    RuntimeEvents,
);

#[derive(Debug, PartialEq)]
pub(crate) struct RuntimeClientCommand {
    command: ClientCommand,
    logout_barrier: bool,
}

impl RuntimeClientCommand {
    pub(crate) const fn dispatch(command: ClientCommand) -> Self {
        Self {
            command,
            logout_barrier: false,
        }
    }

    pub(crate) const fn logout() -> Self {
        Self {
            command: ClientCommand::LogoutWaltr,
            logout_barrier: true,
        }
    }
}

/// Runtime launch options for the terminal UI.
#[derive(Debug, Default)]
pub struct RuntimeOptions {
    /// Explicit client endpoint for this process.
    pub client_endpoint: Option<String>,
}

/// Run the terminal UI until the app exits.
///
/// # Errors
///
/// Returns an error if terminal drawing, input polling, or Ctrl-C handling fails.
pub async fn run(terminal: &mut DefaultTerminal) -> io::Result<()> {
    Box::pin(run_with_options(terminal, RuntimeOptions::default())).await
}

/// Run the terminal UI with explicit launch options until the app exits.
///
/// # Errors
///
/// Returns an error if terminal drawing, input polling, or Ctrl-C handling fails.
pub async fn run_with_options(
    terminal: &mut DefaultTerminal,
    options: RuntimeOptions,
) -> io::Result<()> {
    Box::pin(tokio::task::LocalSet::new().run_until(async {
        let (events, commands, native_auth, runtime) =
            RuntimeEvents::start(options.client_endpoint);
        let result = run_with_events(terminal, App::new(), events, commands, native_auth).await;
        runtime.abort();
        result
    }))
    .await
}

async fn run_with_events(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut events: mpsc::Receiver<io::Result<AppEvent>>,
    commands: mpsc::Sender<RuntimeClientCommand>,
    native_auth: mpsc::Sender<NativeAuthAction>,
) -> io::Result<()> {
    let _ = terminal.draw(|frame| app.draw(frame))?;

    while let Some(event) = events.recv().await {
        let event = event?;
        let now = event.timestamp(std::time::Instant::now());

        match app.handle_event(&event, now) {
            AppAction::Exit => return Ok(()),
            AppAction::Continue => {}
            AppAction::ClientCommand(command) => {
                if commands
                    .send(RuntimeClientCommand::dispatch(command))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            AppAction::ClientCommands(client_commands) => {
                for command in client_commands {
                    if commands
                        .send(RuntimeClientCommand::dispatch(command))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            AppAction::NativeAuth(action) => {
                if native_auth.send(action).await.is_err() {
                    return Ok(());
                }
            }
            AppAction::CopyAuthUrl(url) => {
                if native_auth
                    .send(NativeAuthAction::CopyUrl(url))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }

        let _ = terminal.draw(|frame| app.draw(frame))?;
    }

    Ok(())
}

#[derive(Debug)]
struct RuntimeEvents {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl RuntimeEvents {
    fn start(client_endpoint: Option<String>) -> RuntimeChannels {
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let (commands, command_receiver) = mpsc::channel(EVENT_BUFFER);
        let (native_actions, native_action_receiver) = mpsc::channel(8);
        let endpoint = client_endpoint_or_default(client_endpoint);
        let credentials = NativeCredentialProvider::new();
        let tasks = vec![
            spawn_tick_events(events.clone()),
            spawn_input_events(events.clone()),
            spawn_ctrl_c_events(events.clone()),
            spawn_client_events(
                events.clone(),
                command_receiver,
                endpoint.clone(),
                credentials.clone(),
                native_actions.clone(),
            ),
            native_auth::spawn(
                events,
                commands.clone(),
                native_action_receiver,
                endpoint,
                credentials,
            ),
        ];

        (receiver, commands, native_actions, Self { tasks })
    }

    fn abort(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

fn spawn_tick_events(events: mpsc::Sender<io::Result<AppEvent>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = time::interval(TICK_INTERVAL);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            if events
                .send(Ok(AppEvent::Tick(std::time::Instant::now())))
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

fn spawn_input_events(events: mpsc::Sender<io::Result<AppEvent>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = time::interval(TICK_INTERVAL);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            match input::drain_input_events() {
                Ok(inputs) => {
                    for input in inputs {
                        if events.send(Ok(AppEvent::Input(input))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    drop(events.send(Err(error)).await);
                    return;
                }
            }
        }
    })
}

fn spawn_ctrl_c_events(events: mpsc::Sender<io::Result<AppEvent>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = tokio::signal::ctrl_c().await {
                drop(events.send(Err(error)).await);
                return;
            }

            if events
                .send(Ok(AppEvent::Input(InputEvent::CtrlC)))
                .await
                .is_err()
            {
                return;
            }
        }
    })
}

fn spawn_client_events(
    events: mpsc::Sender<io::Result<AppEvent>>,
    mut commands: mpsc::Receiver<RuntimeClientCommand>,
    client_endpoint: String,
    credentials: NativeCredentialProvider,
    native_auth: mpsc::Sender<NativeAuthAction>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let transport = NativeGrpcTransport::with_credential_provider(client_endpoint, credentials);
        let mut client = ClientRuntime::with_transport(transport).into_state_stream(sleep_millis);

        loop {
            let logout_barrier = Rc::new(Cell::new(false));
            let command_barrier = logout_barrier.clone();
            match client
                .next_or_dispatch(async {
                    commands.recv().await.map(|queued| {
                        command_barrier.set(queued.logout_barrier);
                        ClientCommandRequest::new(queued.command)
                    })
                })
                .await
            {
                ClientStateStreamEvent::Snapshot(snapshot) => {
                    if snapshot.access.status == AccessStatus::LoginInProgress
                        && let Some(attempt_id) = snapshot.access.active_attempt_id.clone()
                        && native_auth
                            .send(NativeAuthAction::AttemptStarted { attempt_id })
                            .await
                            .is_err()
                    {
                        return;
                    }
                    if !publish_client_snapshot(&events, snapshot, logout_barrier.get()).await {
                        return;
                    }
                }
                ClientStateStreamEvent::CommandsClosed => return,
            }
        }
    })
}

async fn publish_client_snapshot(
    events: &mpsc::Sender<io::Result<AppEvent>>,
    snapshot: Box<waltr_client::ClientSnapshot>,
    logout_barrier: bool,
) -> bool {
    if logout_barrier
        && events
            .send(Ok(AppEvent::NativeLogin(NativeLoginEvent::LoggedOut)))
            .await
            .is_err()
    {
        return false;
    }
    events.send(Ok(AppEvent::Client(snapshot))).await.is_ok()
}

async fn sleep_millis(milliseconds: u64) {
    time::sleep(Duration::from_millis(milliseconds)).await;
}

fn client_endpoint_or_default(client_endpoint: Option<String>) -> String {
    client_endpoint
        .or_else(|| env::var(CLIENT_ENDPOINT_ENV).ok())
        .unwrap_or_else(|| DEFAULT_CLIENT_ENDPOINT.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logout_barrier_is_published_before_its_reducer_snapshot() {
        let (events, mut receiver) = mpsc::channel(3);
        let stale = Box::new(ClientRuntime::for_endpoint("http://127.0.0.1:0").snapshot());
        events.send(Ok(AppEvent::Client(stale))).await.unwrap();
        let checking = Box::new(ClientRuntime::for_endpoint("http://127.0.0.1:0").snapshot());

        assert!(publish_client_snapshot(&events, checking, true).await);
        assert!(matches!(
            receiver.recv().await,
            Some(Ok(AppEvent::Client(_)))
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(Ok(AppEvent::NativeLogin(NativeLoginEvent::LoggedOut)))
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(Ok(AppEvent::Client(_)))
        ));
    }
}
