//! Permission claims and resources.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Abstract operation requested by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionOperation {
    /// Read a resource.
    Read,
    /// Write or update a resource.
    Write,
    /// Delete a resource.
    Delete,
    /// Execute a command or program.
    Execute,
    /// Access a network resource.
    Network,
}

/// Abstract resource requested by a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionResource {
    /// Concrete filesystem path.
    FilesystemPath(PathBuf),
    /// Filesystem path prefix.
    FilesystemPathPrefix(PathBuf),
    /// Working directory.
    WorkingDirectory(PathBuf),
    /// Shell command.
    ShellCommand(String),
}

/// One permission claim extracted from a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionClaim {
    /// Requested operation.
    pub operation: PermissionOperation,
    /// Requested resource.
    pub resource: PermissionResource,
    /// Human-readable summary for prompts and diagnostics.
    pub summary: String,
}

impl PermissionClaim {
    /// Create a permission claim.
    pub fn new(
        operation: PermissionOperation,
        resource: PermissionResource,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            operation,
            resource,
            summary: summary.into(),
        }
    }

    pub(in crate::permission) fn directory_path(&self) -> Option<&Path> {
        match &self.resource {
            PermissionResource::FilesystemPath(path)
            | PermissionResource::FilesystemPathPrefix(path)
            | PermissionResource::WorkingDirectory(path) => Some(path),
            PermissionResource::ShellCommand(_) => None,
        }
    }
}
