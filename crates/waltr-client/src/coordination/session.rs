//! Session reducer responsibilities.

use crate::transport::{SessionWatchEvent, SessionWatchUpdate};
use crate::{
    ClientEffect, ClientState, ClientUpdate, OperationStatus, SessionEffect, SessionEvent,
    StatusEffect,
};

pub fn reduce(state: &mut ClientState, event: SessionEvent) -> ClientUpdate {
    match event {
        SessionEvent::CreateRequested(operation_key) => {
            state.set_session_create_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Create {
                operation_key,
            }))
        }
        SessionEvent::Created {
            operation_key,
            session,
        } => {
            state.set_session_create_ready(operation_key, session);
            ClientUpdate::default()
        }
        SessionEvent::CreateFailed {
            operation_key,
            error,
        } => {
            state.set_session_create_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::DeleteRequested { operation_key, id } => {
            state.set_session_delete_loading(operation_key.clone(), id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Delete {
                operation_key,
                id,
            }))
        }
        SessionEvent::Deleted { operation_key, id } => {
            state.set_session_delete_ready(operation_key, id);
            ClientUpdate::default()
        }
        SessionEvent::DeleteFailed {
            operation_key,
            id,
            error,
        } => {
            state.set_session_delete_failed(operation_key, id, error.to_string());
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
        apply_watch_event(state, update);
        return ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List));
    }

    apply_watch_event(state, update);
    ClientUpdate::default()
}

fn apply_watch_event(state: &mut ClientState, update: SessionWatchUpdate) {
    match update.event {
        SessionWatchEvent::Upsert(session) => {
            state.set_session_watch_changed(update.sequence, session)
        }
        SessionWatchEvent::Deleted(id) => state.set_session_watch_deleted(update.sequence, &id),
    }
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
