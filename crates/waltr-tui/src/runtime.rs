//! Async runtime loop for the terminal UI.

use std::{env, io, time::Duration};

use waltr_client::{ClientCommand, ClientCommandRequest, ClientRuntime, ClientStateStreamEvent};
use ratatui::DefaultTerminal;
use tokio::{sync::mpsc, time};

use crate::{
    app::{App, AppAction},
    event::AppEvent,
    input::{self, InputEvent},
};

const EVENT_BUFFER: usize = 256;
const TICK_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_CLIENT_ENDPOINT: &str = "http://127.0.0.1:8080";
const CLIENT_ENDPOINT_ENV: &str = "WALTR_SERVER_ENDPOINT";

/// Run the terminal UI until the app exits.
///
/// # Errors
///
/// Returns an error if terminal drawing, input polling, or Ctrl-C handling fails.
pub async fn run(terminal: &mut DefaultTerminal) -> io::Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (events, commands, runtime) = RuntimeEvents::start();
            let result = run_with_events(terminal, App::new(), events, commands).await;
            runtime.abort();
            result
        })
        .await
}

async fn run_with_events(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut events: mpsc::Receiver<io::Result<AppEvent>>,
    commands: mpsc::Sender<ClientCommand>,
) -> io::Result<()> {
    let _ = terminal.draw(|frame| app.draw(frame))?;

    while let Some(event) = events.recv().await {
        let event = event?;
        let now = event.timestamp(std::time::Instant::now());

        match app.handle_event(&event, now) {
            AppAction::Exit => return Ok(()),
            AppAction::Continue => {}
            AppAction::ClientCommand(command) => {
                if commands.send(command).await.is_err() {
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
    fn start() -> (
        mpsc::Receiver<io::Result<AppEvent>>,
        mpsc::Sender<ClientCommand>,
        Self,
    ) {
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let (commands, command_receiver) = mpsc::channel(EVENT_BUFFER);
        let tasks = vec![
            spawn_tick_events(events.clone()),
            spawn_input_events(events.clone()),
            spawn_ctrl_c_events(events.clone()),
            spawn_client_events(events, command_receiver),
        ];

        (receiver, commands, Self { tasks })
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
            match input::drain_key_events() {
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
    mut commands: mpsc::Receiver<ClientCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(async move {
        let mut client =
            ClientRuntime::for_endpoint(client_endpoint()).into_state_stream(sleep_millis);

        loop {
            let event = match client
                .next_or_dispatch(async { commands.recv().await.map(ClientCommandRequest::new) })
                .await
            {
                ClientStateStreamEvent::Snapshot(snapshot) => Ok(AppEvent::Client(snapshot)),
                ClientStateStreamEvent::CommandsClosed => return,
            };

            if events.send(event).await.is_err() {
                return;
            }
        }
    })
}

async fn sleep_millis(milliseconds: u64) {
    time::sleep(Duration::from_millis(milliseconds)).await;
}

fn client_endpoint() -> String {
    env::var(CLIENT_ENDPOINT_ENV).unwrap_or_else(|_| DEFAULT_CLIENT_ENDPOINT.to_owned())
}
