#![allow(clippy::similar_names)]

use std::{fs, path::Path};
use tokio_util::sync::CancellationToken;

use crate::tools::{Result, SandboxError, check_cancel, resolved};

#[derive(Debug)]
enum Change {
    Add(String, String),
    Update(String, Vec<(Vec<String>, Vec<String>)>),
    Delete(String),
}

#[allow(clippy::too_many_lines)]
fn changes(patch: &str) -> Result<Vec<Change>> {
    let mut lines = patch.lines().peekable();
    if lines.next() != Some("*** Begin Patch") {
        return Err(SandboxError("patch must start with *** Begin Patch".into()));
    }
    let mut changes = Vec::new();
    loop {
        let Some(header) = lines.next() else {
            return Err(SandboxError("patch missing *** End Patch".into()));
        };
        if header == "*** End Patch" {
            if lines.next().is_some() {
                return Err(SandboxError("trailing patch content".into()));
            }
            return Ok(changes);
        }
        let (kind, path) = if let Some(path) = header.strip_prefix("*** Add File: ") {
            ("add", path)
        } else if header == "*** Add File" {
            ("add", next_path(&mut lines)?)
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            ("update", path)
        } else if header == "*** Update File" {
            ("update", next_path(&mut lines)?)
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            ("delete", path)
        } else if header == "*** Delete File" {
            ("delete", next_path(&mut lines)?)
        } else {
            return Err(SandboxError(format!("unsupported patch line: {header}")));
        };
        if path.is_empty() {
            return Err(SandboxError("empty patch path".into()));
        }
        match kind {
            "add" => {
                let mut content = String::new();
                while lines.peek().is_some_and(|line| !line.starts_with("*** ")) {
                    let line = lines.next().expect("peeked line");
                    let text = line
                        .strip_prefix('+')
                        .ok_or_else(|| SandboxError("add file lines must start with +".into()))?;
                    content.push_str(text);
                    content.push('\n');
                }
                changes.push(Change::Add(path.into(), content));
            }
            "delete" => changes.push(Change::Delete(path.into())),
            _ => {
                let mut chunks = Vec::new();
                let mut old = Vec::new();
                let mut new = Vec::new();
                while lines.peek().is_some_and(|line| !line.starts_with("*** ")) {
                    let line = lines.next().expect("peeked line");
                    if line.starts_with("@@") {
                        if !old.is_empty() || !new.is_empty() {
                            chunks.push((old, new));
                            old = Vec::new();
                            new = Vec::new();
                        }
                        let context = line.trim_start_matches("@@").trim();
                        if !context.is_empty() {
                            old.push(context.into());
                            new.push(context.into());
                        }
                    } else if let Some(text) = line.strip_prefix(' ') {
                        old.push(text.into());
                        new.push(text.into());
                    } else if let Some(text) = line.strip_prefix('-') {
                        old.push(text.into());
                    } else if let Some(text) = line.strip_prefix('+') {
                        new.push(text.into());
                    } else {
                        return Err(SandboxError("invalid update line".into()));
                    }
                }
                if !old.is_empty() || !new.is_empty() {
                    chunks.push((old, new));
                }
                if chunks.is_empty() {
                    return Err(SandboxError("update patch has no changes".into()));
                }
                changes.push(Change::Update(path.into(), chunks));
            }
        }
    }
}

fn next_path<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Result<&'a str> {
    while lines.peek().is_some_and(|line| line.trim().is_empty()) {
        lines.next();
    }
    let line = lines
        .next()
        .filter(|line| !line.starts_with("*** "))
        .ok_or_else(|| SandboxError("patch operation missing file path".into()))?;
    Ok(line
        .trim()
        .strip_prefix("File: ")
        .unwrap_or_else(|| line.trim()))
}

pub fn claims(root: &Path, patch: &str) -> Result<Vec<(String, String)>> {
    changes(patch)?
        .into_iter()
        .map(|change| {
            let (operation, path, writable) = match change {
                Change::Add(path, _) => ("write", path, true),
                Change::Update(path, _) => ("write", path, false),
                Change::Delete(path) => ("delete", path, false),
            };
            Ok((
                operation.into(),
                resolved(root, &path, writable)?.display().to_string(),
            ))
        })
        .collect()
}

pub fn apply(root: &Path, patch: &str, cancel: &CancellationToken) -> Result<String> {
    let changes = changes(patch)?;
    let mut summary = Vec::new();
    for change in changes {
        check_cancel(cancel)?;
        match change {
            Change::Add(path, content) => {
                let target = resolved(root, &path, true)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let target = resolved(root, &path, true)?;
                check_cancel(cancel)?;
                fs::write(target, content)?;
                summary.push(format!("A {path}"));
            }
            Change::Delete(path) => {
                let target = resolved(root, &path, false)?;
                check_cancel(cancel)?;
                fs::remove_file(target)?;
                summary.push(format!("D {path}"));
            }
            Change::Update(path, chunks) => {
                let target = resolved(root, &path, false)?;
                let original = fs::read_to_string(&target)?;
                let mut lines: Vec<String> = original.split('\n').map(str::to_owned).collect();
                if original.ends_with('\n') {
                    lines.pop();
                }
                let mut cursor = 0;
                for (old, new) in chunks {
                    let position = if old.is_empty() {
                        cursor.min(lines.len())
                    } else {
                        (cursor..=lines.len().saturating_sub(old.len()))
                            .find(|&start| {
                                lines.get(start..start + old.len()) == Some(old.as_slice())
                            })
                            .ok_or_else(|| SandboxError("failed to find expected lines".into()))?
                    };
                    cursor = position + new.len();
                    lines.splice(position..position + old.len(), new);
                }
                let mut updated = lines.join("\n");
                if !updated.is_empty() && original.ends_with('\n') {
                    updated.push('\n');
                }
                check_cancel(cancel)?;
                fs::write(target, updated)?;
                summary.push(format!("M {path}"));
            }
        }
    }
    Ok(format!(
        "Success. Updated the following files:\n{}",
        summary.join("\n")
    ))
}
