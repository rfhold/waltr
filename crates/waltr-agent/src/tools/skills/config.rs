//! Agent Skills configuration.

use std::path::PathBuf;

use super::{
    definition::{SkillDefinition, canonicalize_or_clone},
    discovery::discover_skills_root,
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
}
