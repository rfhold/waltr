//! Session reducer responsibilities.

use crate::transport::{SessionWatchEvent, SessionWatchUpdate};
use crate::{
    ClientEffect, ClientState, ClientUpdate, OperationStatus, SessionEffect, SessionEvent,
};

#[allow(clippy::too_many_lines)]
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
        SessionEvent::RuntimeConfigUpdateRequested {
            operation_key,
            id,
            runtime_config,
        } => {
            state.set_session_runtime_config_update_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::UpdateRuntimeConfig {
                operation_key,
                id,
                runtime_config,
            }))
        }
        SessionEvent::RuntimeConfigUpdated {
            operation_key,
            session,
        } => {
            state.set_session_runtime_config_update_ready(operation_key, session);
            ClientUpdate::default()
        }
        SessionEvent::RuntimeConfigUpdateFailed {
            operation_key,
            error,
        } => {
            state.set_session_runtime_config_update_failed(operation_key, error.to_string());
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
        SessionEvent::TimelineRequested { session_id, query } => {
            state.set_session_timeline_load_loading(&session_id);
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::GetTimeline {
                session_id,
                query,
            }))
        }
        SessionEvent::TimelineLoaded { session_id, events } => {
            state.set_session_timeline_load_ready(&session_id, events);
            ClientUpdate::default()
        }
        SessionEvent::TimelineLoadFailed { session_id, error } => {
            state.set_session_timeline_load_failed(&session_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::WatchInterestRegistered => {
            state.add_session_watch_interest();
            maintain_watch(state)
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
        SessionEvent::TimelineWatchInterestRegistered {
            session_id,
            after_cursor,
            channels,
            initial_limit,
        } => {
            state.configure_session_timeline_watch(
                &session_id,
                after_cursor,
                channels,
                initial_limit,
            );
            maintain_timeline_watch(state, &session_id)
        }
        SessionEvent::TimelineWatchInterestReleased { session_id } => {
            state.remove_session_timeline_watch_interest(&session_id);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchStarted { session_id } => {
            state.set_session_timeline_watch_opened(&session_id);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchChanged(update) => {
            state.set_session_timeline_watch_changed(update.sequence, update.event);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchFailed { session_id, error } => {
            timeline_watch_failed(state, &session_id, error.to_string())
        }
        SessionEvent::TimelineWatchEnded { session_id } => {
            timeline_watch_failed(state, &session_id, "timeline watch ended".to_owned())
        }
        SessionEvent::TimelineReconnectReady { session_id } => {
            maintain_timeline_watch(state, &session_id)
        }
        SessionEvent::UserMessageSubmitRequested {
            operation_key,
            session_id,
            text,
        } => {
            state
                .set_session_submit_user_message_loading(operation_key.clone(), session_id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::SubmitUserMessage {
                operation_key,
                session_id,
                text,
            }))
        }
        SessionEvent::UserMessageSubmitted { operation_key } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_submit_user_message_ready(operation_key, session_id);
            ClientUpdate::default()
        }
        SessionEvent::UserMessageSubmitFailed {
            operation_key,
            error,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_submit_user_message_failed(
                operation_key,
                session_id,
                error.to_string(),
            );
            ClientUpdate::default()
        }
        SessionEvent::ExternalRequestRespondRequested {
            operation_key,
            session_id,
            request_id,
            response_json,
        } => {
            state.set_session_respond_external_request_loading(
                operation_key.clone(),
                session_id.clone(),
                request_id.clone(),
            );
            ClientUpdate::with_effect(ClientEffect::Session(
                SessionEffect::RespondExternalRequest {
                    operation_key,
                    session_id,
                    request_id,
                    response_json,
                },
            ))
        }
        SessionEvent::ExternalRequestResponded { operation_key } => {
            let (session_id, request_id) = operation_session_and_request_id(state, &operation_key);
            state.set_session_respond_external_request_ready(operation_key, session_id, request_id);
            ClientUpdate::default()
        }
        SessionEvent::ExternalRequestRespondFailed {
            operation_key,
            error,
        } => {
            let (session_id, request_id) = operation_session_and_request_id(state, &operation_key);
            state.set_session_respond_external_request_failed(
                operation_key,
                session_id,
                request_id,
                error.to_string(),
            );
            ClientUpdate::default()
        }
        SessionEvent::InterruptRequested {
            operation_key,
            session_id,
        } => {
            state.set_session_interrupt_loading(operation_key.clone(), session_id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Interrupt {
                operation_key,
                session_id,
            }))
        }
        SessionEvent::Interrupted { operation_key } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_interrupt_ready(operation_key, session_id);
            ClientUpdate::default()
        }
        SessionEvent::InterruptFailed {
            operation_key,
            error,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_interrupt_failed(operation_key, session_id, error.to_string());
            ClientUpdate::default()
        }
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

pub fn maintain_timeline_watch(state: &mut ClientState, session_id: &str) -> ClientUpdate {
    let Some(timeline) = state.sessions.timelines.get(session_id) else {
        return ClientUpdate::default();
    };

    if timeline.watch.interests == 0 {
        return ClientUpdate::default();
    }

    if matches!(
        timeline.watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    ) {
        return ClientUpdate::default();
    }

    if !state.permits_watch_maintenance() {
        state.set_session_timeline_watch_waiting(session_id);
        return ClientUpdate::default();
    }

    let Some((after_cursor, channels, initial_limit)) =
        state.session_timeline_watch_request(session_id)
    else {
        return ClientUpdate::default();
    };

    state.set_session_timeline_watch_loading(session_id);
    ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::WatchTimeline {
        session_id: session_id.to_owned(),
        after_cursor,
        channels,
        initial_limit,
    }))
}

fn apply_watch_update(state: &mut ClientState, update: SessionWatchUpdate) -> ClientUpdate {
    if matches!(update.event, SessionWatchEvent::Snapshot(_)) {
        apply_watch_event(state, update);
        return ClientUpdate::default();
    }

    if update.sequence != state.sessions.last_sequence.saturating_add(1) {
        apply_watch_event(state, update);
        return ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::List));
    }

    apply_watch_event(state, update);
    ClientUpdate::default()
}

fn timeline_watch_failed(
    state: &mut ClientState,
    session_id: &str,
    message: String,
) -> ClientUpdate {
    let Some(timeline) = state.sessions.timelines.get(session_id) else {
        return ClientUpdate::default();
    };

    if timeline.watch.interests == 0 {
        return ClientUpdate::default();
    }

    let delay_ms = state.set_session_timeline_watch_failed(session_id, message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Session(
            SessionEffect::SleepTimelineReconnect {
                session_id: session_id.to_owned(),
                delay_ms,
            },
        ))
    } else {
        state.set_session_timeline_watch_waiting(session_id);
        ClientUpdate::default()
    }
}

fn operation_session_id(state: &ClientState, operation_key: &str) -> String {
    match state.operations.get(operation_key) {
        Some(crate::ClientOperationState::SessionSubmitUserMessage { session_id, .. })
        | Some(crate::ClientOperationState::SessionInterrupt { session_id, .. }) => {
            session_id.clone()
        }
        _ => String::new(),
    }
}

fn operation_session_and_request_id(state: &ClientState, operation_key: &str) -> (String, String) {
    match state.operations.get(operation_key) {
        Some(crate::ClientOperationState::SessionRespondExternalRequest {
            session_id,
            request_id,
            ..
        }) => (session_id.clone(), request_id.clone()),
        _ => (String::new(), String::new()),
    }
}

#[allow(clippy::semicolon_if_nothing_returned)]
fn apply_watch_event(state: &mut ClientState, update: SessionWatchUpdate) {
    match update.event {
        SessionWatchEvent::Snapshot(sessions) => {
            state.set_session_watch_snapshot(update.sequence, sessions)
        }
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
