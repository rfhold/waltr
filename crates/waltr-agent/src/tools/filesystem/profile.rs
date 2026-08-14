//! Filesystem tool profiles.

use crate::{ModelRef, ProviderId};

use super::FilesystemToolKind;

/// Requested filesystem tool profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemToolProfile {
    /// Pick a best-effort profile from provider and model metadata.
    Auto,
    /// Expose exact string replacement editing tools.
    ExactEdit,
    /// Expose whole-file write editing tools.
    WholeFileWrite,
    /// Expose structured patch editing tools.
    StructuredPatch,
}

impl FilesystemToolProfile {
    /// Resolve the requested profile for a specific model.
    #[must_use]
    pub fn resolve(self, model: &ModelRef) -> ResolvedFilesystemToolProfile {
        match self {
            Self::Auto => resolve_auto_profile(model),
            Self::ExactEdit => ResolvedFilesystemToolProfile::ExactEdit,
            Self::WholeFileWrite => ResolvedFilesystemToolProfile::WholeFileWrite,
            Self::StructuredPatch => ResolvedFilesystemToolProfile::StructuredPatch,
        }
    }
}

/// Concrete filesystem profile exposed to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFilesystemToolProfile {
    /// Exact string replacement plus whole-file write.
    ExactEdit,
    /// Whole-file writes only.
    WholeFileWrite,
    /// Structured patch envelope edits.
    StructuredPatch,
}

/// Filesystem tools exposed for a resolved profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemToolSet {
    /// Read-only discovery/read tools.
    pub read_tools: Vec<FilesystemToolKind>,
    /// Mutation tools selected for the model profile.
    pub edit_tools: Vec<FilesystemToolKind>,
}

impl FilesystemToolSet {
    /// Build a tool set for a resolved profile.
    #[must_use]
    pub fn for_profile(profile: ResolvedFilesystemToolProfile) -> Self {
        let edit_tools = match profile {
            ResolvedFilesystemToolProfile::ExactEdit => {
                vec![
                    FilesystemToolKind::Edit,
                    FilesystemToolKind::Write,
                    FilesystemToolKind::Delete,
                ]
            }
            ResolvedFilesystemToolProfile::WholeFileWrite => {
                vec![FilesystemToolKind::Write, FilesystemToolKind::Delete]
            }
            ResolvedFilesystemToolProfile::StructuredPatch => {
                vec![FilesystemToolKind::ApplyPatch]
            }
        };

        Self {
            read_tools: vec![
                FilesystemToolKind::Read,
                FilesystemToolKind::Glob,
                FilesystemToolKind::Grep,
            ],
            edit_tools,
        }
    }

    /// Return all exposed tools in presentation order.
    #[must_use]
    pub fn all(&self) -> Vec<FilesystemToolKind> {
        self.read_tools
            .iter()
            .chain(&self.edit_tools)
            .copied()
            .collect()
    }
}

fn resolve_auto_profile(model: &ModelRef) -> ResolvedFilesystemToolProfile {
    let model_id = model.model.as_str();

    if model.provider == ProviderId::OpenAi
        && model_id.contains("gpt-")
        && !model_id.contains("gpt-4")
        && !model_id.contains("oss")
    {
        return ResolvedFilesystemToolProfile::StructuredPatch;
    }

    ResolvedFilesystemToolProfile::ExactEdit
}
