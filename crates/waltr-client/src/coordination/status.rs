//! Status reducer responsibilities.

use super::{config, session};
use crate::{ClientEffect, ClientState, ClientUpdate, OperationStatus, StatusEffect, StatusEvent};

pub fn reduce(state: &mut ClientState, event: StatusEvent) -> ClientUpdate {
    match event {
        StatusEvent::WatchRequested => {
            state.set_status_watch_loading();
            ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch))
        }
        StatusEvent::WatchStarted => {
            state.set_status_watch_started();
            ClientUpdate::default()
        }
        StatusEvent::WatchChanged(status) => {
            state.set_status_watch_changed(status);
            let mut update = config::maintain_watch(state);
            update.extend(session::maintain_watch(state));
            update
        }
        StatusEvent::WatchFailed(error) => watch_failed(state, error.to_string()),
        StatusEvent::WatchEnded => watch_failed(state, "status watch ended".to_owned()),
        StatusEvent::ReconnectReady => {
            ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch))
        }
    }
}

#[must_use]
pub const fn needs_watch_request(state: &ClientState) -> bool {
    !matches!(
        state.status.watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    )
}

fn watch_failed(state: &mut ClientState, message: String) -> ClientUpdate {
    let delay_ms = state.set_status_watch_failed(message);
    if state.config.watch.interests > 0 {
        state.set_config_watch_waiting();
    }
    if state.sessions.watch.interests > 0 {
        state.set_session_watch_waiting();
    }
    ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::SleepReconnect {
        delay_ms,
    }))
}
