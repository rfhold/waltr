use std::sync::Arc;

use serde::Deserialize;
use uuid::Uuid;
use waltr_datastore::Definition;

use super::error::AgentDefinitionLoadError;
use super::materialize::{ResourceMetadata, invalid_value, parse_resource};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    #[serde(rename = "apiVersion")]
    _api_version: String,
    #[serde(rename = "kind")]
    _kind: String,
    #[serde(rename = "metadata")]
    _metadata: ResourceMetadata,
    spec: Definition,
}

/// One validated DataStore in an immutable resource snapshot.
#[derive(Debug, Clone)]
pub struct ActiveDataStore {
    pub(super) definition: Arc<Definition>,
    pub(super) activation: Uuid,
}

impl ActiveDataStore {
    pub(super) fn parse(source: &str, path: &str) -> Result<Self, AgentDefinitionLoadError> {
        let Envelope { spec, .. } = parse_resource(source, path)?;
        spec.validate()
            .map_err(|error| invalid_value(path, error.to_string()))?;
        Ok(Self {
            definition: Arc::new(spec),
            activation: Uuid::new_v4(),
        })
    }

    /// Logical type, access scope, and schema for this activation.
    #[must_use]
    pub fn definition(&self) -> &Definition {
        &self.definition
    }

    /// Process-local identity of one continuously active definition.
    #[must_use]
    pub const fn activation(&self) -> Uuid {
        self.activation
    }
}
