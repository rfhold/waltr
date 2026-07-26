//! Waltr access authentication reducer responsibilities.

use crate::{AccessEvent, ClientEffect, ClientState, ClientUpdate, StatusEffect};

pub fn reduce(state: &mut ClientState, event: AccessEvent) -> ClientUpdate {
    match event {
        AccessEvent::LoginRequired { error } => {
            state.require_waltr_login(error);
            ClientUpdate::default()
        }
        AccessEvent::LoginStarted => {
            state.start_waltr_login();
            ClientUpdate::default()
        }
        AccessEvent::LoginCompleted { attempt_id } => {
            let Some(epoch) = state.complete_waltr_login(&attempt_id) else {
                return ClientUpdate::default();
            };
            ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch { epoch }))
        }
        AccessEvent::LoginFailed { attempt_id, error } => {
            state.fail_waltr_login(&attempt_id, error);
            ClientUpdate::default()
        }
        AccessEvent::LoginCanceled { attempt_id } => {
            state.cancel_waltr_login(&attempt_id);
            ClientUpdate::default()
        }
        AccessEvent::ReprobeRequested => {
            let Some(epoch) = state.reprobe_waltr_access() else {
                return ClientUpdate::default();
            };
            ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch { epoch }))
        }
        AccessEvent::LoggedOut => {
            state.logout_waltr();
            ClientUpdate::with_effect(ClientEffect::Status(StatusEffect::Watch {
                epoch: state.status.watch_epoch,
            }))
        }
    }
}
