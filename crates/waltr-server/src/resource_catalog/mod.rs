//! Unified resource catalog state, persistence, materialization, and watcher lifecycle.

mod agent;
mod error;
mod materialize;
pub(crate) mod routine;
mod state;
mod store;
mod watcher;

pub use routine::{MaterializedRoutine, RoutineDefinition};
pub use state::{
    AgentDefinitionInspection, AgentDefinitionLoadError, AgentDefinitionSnapshot,
    AgentDefinitionState, DefinitionResourceDetail, DefinitionResourceSummary,
    LoadedAgentDefinition, ResourceKind, ResourceOrigin, load_default_agent,
    load_default_agent_from_env,
};
pub use watcher::{AgentDefinitionWatcher, DefinitionWatcherHealth};

pub(crate) use state::PublishedState;
