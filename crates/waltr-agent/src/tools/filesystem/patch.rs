//! Filesystem patch parsing and application.

use crate::Result;

use super::{ApplyPatchInput, DeleteInput, FilesystemConfig, WriteInput, filesystem_error};

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
                    let mut content = String::new();
                    while index < lines.len() {
                        let patch_line = lines[index];
                        if patch_line.starts_with("*** ") {
                            break;
                        }
                        index += 1;
                        if patch_line.starts_with("@@") {
                            continue;
                        }
                        if patch_line.starts_with('-') {
                            continue;
                        }
                        let text = patch_line
                            .strip_prefix('+')
                            .or_else(|| patch_line.strip_prefix(' '))
                            .ok_or_else(|| filesystem_error("invalid update patch line"))?;
                        content.push_str(text);
                        content.push('\n');
                    }
                    if unscoped {
                        self.write_unscoped(&WriteInput::new(content, path))?;
                    } else {
                        self.write(&WriteInput::new(content, path))?;
                    }
                    summary.push(format!("M {path}"));
                }
                "*** Update File" => {
                    let path = patch_path(&lines, &mut index)?;
                    let mut content = String::new();
                    while index < lines.len() {
                        let patch_line = lines[index];
                        if patch_line.starts_with("*** ") {
                            break;
                        }
                        index += 1;
                        if patch_line.starts_with("@@") || patch_line.starts_with('-') {
                            continue;
                        }
                        let text = patch_line
                            .strip_prefix('+')
                            .or_else(|| patch_line.strip_prefix(' '))
                            .ok_or_else(|| filesystem_error("invalid update patch line"))?;
                        content.push_str(text);
                        content.push('\n');
                    }
                    if unscoped {
                        self.write_unscoped(&WriteInput::new(content, &path))?;
                    } else {
                        self.write(&WriteInput::new(content, &path))?;
                    }
                    summary.push(format!("M {path}"));
                }
                _ => return Err(filesystem_error(format!("unsupported patch line: {line}"))),
            }
        }

        Err(filesystem_error("patch missing *** End Patch"))
    }
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
