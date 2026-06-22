//! Configuration reducer responsibilities.

use crate::{ClientEffect, ClientState, ClientUpdate, ConfigEffect, ConfigEvent, OperationStatus};
use waltr::config::{ConfigWatchApplyOutcome, ConfigWatchUpdate, apply_config_watch_update};

pub fn reduce(state: &mut ClientState, event: ConfigEvent) -> ClientUpdate {
    match event {
        ConfigEvent::LoadRequested => {
            state.set_config_load_loading();
            ClientUpdate::with_effect(ClientEffect::Config(ConfigEffect::Load))
        }
        ConfigEvent::Loaded(config) => {
            state.set_config_load_ready(config);
            ClientUpdate::default()
        }
        ConfigEvent::LoadFailed(error) => {
            state.set_config_load_failed(error.to_string());
            ClientUpdate::default()
        }
        ConfigEvent::PatchRequested(operation_key) => {
            state.set_config_patch_loading(operation_key);
            ClientUpdate::default()
        }
        ConfigEvent::Patched {
            operation_key,
            config,
        } => {
            state.set_config_patch_ready(operation_key, config);
            ClientUpdate::default()
        }
        ConfigEvent::PatchFailed {
            operation_key,
            error,
        } => {
            state.set_config_patch_failed(operation_key, error.to_string());
            ClientUpdate::default()
        }
        ConfigEvent::WatchInterestRegistered => {
            state.add_config_watch_interest();
            maintain_watch(state)
        }
        ConfigEvent::WatchInterestReleased => {
            state.remove_config_watch_interest();
            ClientUpdate::default()
        }
        ConfigEvent::WatchStarted => {
            state.set_config_watch_opened();
            ClientUpdate::default()
        }
        ConfigEvent::WatchChanged(update) => apply_watch_update(state, &update),
        ConfigEvent::WatchFailed(error) => watch_failed(state, error.to_string()),
        ConfigEvent::WatchEnded => watch_failed(state, "config watch ended".to_owned()),
        ConfigEvent::ReconnectReady => maintain_watch(state),
    }
}

pub fn maintain_watch(state: &mut ClientState) -> ClientUpdate {
    if state.config.watch.interests == 0 {
        return ClientUpdate::default();
    }

    if matches!(
        state.config.watch.status,
        OperationStatus::Loading | OperationStatus::Opened | OperationStatus::Received
    ) {
        return ClientUpdate::default();
    }

    if !state.permits_watch_maintenance() {
        state.set_config_watch_waiting();
        return ClientUpdate::default();
    }

    state.set_config_watch_loading();
    ClientUpdate::with_effect(ClientEffect::Config(ConfigEffect::Watch))
}

fn apply_watch_update(state: &mut ClientState, update: &ConfigWatchUpdate) -> ClientUpdate {
    match apply_config_watch_update(state.config.current.as_ref(), update) {
        ConfigWatchApplyOutcome::Retry(error) => {
            state.set_config_watch_changed(None, error);
            watch_retry(state)
        }
        ConfigWatchApplyOutcome::ReloadRequired => {
            state.set_config_watch_changed(None, String::new());
            ClientUpdate::with_effect(ClientEffect::Config(ConfigEffect::Load))
        }
        ConfigWatchApplyOutcome::Updated(config) => {
            state.set_config_watch_changed(Some(config), String::new());
            ClientUpdate::default()
        }
    }
}

fn watch_failed(state: &mut ClientState, message: String) -> ClientUpdate {
    if state.config.watch.interests == 0 {
        return ClientUpdate::default();
    }

    let delay_ms = state.set_config_watch_failed(message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Config(ConfigEffect::SleepReconnect {
            delay_ms,
        }))
    } else {
        state.set_config_watch_waiting();
        ClientUpdate::default()
    }
}

fn watch_retry(state: &mut ClientState) -> ClientUpdate {
    let message = state
        .config
        .watch
        .error
        .clone()
        .unwrap_or_else(|| "config watch failed".to_owned());
    let delay_ms = state.set_config_watch_failed(message);

    if state.permits_watch_maintenance() {
        ClientUpdate::with_effect(ClientEffect::Config(ConfigEffect::SleepReconnect {
            delay_ms,
        }))
    } else {
        state.set_config_watch_waiting();
        ClientUpdate::default()
    }
}
