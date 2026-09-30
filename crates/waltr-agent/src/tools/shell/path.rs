use std::path::{Component, Path, PathBuf};

use super::ShellConfig;

impl ShellConfig {
    pub(super) fn root_path(&self) -> PathBuf {
        normalize_path(&self.root)
    }

    pub(super) fn resolved_workdir(&self, requested: Option<&str>) -> PathBuf {
        let root = self.root_path();
        let workdir = requested.map_or_else(|| self.default_workdir.clone(), model_path);
        if workdir.is_absolute() {
            normalize_path(&workdir)
        } else {
            normalize_path(&root.join(workdir))
        }
    }
}

pub(super) fn resolve_model_path(path: &str, root: &Path, workdir: &Path) -> PathBuf {
    let path = model_path(path);
    if path.starts_with("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        let stripped = path.strip_prefix("~").unwrap_or(&path);
        return normalize_path(&PathBuf::from(home).join(stripped));
    }
    if path.is_absolute() {
        normalize_path(&path)
    } else if workdir.is_absolute() {
        normalize_path(&workdir.join(path))
    } else {
        normalize_path(&root.join(workdir).join(path))
    }
}

pub(super) fn is_dynamic_path(token: &str) -> bool {
    token.contains('$') || token.contains('`') || token.contains("<(") || token.contains(">(")
}

fn model_path(path: &str) -> PathBuf {
    let path = path
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
