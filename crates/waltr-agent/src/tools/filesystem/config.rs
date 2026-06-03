//! Filesystem tool configuration.

use std::path::PathBuf;

use crate::ModelRef;

use super::{FilesystemToolProfile, FilesystemToolSet, ResolvedFilesystemToolProfile};

/// Filesystem tool configuration for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemConfig {
    /// Root directory filesystem tools are scoped to.
    pub root: PathBuf,
    /// Tool profile to expose to the model.
    pub profile: FilesystemToolProfile,
    /// Whether file reads append relevant `AGENTS.md` context.
    pub agents_read_context: bool,
    /// Whether filesystem operations reveal newly encountered Agent Skills catalogs.
    pub skills_context: bool,
}

impl FilesystemConfig {
    /// Create filesystem tool configuration with automatic profile selection.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            profile: FilesystemToolProfile::Auto,
            agents_read_context: false,
            skills_context: false,
        }
    }

    /// Override the filesystem tool profile.
    #[must_use]
    pub const fn with_profile(mut self, profile: FilesystemToolProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Append relevant `AGENTS.md` content when the agent read tool reads files.
    #[must_use]
    pub const fn with_agents_read_context(mut self) -> Self {
        self.agents_read_context = true;
        self
    }

    /// Reveal newly encountered `.agents/skills` catalogs from filesystem operations.
    #[must_use]
    pub const fn with_skills_context(mut self) -> Self {
        self.skills_context = true;
        self
    }

    /// Resolve the configured profile for a specific model.
    #[must_use]
    pub fn resolve_profile(&self, model: &ModelRef) -> ResolvedFilesystemToolProfile {
        self.profile.resolve(model)
    }

    /// Return the filesystem tools that should be exposed for a model.
    #[must_use]
    pub fn exposed_tools(&self, model: &ModelRef) -> FilesystemToolSet {
        FilesystemToolSet::for_profile(self.resolve_profile(model))
    }
}
