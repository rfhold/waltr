//! Session reducer responsibilities.

use crate::transport::{SessionWatchEvent, SessionWatchUpdate};
use crate::{
    AuthEvent, ClientEffect, ClientState, ClientUpdate, OperationStatus, SessionEffect,
    SessionEvent, TimelineWatchKey,
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
        SessionEvent::TemplatesSearchRequested {
            request_id,
            session_id,
            query,
            limit,
            kinds,
            target,
            cursor,
        } => {
            state.set_session_template_search_loading(
                request_id.clone(),
                session_id.clone(),
                query.clone(),
            );
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::SearchTemplates {
                request_id,
                session_id,
                query,
                limit,
                kinds,
                target,
                cursor,
            }))
        }
        SessionEvent::TemplatesSearched {
            request_id,
            session_id,
            options,
        } => {
            state.set_session_template_search_ready(&request_id, &session_id, options);
            ClientUpdate::default()
        }
        SessionEvent::TemplatesSearchFailed {
            request_id,
            session_id,
            error,
        } => {
            state.set_session_template_search_failed(&request_id, &session_id, error.to_string());
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
            let key = TimelineWatchKey::new(session_id, after_cursor, channels, initial_limit);
            state.configure_session_timeline_watch(key.clone());
            maintain_timeline_watch(state, &key)
        }
        SessionEvent::TimelineWatchInterestReleased { session_id } => {
            state.remove_session_timeline_watch_interest(&session_id);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchStarted { key } => {
            state.set_session_timeline_watch_opened(&key);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchChanged { key, update } => {
            state.set_session_timeline_watch_changed(&key, update.sequence, update.event);
            ClientUpdate::default()
        }
        SessionEvent::TimelineWatchFailed { key, error } => {
            timeline_watch_failed(state, &key, error.to_string())
        }
        SessionEvent::TimelineWatchEnded { key } => {
            timeline_watch_failed(state, &key, "timeline watch ended".to_owned())
        }
        SessionEvent::TimelineReconnectReady { key } => maintain_timeline_watch(state, &key),
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
            let target = error.auth_target().cloned();
            let message = error.to_string();
            state.set_session_submit_user_message_failed(
                operation_key,
                session_id.clone(),
                message.clone(),
            );
            if let Some(target) = target {
                return reduce(
                    state,
                    SessionEvent::AuthRequiredFromSession {
                        target,
                        message,
                        session_id,
                    },
                );
            }
            ClientUpdate::default()
        }
        SessionEvent::AuthRequiredFromSession {
            target,
            message,
            session_id,
        } => crate::coordination::auth::reduce(
            state,
            AuthEvent::AuthorizationRequired {
                target,
                message,
                source_session_id: Some(session_id),
            },
        ),
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
        SessionEvent::RollbackRequested {
            operation_key,
            session_id,
            target,
        } => {
            state.set_session_rollback_loading(operation_key.clone(), session_id.clone());
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Rollback {
                operation_key,
                session_id,
                target,
            }))
        }
        SessionEvent::RolledBack {
            operation_key,
            session,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_rollback_ready(operation_key, session_id, session);
            ClientUpdate::default()
        }
        SessionEvent::RollbackFailed {
            operation_key,
            error,
        } => {
            let session_id = operation_session_id(state, &operation_key);
            state.set_session_rollback_failed(operation_key, session_id, error.to_string());
            ClientUpdate::default()
        }
        SessionEvent::ContinueRequested {
            operation_key,
            session_id,
            target,
        } => {
            state.set_session_continue_loading(
                operation_key.clone(),
                session_id.clone(),
                target.clone(),
            );
            ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::Continue {
                operation_key,
                session_id,
                target,
            }))
        }
        SessionEvent::Continued { operation_key } => {
            let (session_id, target) = operation_session_and_target(state, &operation_key);
            state.set_session_continue_ready(operation_key, session_id, target);
            ClientUpdate::default()
        }
        SessionEvent::ContinueFailed {
            operation_key,
            error,
        } => {
            let (session_id, target) = operation_session_and_target(state, &operation_key);
            state.set_session_continue_failed(operation_key, session_id, target, error.to_string());
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

pub fn maintain_timeline_watch(state: &mut ClientState, key: &TimelineWatchKey) -> ClientUpdate {
    if !state.session_timeline_watch_is_active(key) {
        return ClientUpdate::default();
    }

    let Some(timeline) = state.sessions.timelines.get(&key.session_id) else {
        return ClientUpdate::default();
    };

    let Some(watch) = timeline.watches.get(key) else {
        return ClientUpdate::default();
    };

    if matches!(
        watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    ) {
        return ClientUpdate::default();
    }

    if !state.permits_watch_maintenance() {
        state.set_session_timeline_watch_waiting(key);
        return ClientUpdate::default();
    }

    let Some((after_cursor, channels, initial_limit)) = state.session_timeline_watch_request(key)
    else {
        return ClientUpdate::default();
    };

    state.set_session_timeline_watch_loading(key);
    ClientUpdate::with_effect(ClientEffect::Session(SessionEffect::WatchTimeline {
        key: TimelineWatchKey::new(
            key.session_id.clone(),
            after_cursor,
            channels,
            initial_limit,
        ),
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
    key: &TimelineWatchKey,
    message: String,
) -> ClientUpdate {
    if !state.session_timeline_watch_is_active(key) {
        return ClientUpdate::default();
    }

    let delay_ms = state.set_session_timeline_watch_failed(key, message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Session(
            SessionEffect::SleepTimelineReconnect {
                key: key.clone(),
                delay_ms,
            },
        ))
    } else {
        state.set_session_timeline_watch_waiting(key);
        ClientUpdate::default()
    }
}

fn operation_session_id(state: &ClientState, operation_key: &str) -> String {
    match state.operations.get(operation_key) {
        Some(
            crate::ClientOperationState::SessionSubmitUserMessage { session_id, .. }
            | crate::ClientOperationState::SessionInterrupt { session_id, .. }
            | crate::ClientOperationState::SessionRollback { session_id, .. },
        ) => session_id.clone(),
        _ => String::new(),
    }
}

fn operation_session_and_target(
    state: &ClientState,
    operation_key: &str,
) -> (String, crate::RollbackTarget) {
    match state.operations.get(operation_key) {
        Some(crate::ClientOperationState::SessionContinue {
            session_id, target, ..
        }) => (session_id.clone(), target.clone()),
        _ => (String::new(), crate::RollbackTarget { target: None }),
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
