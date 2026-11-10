//! Agent Skills configuration.

use std::path::PathBuf;

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
}
