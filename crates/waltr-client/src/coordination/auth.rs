//! Auth reducer responsibilities.

use crate::{AuthEffect, AuthEvent, ClientEffect, ClientState, ClientUpdate};

pub fn reduce(state: &mut ClientState, event: AuthEvent) -> ClientUpdate {
    match event {
        AuthEvent::ListRequested => {
            state.set_auth_list_loading();
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::ListStatuses))
        }
        AuthEvent::Listed(statuses) => {
            state.set_auth_list_ready(statuses);
            ClientUpdate::default()
        }
        AuthEvent::ListFailed(error) => {
            state.set_auth_list_failed(error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::AuthorizationRequired {
            target,
            message,
            source_session_id,
        } => {
            state.enqueue_auth_requirement(target, message, source_session_id);
            ClientUpdate::default()
        }
        AuthEvent::Dismissed { target } => {
            state.remove_auth_requirement(&target);
            ClientUpdate::default()
        }
        AuthEvent::BeginRequested {
            operation_key,
            target,
        } => {
            state.set_auth_begin_loading(operation_key.clone(), target.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Begin {
                operation_key,
                target,
            }))
        }
        AuthEvent::Begun {
            operation_key,
            target,
            attempt,
        } => {
            state.set_auth_begin_ready(operation_key, target, attempt);
            ClientUpdate::default()
        }
        AuthEvent::BeginFailed {
            operation_key,
            target,
            error,
        } => {
            state.set_auth_begin_failed(operation_key, target, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::CompleteRequested {
            operation_key,
            attempt_id,
            input,
        } => {
            state.set_auth_complete_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Complete {
                operation_key,
                attempt_id,
                input,
            }))
        }
        AuthEvent::Completed {
            operation_key,
            status,
        } => {
            state.set_auth_complete_ready(operation_key, status);
            ClientUpdate::default()
        }
        AuthEvent::CompleteFailed {
            operation_key,
            error,
        } => {
            state.set_auth_complete_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::CancelRequested {
            operation_key,
            attempt_id,
        } => {
            state.set_auth_cancel_loading(operation_key.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Cancel {
                operation_key,
                attempt_id,
            }))
        }
        AuthEvent::Canceled { operation_key } => {
            state.set_auth_cancel_ready(operation_key);
            ClientUpdate::default()
        }
        AuthEvent::CancelFailed {
            operation_key,
            error,
        } => {
            state.set_auth_cancel_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        AuthEvent::LogoutRequested {
            operation_key,
            target,
        } => {
            state.set_auth_logout_loading(operation_key.clone(), target.clone());
            ClientUpdate::with_effect(ClientEffect::Auth(AuthEffect::Logout {
                operation_key,
                target,
            }))
        }
        AuthEvent::LoggedOut {
            operation_key,
            status,
        } => {
            state.set_auth_logout_ready(operation_key, status);
            ClientUpdate::default()
        }
        AuthEvent::LogoutFailed {
            operation_key,
            error,
        } => {
            state.set_auth_logout_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
    }
}
