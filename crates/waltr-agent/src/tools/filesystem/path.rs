//! Filesystem path scoping.

use std::path::{Component, Path, PathBuf};

use crate::{LlmError, Result};

use super::FilesystemConfig;

impl FilesystemConfig {
    pub(super) fn scoped_existing_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        let path = self.resolved_existing_path(path)?;
        if !path.starts_with(&root) {
            return Err(root_escape_error(&path, &root));
        }
        Ok(path)
    }

    pub(super) fn scoped_writable_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        let full = self.resolved_writable_path(path)?;
        if !full.starts_with(&root) {
            return Err(root_escape_error(&full, &root));
        }

        Ok(full)
    }

    pub(super) fn resolved_existing_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        if path.is_absolute() {
            Ok(path.canonicalize()?)
        } else {
            Ok(root.join(path).canonicalize()?)
        }
    }

    pub(super) fn resolved_writable_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        Ok(if path.is_absolute() {
            normalize_path(path)
        } else {
            normalize_path(&root.join(path))
        })
    }
}

pub(super) fn model_path(path: &str) -> PathBuf {
    let mut path = path.trim();

    if let Some(start) = path.find("<path>") {
        let after_start = &path[start + "<path>".len()..];
        if let Some(end) = after_start.find("</path>") {
            path = &after_start[..end];
        }
    }

    path = path
        .trim()
        .trim_matches(|character| matches!(character, '`' | '"' | '\''));

    PathBuf::from(path.trim_end_matches(':'))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub(super) fn include_relative_path<'a>(path: &'a Path, base: &'a Path) -> Result<&'a Path> {
    if base.is_file() {
        return path
            .file_name()
            .map(Path::new)
            .ok_or_else(|| filesystem_error("path has no file name"));
    }
    path.strip_prefix(base)
        .map_err(|error| filesystem_error(error.to_string()))
}

fn root_escape_error(path: &Path, root: &Path) -> LlmError {
    filesystem_error(format!(
        "path escapes filesystem root: {} is outside {}. Use a path relative to the filesystem root, or an absolute path that starts with the filesystem root.",
        path.display(),
        root.display()
    ))
}

pub(super) fn filesystem_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "filesystem",
        message: message.into(),
    }
}
