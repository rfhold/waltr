//! Agent registry compilation and compiled-feature validation.

use waltr_agent::{
    AgentDefinitionCatalog, AgentDefinitionError, AgentDefinitionRegistry,
    AgentDefinitionResourceKind, ResolvedAgent,
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
    for agent in &agents {
        validate_compiled_features(agent)?;
    }
    let resolved = agents
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
    validate_compiled_features(resolved)?;
    AgentDefinitionCatalog::from_resolved_agents(sources.to_vec(), default_agent, agents)
        .map_err(|source| AgentDefinitionLoadError::ResolveAll { source })
}

#[cfg(not(feature = "shell"))]
fn validate_compiled_features(agent: &ResolvedAgent) -> Result<(), AgentDefinitionLoadError> {
    if agent.features.shell.is_some() {
        return Err(AgentDefinitionLoadError::UnsupportedFeature {
            name: agent.name.clone(),
            feature: "shell",
        });
    }
    Ok(())
}

#[cfg(feature = "shell")]
#[allow(clippy::unnecessary_wraps)]
const fn validate_compiled_features(_: &ResolvedAgent) -> Result<(), AgentDefinitionLoadError> {
    Ok(())
}
