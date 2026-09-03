//! Agent registry compilation.

use waltr_agent::{
    AgentDefinitionCatalog, AgentDefinitionError, AgentDefinitionRegistry,
    AgentDefinitionResourceKind,
};

use super::error::AgentDefinitionLoadError;

pub(super) fn compile_catalog(
    sources: &[String],
    default_agent: &str,
) -> Result<AgentDefinitionCatalog, AgentDefinitionLoadError> {
    let mut registry = AgentDefinitionRegistry::new();
    for source in sources {
        registry
            .load_yaml(source)
            .map_err(AgentDefinitionLoadError::DefinitionSource)?;
    }
    let agents = registry
        .resolve_agents()
        .map_err(|source| AgentDefinitionLoadError::ResolveAll { source })?;
    agents
        .iter()
        .find(|agent| agent.name == default_agent)
        .ok_or_else(|| AgentDefinitionLoadError::Resolve {
            name: default_agent.to_owned(),
            source: AgentDefinitionError::MissingReference {
                kind: AgentDefinitionResourceKind::Agent,
                name: default_agent.to_owned(),
                field: None,
            },
        })?;
    AgentDefinitionCatalog::from_resolved_agents(sources.to_vec(), default_agent, agents)
        .map_err(|source| AgentDefinitionLoadError::ResolveAll { source })
}
