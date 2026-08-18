//! Agent compilation and durable bundle API backed by the unified resource catalog.

pub use crate::resource_catalog::{
    AgentDefinitionInspection, AgentDefinitionLoadError, AgentDefinitionSnapshot,
    AgentDefinitionState, AgentDefinitionWatcher, DefinitionResourceDetail,
    DefinitionResourceSummary, DefinitionWatcherHealth, LoadedAgentDefinition, ResourceKind,
    ResourceOrigin, load_default_agent, load_default_agent_from_env,
};

pub(crate) use crate::resource_catalog::PublishedState;
