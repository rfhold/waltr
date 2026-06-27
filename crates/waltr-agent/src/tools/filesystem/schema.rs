//! Filesystem tool schemas.

use std::path::{Path, PathBuf};

use globset::GlobSet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{PermissionOperation, ToolDefinition};

/// Input for the `filesystem.read` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadInput {
    /// File or directory path.
    pub file_path: String,
    /// One-indexed line offset for file reads.
    pub offset: Option<usize>,
    /// Maximum number of lines to return for file reads.
    pub limit: Option<usize>,
}

impl ReadInput {
    /// Create read input for a file or directory path.
    pub fn new(file_path: impl Into<String>) -> Self {
        Self {
            file_path: file_path.into(),
            offset: None,
            limit: None,
        }
    }

    /// Set a one-indexed line offset.
    #[must_use]
    pub const fn with_offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Set a maximum line count.
    #[must_use]
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// Input for the `filesystem.glob` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobInput {
    /// Glob pattern to match.
    pub pattern: String,
    /// Optional directory path to search.
    pub path: Option<String>,
}

impl GlobInput {
    /// Create glob input for a pattern.
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            path: None,
        }
    }

    /// Set the directory path to search.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// Input for the `filesystem.grep` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepInput {
    /// Literal pattern to search for.
    pub pattern: String,
    /// Optional file or directory path to search.
    pub path: Option<String>,
    /// Optional glob pattern for files to include.
    pub include: Option<String>,
}

impl GrepInput {
    /// Create grep input for a literal pattern.
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            path: None,
            include: None,
        }
    }

    /// Set the file or directory path to search.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the include glob.
    #[must_use]
    pub fn with_include(mut self, include: impl Into<String>) -> Self {
        self.include = Some(include.into());
        self
    }
}

/// Input for the `filesystem.edit` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditInput {
    /// File path to edit.
    pub file_path: String,
    /// Existing exact string to replace.
    pub old_string: String,
    /// Replacement string.
    pub new_string: String,
    /// Replace all occurrences instead of requiring exactly one match.
    pub replace_all: Option<bool>,
}

impl EditInput {
    /// Create edit input.
    pub fn new(
        file_path: impl Into<String>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> Self {
        Self {
            file_path: file_path.into(),
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: None,
        }
    }

    /// Replace all occurrences.
    #[must_use]
    pub const fn with_replace_all(mut self, replace_all: bool) -> Self {
        self.replace_all = Some(replace_all);
        self
    }
}

/// Input for the `filesystem.write` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteInput {
    /// File content to write.
    pub content: String,
    /// File path to write.
    pub file_path: String,
}

impl WriteInput {
    /// Create write input.
    pub fn new(content: impl Into<String>, file_path: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            file_path: file_path.into(),
        }
    }
}

/// Input for the `filesystem.apply_patch` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPatchInput {
    /// Patch text envelope.
    pub patch_text: String,
}

impl ApplyPatchInput {
    /// Create apply patch input.
    pub fn new(patch_text: impl Into<String>) -> Self {
        Self {
            patch_text: patch_text.into(),
        }
    }
}

/// Input for the `filesystem.delete` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteInput {
    /// File path to delete.
    pub file_path: String,
}

impl DeleteInput {
    /// Create delete input.
    pub fn new(file_path: impl Into<String>) -> Self {
        Self {
            file_path: file_path.into(),
        }
    }
}

/// Literal grep match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// File path relative to the filesystem root.
    pub path: PathBuf,
    /// One-indexed line number.
    pub line_number: usize,
    /// Matching line content.
    pub line: String,
}

pub(super) struct GrepSearch<'a> {
    pub(super) base: &'a Path,
    pub(super) pattern: &'a str,
    pub(super) include: Option<&'a GlobSet>,
}

/// Filesystem tool kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemToolKind {
    /// Read a file or directory.
    Read,
    /// Match files by glob pattern.
    Glob,
    /// Search file contents.
    Grep,
    /// Replace exact file content.
    Edit,
    /// Write a full file.
    Write,
    /// Delete a file.
    Delete,
    /// Apply a structured patch.
    ApplyPatch,
}

impl FilesystemToolKind {
    /// Provider-facing tool name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::ApplyPatch => "apply_patch",
        }
    }

    pub(super) fn definition(self) -> ToolDefinition {
        ToolDefinition::new(self.name(), self.description(), self.input_schema())
    }

    pub(super) const fn permission_operation(self) -> PermissionOperation {
        match self {
            Self::Read | Self::Glob | Self::Grep => PermissionOperation::Read,
            Self::Edit | Self::Write | Self::ApplyPatch => PermissionOperation::Write,
            Self::Delete => PermissionOperation::Delete,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Read => "Read a file or directory from the filesystem.",
            Self::Glob => "Return files matching a glob pattern.",
            Self::Grep => "Search file contents for a literal pattern.",
            Self::Edit => "Replace exact file content.",
            Self::Write => "Write full file content.",
            Self::Delete => "Delete a file.",
            Self::ApplyPatch => {
                "Apply a structured patch envelope. patchText must start with *** Begin Patch and end with *** End Patch. Use *** Add File, *** Update File, and *** Delete File sections."
            }
        }
    }

    fn input_schema(self) -> Value {
        match self {
            Self::Read => json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "File or directory path. Use a root-relative path like status.txt or an absolute path inside the filesystem root." },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 0 }
                },
                "required": ["filePath"],
                "additionalProperties": false
            }),
            Self::Glob => json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern, for example **/* to list files recursively." },
                    "path": { "type": "string", "description": "Optional root-relative directory or absolute directory inside the filesystem root. Omit this or use . to search the root." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            Self::Grep => json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Literal text to search for." },
                    "path": { "type": "string", "description": "Optional root-relative file/directory or absolute path inside the filesystem root. Omit this or use . to search the root." },
                    "include": { "type": "string", "description": "Optional glob pattern limiting searched files, for example *.txt." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            Self::Edit => json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "Root-relative file path or absolute file path inside the filesystem root." },
                    "oldString": { "type": "string" },
                    "newString": { "type": "string" },
                    "replaceAll": { "type": "boolean" }
                },
                "required": ["filePath", "oldString", "newString"],
                "additionalProperties": false
            }),
            Self::Write => json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "filePath": { "type": "string", "description": "Root-relative file path or absolute file path inside the filesystem root." }
                },
                "required": ["content", "filePath"],
                "additionalProperties": false
            }),
            Self::Delete => json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "Root-relative file path or absolute file path inside the filesystem root." }
                },
                "required": ["filePath"],
                "additionalProperties": false
            }),
            Self::ApplyPatch => json!({
                "type": "object",
                "properties": {
                    "patchText": { "type": "string", "description": "Patch envelope beginning with *** Begin Patch and ending with *** End Patch. File paths in Add/Update/Delete sections must be root-relative paths or absolute paths inside the filesystem root." }
                },
                "required": ["patchText"],
                "additionalProperties": false
            }),
        }
    }
}
