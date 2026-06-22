//! Shared configuration watch merge and healing helpers.

use crate::grpc::waltr::config::v1::Configuration;

/// Configuration watch update shape shared by client and server watchers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigWatchUpdate {
    /// Previous entity tag observed by the producer.
    pub previous_etag: String,
    /// Current entity tag observed by the producer.
    pub etag: String,
    /// Top-level keys included in a sparse config payload.
    pub changed_keys: Vec<String>,
    /// Watch error message, when the producer could not read config.
    pub error: String,
    /// Full snapshot or sparse config payload.
    pub config: Option<Configuration>,
}

/// Result of applying a watch update to a known config snapshot.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigWatchApplyOutcome {
    /// The update was applied to produce this current config.
    Updated(Configuration),
    /// The consumer should reload config from its authoritative source.
    ReloadRequired,
    /// The watch produced an error and should be retried.
    Retry(String),
}

/// Apply a config watch update to an optional current config snapshot.
#[must_use]
pub fn apply_config_watch_update(
    current: Option<&Configuration>,
    update: &ConfigWatchUpdate,
) -> ConfigWatchApplyOutcome {
    if !update.error.is_empty() {
        return ConfigWatchApplyOutcome::Retry(update.error.clone());
    }

    if update.previous_etag.is_empty() {
        return match &update.config {
            Some(config) => ConfigWatchApplyOutcome::Updated(config.clone()),
            None => ConfigWatchApplyOutcome::ReloadRequired,
        };
    }

    let Some(current) = current else {
        return ConfigWatchApplyOutcome::ReloadRequired;
    };
    let Some(patch) = update.config.as_ref() else {
        return ConfigWatchApplyOutcome::ReloadRequired;
    };
    if current.etag != update.previous_etag {
        return ConfigWatchApplyOutcome::ReloadRequired;
    }

    let mut config = current.clone();
    if changed(&update.changed_keys, "providers") {
        config.providers.clone_from(&patch.providers);
    }
    if changed(&update.changed_keys, "mcp_servers") {
        config.mcp_servers.clone_from(&patch.mcp_servers);
    }
    if changed(&update.changed_keys, "runtime_defaults") {
        config.runtime_defaults.clone_from(&patch.runtime_defaults);
    }
    if update.etag.is_empty() {
        patch.etag.clone_into(&mut config.etag);
    } else {
        update.etag.clone_into(&mut config.etag);
    }

    ConfigWatchApplyOutcome::Updated(config)
}

fn changed(keys: &[String], key: &str) -> bool {
    keys.iter().any(|changed_key| changed_key == key)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::grpc::waltr::config::v1::{Configuration, Provider, RuntimeDefaults};

    use super::{ConfigWatchApplyOutcome, ConfigWatchUpdate, apply_config_watch_update};

    #[test]
    fn applies_initial_snapshot() {
        let config = Configuration {
            etag: "v1".to_owned(),
            ..Configuration::default()
        };

        assert_eq!(
            apply_config_watch_update(
                None,
                &ConfigWatchUpdate {
                    config: Some(config.clone()),
                    ..ConfigWatchUpdate::default()
                },
            ),
            ConfigWatchApplyOutcome::Updated(config)
        );
    }

    #[test]
    fn merges_sparse_runtime_defaults_change() {
        let current = Configuration {
            etag: "v1".to_owned(),
            providers: HashMap::from([("anthropic".to_owned(), Provider::default())]),
            ..Configuration::default()
        };
        let patch = Configuration {
            etag: "v2".to_owned(),
            runtime_defaults: Some(RuntimeDefaults {
                provider_key: "anthropic".to_owned(),
                model_key: "sonnet".to_owned(),
                mcp_default_enabled: None,
                enabled_mcp_server_keys: Vec::new(),
            }),
            ..Configuration::default()
        };

        let ConfigWatchApplyOutcome::Updated(config) = apply_config_watch_update(
            Some(&current),
            &ConfigWatchUpdate {
                previous_etag: "v1".to_owned(),
                etag: "v2".to_owned(),
                changed_keys: vec!["runtime_defaults".to_owned()],
                config: Some(patch.clone()),
                ..ConfigWatchUpdate::default()
            },
        ) else {
            panic!("watch update should apply");
        };

        assert_eq!(config.etag, "v2");
        assert_eq!(config.providers, current.providers);
        assert_eq!(config.runtime_defaults, patch.runtime_defaults);
    }

    #[test]
    fn reloads_on_etag_gap() {
        let current = Configuration {
            etag: "v1".to_owned(),
            ..Configuration::default()
        };

        assert_eq!(
            apply_config_watch_update(
                Some(&current),
                &ConfigWatchUpdate {
                    previous_etag: "missing".to_owned(),
                    config: Some(Configuration::default()),
                    ..ConfigWatchUpdate::default()
                },
            ),
            ConfigWatchApplyOutcome::ReloadRequired
        );
    }
}
