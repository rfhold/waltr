//! Filesystem patch parsing and application.

use std::{fs, path::Path};

use crate::Result;

use super::{
    ApplyPatchInput, DeleteInput, FilesystemConfig, WriteInput, filesystem_error, model_path,
};

#[derive(Debug, Default)]
struct UpdateChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}

impl FilesystemConfig {
    /// Apply a stripped-down patch envelope using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the patch envelope is invalid, the path escapes the root, or a file
    /// operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn apply_patch(&self, input: &ApplyPatchInput) -> Result<String> {
        self.apply_patch_impl(input, false)
    }

    pub(super) fn apply_patch_unscoped(&self, input: &ApplyPatchInput) -> Result<String> {
        self.apply_patch_impl(input, true)
    }

    #[allow(clippy::too_many_lines)]
    fn apply_patch_impl(&self, input: &ApplyPatchInput, unscoped: bool) -> Result<String> {
        if input.patch_text.trim().is_empty() {
            return Err(filesystem_error("patchText cannot be empty"));
        }

        let mut summary = Vec::new();
        let lines = input.patch_text.lines().collect::<Vec<_>>();
        if lines.first().copied() != Some("*** Begin Patch") {
            return Err(filesystem_error("patch must start with *** Begin Patch"));
        }

        let mut index = 1;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            match line {
                "*** End Patch" => return Ok(patch_success(&summary)),
                _ if line.starts_with("*** Add File: ") => {
                    let path = line.trim_start_matches("*** Add File: ");
                    let mut content = String::new();
                    while index < lines.len() {
                        let patch_line = lines[index];
                        if patch_line.starts_with("*** ") {
                            break;
                        }
                        index += 1;
                        let Some(text) = patch_line.strip_prefix('+') else {
                            return Err(filesystem_error("add file lines must start with +"));
                        };
                        content.push_str(text);
                        content.push('\n');
                    }
                    if unscoped {
                        self.write_unscoped(&WriteInput::new(content, path))?;
                    } else {
                        self.write(&WriteInput::new(content, path))?;
                    }
                    summary.push(format!("A {path}"));
                }
                "*** Add File" => {
                    let path = patch_path(&lines, &mut index)?;
                    let mut content = String::new();
                    while index < lines.len() {
                        let patch_line = lines[index];
                        if patch_line.starts_with("*** ") {
                            break;
                        }
                        index += 1;
                        let Some(text) = patch_line.strip_prefix('+') else {
                            return Err(filesystem_error("add file lines must start with +"));
                        };
                        content.push_str(text);
                        content.push('\n');
                    }
                    if unscoped {
                        self.write_unscoped(&WriteInput::new(content, &path))?;
                    } else {
                        self.write(&WriteInput::new(content, &path))?;
                    }
                    summary.push(format!("A {path}"));
                }
                _ if line.starts_with("*** Delete File: ") => {
                    let path = line.trim_start_matches("*** Delete File: ");
                    if unscoped {
                        self.delete_unscoped(&DeleteInput::new(path))?;
                    } else {
                        self.delete(&DeleteInput::new(path))?;
                    }
                    summary.push(format!("D {path}"));
                }
                "*** Delete File" => {
                    let path = patch_path(&lines, &mut index)?;
                    if unscoped {
                        self.delete_unscoped(&DeleteInput::new(&path))?;
                    } else {
                        self.delete(&DeleteInput::new(&path))?;
                    }
                    summary.push(format!("D {path}"));
                }
                _ if line.starts_with("*** Update File: ") => {
                    let path = line.trim_start_matches("*** Update File: ");
                    let chunks = parse_update_chunks(&lines, &mut index)?;
                    let content = self.apply_update_chunks(path, &chunks, unscoped)?;
                    self.write_patch_update(path, content, unscoped)?;
                    summary.push(format!("M {path}"));
                }
                "*** Update File" => {
                    let path = patch_path(&lines, &mut index)?;
                    let chunks = parse_update_chunks(&lines, &mut index)?;
                    let content = self.apply_update_chunks(&path, &chunks, unscoped)?;
                    self.write_patch_update(&path, content, unscoped)?;
                    summary.push(format!("M {path}"));
                }
                _ => return Err(filesystem_error(format!("unsupported patch line: {line}"))),
            }
        }

        Err(filesystem_error("patch missing *** End Patch"))
    }

    fn apply_update_chunks(
        &self,
        path: &str,
        chunks: &[UpdateChunk],
        unscoped: bool,
    ) -> Result<String> {
        let path = model_path(path);
        let resolved = if unscoped {
            self.resolved_existing_path(&path)?
        } else {
            self.scoped_existing_path(&path)?
        };
        let content = fs::read_to_string(&resolved)?;
        apply_update_chunks_to_content(&resolved, &content, chunks)
    }

    fn write_patch_update(&self, path: &str, content: String, unscoped: bool) -> Result<()> {
        if unscoped {
            self.write_unscoped(&WriteInput::new(content, path))?;
        } else {
            self.write(&WriteInput::new(content, path))?;
        }
        Ok(())
    }
}

fn parse_update_chunks(lines: &[&str], index: &mut usize) -> Result<Vec<UpdateChunk>> {
    let mut chunks = Vec::new();
    let mut current = UpdateChunk::default();

    while *index < lines.len() {
        let line = lines[*index];
        if line.starts_with("*** ") {
            break;
        }
        *index += 1;

        if line.starts_with("@@") {
            if !current.old_lines.is_empty() || !current.new_lines.is_empty() {
                chunks.push(current);
                current = UpdateChunk::default();
            }
            let context = line.trim_start_matches("@@").trim();
            if !context.is_empty() {
                current.old_lines.push(context.to_owned());
                current.new_lines.push(context.to_owned());
            }
            continue;
        }

        if let Some(text) = line.strip_prefix(' ') {
            current.old_lines.push(text.to_owned());
            current.new_lines.push(text.to_owned());
        } else if let Some(text) = line.strip_prefix('-') {
            current.old_lines.push(text.to_owned());
        } else if let Some(text) = line.strip_prefix('+') {
            current.new_lines.push(text.to_owned());
        } else {
            return Err(filesystem_error("invalid update patch line"));
        }
    }

    if !current.old_lines.is_empty() || !current.new_lines.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        return Err(filesystem_error("update patch has no changes"));
    }

    Ok(chunks)
}

fn apply_update_chunks_to_content(
    path: &Path,
    content: &str,
    chunks: &[UpdateChunk],
) -> Result<String> {
    let mut lines = split_patch_lines(content);
    let had_trailing_newline = content.ends_with('\n');
    let mut search_start = 0;

    for chunk in chunks {
        if chunk.old_lines.is_empty() {
            let insert_at = search_start.min(lines.len());
            lines.splice(insert_at..insert_at, chunk.new_lines.clone());
            search_start = insert_at + chunk.new_lines.len();
            continue;
        }

        let Some(start) = seek_lines(&lines, &chunk.old_lines, search_start) else {
            return Err(filesystem_error(format!(
                "failed to find expected lines in {}",
                path.display()
            )));
        };
        lines.splice(
            start..start + chunk.old_lines.len(),
            chunk.new_lines.clone(),
        );
        search_start = start + chunk.new_lines.len();
    }

    let mut updated = lines.join("\n");
    if !updated.is_empty() && had_trailing_newline {
        updated.push('\n');
    }
    Ok(updated)
}

fn split_patch_lines(content: &str) -> Vec<String> {
    let mut lines = content
        .split('\n')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn seek_lines(lines: &[String], pattern: &[String], start: usize) -> Option<usize> {
    if pattern.is_empty() || pattern.len() > lines.len() {
        return None;
    }
    (start..=lines.len() - pattern.len())
        .find(|&index| lines[index..index + pattern.len()] == *pattern)
}

fn patch_success(summary: &[String]) -> String {
    if summary.is_empty() {
        return "Success. Updated the following files:".to_owned();
    }

    format!(
        "Success. Updated the following files:\n{}",
        summary.join("\n")
    )
}

pub(super) fn patch_path(lines: &[&str], index: &mut usize) -> Result<String> {
    while *index < lines.len() && lines[*index].trim().is_empty() {
        *index += 1;
    }

    let Some(line) = lines.get(*index) else {
        return Err(filesystem_error("patch operation missing file path"));
    };
    if line.starts_with("*** ") {
        return Err(filesystem_error("patch operation missing file path"));
    }
    *index += 1;

    Ok(line
        .trim()
        .strip_prefix("File: ")
        .unwrap_or_else(|| line.trim())
        .to_owned())
}

pub(super) fn skip_patch_body(lines: &[&str], index: &mut usize) {
    while *index < lines.len() && !lines[*index].starts_with("*** ") {
        *index += 1;
    }
}
