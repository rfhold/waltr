//! Status reducer responsibilities.

use super::{config, session};
use crate::{ClientEffect, ClientState, ClientUpdate, OperationStatus, StatusEffect, StatusEvent};

pub fn reduce(state: &mut ClientState, event: StatusEvent) -> ClientUpdate {
    match event {
        StatusEvent::WatchRequested => {
            let epoch = state.start_status_watch();
            ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch { epoch }))
        }
        StatusEvent::WatchStarted if state.status.watch_epoch == 0 => {
            state.set_status_watch_started();
            ClientUpdate::default()
        }
        StatusEvent::WatchStartedForEpoch { epoch } if state.status.watch_epoch == epoch => {
            state.set_status_watch_started();
            ClientUpdate::default()
        }
        StatusEvent::WatchChanged(status) if state.status.watch_epoch == 0 => {
            state.authenticate_waltr_access(0);
            apply_status_changed(state, status)
        }
        StatusEvent::WatchChangedForEpoch { epoch, status }
            if state.authenticate_waltr_access(epoch)
                || (state.status.watch_epoch == epoch
                    && state.access.status == crate::AccessStatus::Authenticated) =>
        {
            apply_status_changed(state, status)
        }
        StatusEvent::WatchFailed(error) if state.status.watch_epoch == 0 => {
            watch_failed(state, 0, error.to_string())
        }
        StatusEvent::WatchFailedForEpoch { epoch, error } if state.status.watch_epoch == epoch => {
            watch_failed(state, epoch, error.to_string())
        }
        StatusEvent::WatchEnded if state.status.watch_epoch == 0 => {
            watch_failed(state, 0, "status watch ended".to_owned())
        }
        StatusEvent::WatchEndedForEpoch { epoch } if state.status.watch_epoch == epoch => {
            watch_failed(state, epoch, "status watch ended".to_owned())
        }
        StatusEvent::ReconnectReady if state.status.watch_epoch == 0 => {
            if state.permits_protected_requests() {
                ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch { epoch: 0 }))
            } else {
                ClientUpdate::default()
            }
        }
        StatusEvent::ReconnectReadyForEpoch { epoch } if state.status.watch_epoch == epoch => {
            if state.permits_protected_requests() {
                ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch { epoch }))
            } else {
                ClientUpdate::default()
            }
        }
        _ => ClientUpdate::default(),
    }
}

fn apply_status_changed(state: &mut ClientState, status: crate::SystemStatus) -> ClientUpdate {
    state.set_status_watch_changed(status);
    let mut update = config::maintain_watch(state);
    update.extend(session::maintain_watch(state));
    if state.access.reload_config {
        state.access.reload_config = false;
        update.extend(ClientUpdate::with_effect(ClientEffect::Config(
            crate::ConfigEffect::Load,
        )));
    }
    if state.access.reload_sessions {
        state.access.reload_sessions = false;
        update.extend(ClientUpdate::with_effect(ClientEffect::Session(
            crate::SessionEffect::List,
        )));
    }
    update
}

#[must_use]
pub const fn needs_watch_request(state: &ClientState) -> bool {
    state.permits_protected_requests()
        && !matches!(
            state.status.watch.status,
            OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
        )
}

fn watch_failed(state: &mut ClientState, epoch: u64, message: String) -> ClientUpdate {
    let delay_ms = state.set_status_watch_failed(message);
    if state.config.watch.interests > 0 {
        state.set_config_watch_waiting();
    }
    if state.sessions.watch.interests > 0 {
        state.set_session_watch_waiting();
    }
    ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::SleepReconnect {
        epoch,
        delay_ms,
    }))
}
