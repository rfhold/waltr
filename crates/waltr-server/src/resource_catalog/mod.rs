//! Unified resource catalog state, persistence, materialization, and watcher lifecycle.

mod agent;
mod data_store;
mod error;
mod materialize;
mod remote;
pub(crate) mod routine;
pub mod speech;
mod state;
mod store;
mod watcher;

pub use data_store::ActiveDataStore;
pub use remote::{Remote, RemoteBinding};
pub use state::{
    AgentDefinitionInspection, AgentDefinitionLoadError, AgentDefinitionSnapshot,
    AgentDefinitionState, DefinitionResourceDetail, DefinitionResourceSummary,
    LoadedAgentDefinition, ResourceKind, ResourceOrigin, load_default_agent,
    load_default_agent_from_env,
};
pub use watcher::{AgentDefinitionWatcher, DefinitionWatcherHealth};

pub(crate) use state::PublishedState;
pub(crate) use store::provision_persisted_resource;
