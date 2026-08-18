//! Effective configuration views used by session runtime construction.

use std::sync::Arc;

use waltr::grpc::waltr::config::v1::Configuration;
use tokio::sync::RwLock;

/// Read-only handle over either a live resource catalog or an explicit test snapshot.
#[derive(Debug, Clone)]
pub struct ConfigHandle {
    source: ConfigSource,
}

#[derive(Debug, Clone)]
enum ConfigSource {
    Resources(crate::agent_definition::AgentDefinitionState),
    Static(Arc<RwLock<Configuration>>),
}

impl Default for ConfigHandle {
    fn default() -> Self {
        Self::new(Configuration::default())
    }
}

impl ConfigHandle {
    /// Create a handle over an explicit immutable-by-default test configuration.
    #[must_use]
    pub fn new(initial: Configuration) -> Self {
        Self {
            source: ConfigSource::Static(Arc::new(RwLock::new(initial))),
        }
    }

    /// Create a live handle backed directly by the unified resource snapshot.
    #[must_use]
    pub const fn from_resources(resources: crate::agent_definition::AgentDefinitionState) -> Self {
        Self {
            source: ConfigSource::Resources(resources),
        }
    }

    /// Return the current effective configuration.
    pub async fn load(&self) -> Configuration {
        match &self.source {
            ConfigSource::Resources(resources) => resources.snapshot().configuration().clone(),
            ConfigSource::Static(configuration) => configuration.read().await.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use waltr::grpc::waltr::config::v1::{Configuration, Provider};

    use super::ConfigHandle;

    #[tokio::test]
    async fn load_returns_explicit_snapshot() {
        let handle = ConfigHandle::new(Configuration {
            etag: "v1".to_owned(),
            providers: HashMap::from([("anthropic".to_owned(), Provider::default())]),
            ..Configuration::default()
        });
        let config = handle.load().await;
        assert_eq!(config.etag, "v1");
        assert!(config.providers.contains_key("anthropic"));
    }
}
