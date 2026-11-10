//! Permission patterns.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    PermissionClaim, PermissionInputScalar, PermissionJsonPointer, PermissionOperation,
    PermissionResource,
};

/// Path pattern used by permission policy rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionPathPattern {
    /// Match any path.
    Any,
    /// Match one exact path.
    Exact(PathBuf),
    /// Match a path prefix.
    Prefix(PathBuf),
    /// Match paths outside one root.
    OutsideRoot(PathBuf),
    /// Minimal glob-like wildcard pattern. `*` matches all paths and `prefix/*` matches a prefix.
    Wildcard(String),
}

impl PermissionPathPattern {
    pub(in crate::permission) fn matches(&self, path: &Path) -> Option<usize> {
        match self {
            Self::Any => Some(0),
            Self::Exact(pattern) => (path == pattern).then_some(usize::MAX),
            Self::Prefix(pattern) => path
                .starts_with(pattern)
                .then_some(pattern.as_os_str().len()),
            Self::OutsideRoot(root) => (!path.starts_with(root)).then_some(root.as_os_str().len()),
            Self::Wildcard(pattern) if pattern == "*" => Some(0),
            Self::Wildcard(pattern) if pattern.ends_with("/*") => {
                let prefix = &pattern[..pattern.len() - 2];
                path.to_str()
                    .is_some_and(|path| path.starts_with(prefix))
                    .then_some(prefix.len())
            }
            Self::Wildcard(pattern) => path
                .to_str()
                .is_some_and(|path| path == pattern)
                .then_some(pattern.len()),
        }
    }
}

/// Command pattern used by permission policy rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionCommandPattern {
    /// Match any command.
    Any,
    /// Match an exact full command string.
    Exact(String),
    /// Match a command prefix.
    Prefix(String),
    /// Match the executable name at the start of the command string.
    Name(String),
}

impl PermissionCommandPattern {
    pub(in crate::permission) fn matches(&self, command: &str) -> Option<usize> {
        match self {
            Self::Any => Some(0),
            Self::Exact(pattern) => (command == pattern).then_some(usize::MAX),
            Self::Prefix(pattern) => command.starts_with(pattern).then_some(pattern.len()),
            Self::Name(name) => command
                .split_whitespace()
                .next()
                .is_some_and(|command_name| command_name == name)
                .then_some(name.len()),
        }
    }
}

/// Reusable permission approval pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPattern {
    /// Operation matched by this pattern. `None` matches any operation.
    pub operation: Option<PermissionOperation>,
    /// Resource matcher.
    pub resource: PermissionResourcePattern,
}

impl PermissionPattern {
    /// Create a filesystem prefix pattern for one operation.
    pub fn filesystem_prefix(operation: PermissionOperation, path: impl Into<PathBuf>) -> Self {
        Self {
            operation: Some(operation),
            resource: PermissionResourcePattern::FilesystemPathPrefix(path.into()),
        }
    }

    /// Return true if this pattern matches a claim.
    #[must_use]
    pub fn matches(&self, claim: &PermissionClaim) -> bool {
        if self
            .operation
            .is_some_and(|operation| operation != claim.operation)
        {
            return false;
        }
        self.resource.matches(&claim.resource)
    }
}

/// Resource matcher for reusable permission approvals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionResourcePattern {
    /// Exact filesystem path.
    FilesystemPath(PathBuf),
    /// Filesystem path prefix.
    FilesystemPathPrefix(PathBuf),
    /// Working directory prefix.
    WorkingDirectoryPrefix(PathBuf),
    /// Exact shell command.
    ShellCommand(String),
    /// Shell command prefix.
    ShellCommandPrefix(String),
    /// Exact scalar selected from one concrete tool input.
    ToolInput {
        /// Concrete provider-facing tool name.
        tool: String,
        /// Canonical JSON Pointer.
        pointer: PermissionJsonPointer,
        /// Exact observed typed scalar.
        value: PermissionInputScalar,
    },
}

impl PermissionResourcePattern {
    fn matches(&self, resource: &PermissionResource) -> bool {
        match (self, resource) {
            (Self::FilesystemPath(pattern), PermissionResource::FilesystemPath(path)) => {
                path == pattern
            }
            (
                Self::FilesystemPathPrefix(pattern),
                PermissionResource::FilesystemPath(path)
                | PermissionResource::FilesystemPathPrefix(path),
            ) => path.starts_with(pattern),
            (Self::WorkingDirectoryPrefix(pattern), PermissionResource::WorkingDirectory(path)) => {
                path.starts_with(pattern)
            }
            (Self::ShellCommand(pattern), PermissionResource::ShellCommand(command)) => {
                command == pattern
            }
            (Self::ShellCommandPrefix(pattern), PermissionResource::ShellCommand(command)) => {
                command.starts_with(pattern)
            }
            (
                Self::ToolInput {
                    tool: pattern_tool,
                    pointer: pattern_pointer,
                    value: pattern_value,
                },
                PermissionResource::ToolInput {
                    tool,
                    pointer,
                    value,
                },
            ) => pattern_tool == tool && pattern_pointer == pointer && pattern_value == value,
            _ => false,
        }
    }
}
