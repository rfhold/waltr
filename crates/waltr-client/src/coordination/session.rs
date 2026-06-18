//! Session reducer responsibilities.

use crate::transport::SessionWatchUpdate;
use crate::{
    ClientEffect, ClientState, ClientUpdate, OperationStatus, SessionEffect, SessionEvent,
    StatusEffect,
};

pub fn reduce(state: &mut ClientState, event: SessionEvent) -> ClientUpdate {
    match event {
        SessionEvent::CreateRequested => {
            state.set_session_create_loading();
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Create))
        }
        SessionEvent::Created(session) => {
            state.set_session_create_ready(session);
            ClientUpdate::default()
        }
        SessionEvent::CreateFailed(error) => {
            state.set_session_create_failed(error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::ListRequested => {
            state.set_session_list_loading();
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List))
        }
        SessionEvent::Listed(sessions) => {
            state.set_session_list_ready(sessions);
            ClientUpdate::default()
        }
        SessionEvent::ListFailed(error) => {
            state.set_session_list_failed(error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::WatchInterestRegistered => {
            state.add_session_watch_interest();
            state.set_session_list_loading();
            let mut update = ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List));
            update.extend(maintain_watch(state));
            update
        }
        SessionEvent::WatchInterestReleased => {
            state.remove_session_watch_interest();
            ClientUpdate::default()
        }
        SessionEvent::WatchStarted => {
            state.set_session_watch_opened();
            ClientUpdate::default()
        }
        SessionEvent::WatchChanged(update) => apply_watch_update(state, update),
        SessionEvent::WatchFailed(error) => watch_failed(state, error.to_string()),
        SessionEvent::WatchEnded => watch_failed(state, "session watch ended".to_owned()),
        SessionEvent::ReconnectReady => maintain_watch(state),
    }
}

pub fn maintain_watch(state: &mut ClientState) -> ClientUpdate {
    if state.sessions.watch.interests == 0 {
        return ClientUpdate::default();
    }

    if matches!(
        state.sessions.watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    ) {
        return ClientUpdate::default();
    }

    if !state.permits_watch_maintenance() {
        state.set_session_watch_waiting();
        return ClientUpdate::default();
    }

    state.set_session_watch_loading();
    ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Watch))
}

fn apply_watch_update(state: &mut ClientState, update: SessionWatchUpdate) -> ClientUpdate {
    if update.sequence != state.sessions.last_sequence.saturating_add(1) {
        state.set_session_watch_changed(update.sequence, update.session);
        return ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List));
    }

    state.set_session_watch_changed(update.sequence, update.session);
    ClientUpdate::default()
}

fn watch_failed(state: &mut ClientState, message: String) -> ClientUpdate {
    if state.sessions.watch.interests == 0 {
        return ClientUpdate::default();
    }

    if state.status.watch.status == OperationStatus::Received {
        let delay_ms = state.set_status_watch_failed(message);
        state.set_session_watch_waiting();
        return ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::SleepReconnect {
            delay_ms,
        }));
    }

    let delay_ms = state.set_session_watch_failed(message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::SleepReconnect {
            delay_ms,
        }))
    } else {
        state.set_session_watch_waiting();
        ClientUpdate::default()
    }
}
