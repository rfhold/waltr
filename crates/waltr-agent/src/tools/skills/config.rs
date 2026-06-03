//! Agent Skills configuration.

use std::path::PathBuf;

use crate::AgentTool;

use super::{
    definition::{SkillDefinition, canonicalize_or_clone},
    discovery::discover_skills_root,
    load_tool::{load_skill, load_skill_definition},
    render::render_skills_catalog,
};

/// Agent Skills configuration for one filesystem root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsConfig {
    /// Filesystem root whose `.agents/skills` directory is initially visible.
    pub root: PathBuf,
}

impl SkillsConfig {
    /// Create skills configuration for a filesystem root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Discover skills visible at this root's `.agents/skills` directory.
    #[must_use]
    pub fn discover(&self) -> Vec<SkillDefinition> {
        discover_skills_root(&canonicalize_or_clone(&self.root))
    }

    /// Render a system prompt catalog for initially visible skills.
    #[must_use]
    pub fn system_prompt(&self) -> Option<String> {
        render_skills_catalog(&self.discover(), "Available skills:")
    }

    /// Convert this configuration into the dedicated skill loading tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        AgentTool::with_context(load_skill_definition(), load_skill)
    }
}
