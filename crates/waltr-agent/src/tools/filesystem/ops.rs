//! Filesystem operations.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use crate::{AgentToolContext, Result};

use super::{
    DeleteInput, EditInput, FilesystemConfig, GlobInput, GrepInput, GrepMatch, GrepSearch,
    ReadInput, WriteInput, filesystem_error, include_relative_path, model_path, truncate_line,
};

const DEFAULT_READ_LIMIT: usize = 2_000;
const MAX_LINE_LENGTH: usize = 2_000;

impl FilesystemConfig {
    /// Read a file or directory using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the root or cannot be read.
    pub fn read(&self, input: &ReadInput) -> Result<String> {
        let path = self.scoped_existing_path(&model_path(&input.file_path))?;
        Self::read_path(input, &path)
    }

    pub(super) fn read_unscoped(
        &self,
        input: &ReadInput,
        context: &mut AgentToolContext,
    ) -> Result<String> {
        // TODO(agent-runtime): Move AGENTS/skills injections to read-hook tool-result middleware.
        let path = self.resolved_existing_path(&model_path(&input.file_path))?;
        let mut output = Self::read_path(input, &path)?;
        if self.agents_read_context && path.is_file() {
            self.append_agents_read_context(&path, context, &mut output)?;
        }
        if self.skills_context {
            self.append_skills_context(&path, false, context, &mut output)?;
        }
        Ok(output)
    }

    fn read_path(input: &ReadInput, path: &Path) -> Result<String> {
        if path.is_dir() {
            return Self::read_directory(path);
        }

        let content = fs::read_to_string(path)?;
        let lines = content.lines().collect::<Vec<_>>();
        let total = lines.len();
        let offset = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let start = offset.saturating_sub(1).min(total);
        let end = start.saturating_add(limit).min(total);
        let mut output = format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n",
            path.display()
        );

        for (index, line) in lines[start..end].iter().enumerate() {
            let line_number = start + index + 1;
            writeln!(
                output,
                "{line_number}: {}",
                truncate_line(line, MAX_LINE_LENGTH)
            )
            .expect("write string output");
        }

        output.push('\n');
        if end == total {
            writeln!(output, "(End of file - total {total} lines)").expect("write string output");
        } else {
            writeln!(
                output,
                "(Showing lines {offset}-{end} of {total}. Use offset={} to continue.)\n",
                end + 1
            )
            .expect("write string output");
        }
        output.push_str("</content>");
        Ok(output)
    }

    /// Return files matching a simple glob pattern using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem root or any child directory cannot be read.
    pub fn glob(&self, input: &GlobInput) -> Result<String> {
        let base = self.scoped_existing_path(&model_path(input.path.as_deref().unwrap_or(".")))?;
        Self::glob_path(input, &base)
    }

    pub(super) fn glob_unscoped(
        &self,
        input: &GlobInput,
        context: &mut AgentToolContext,
    ) -> Result<String> {
        let base =
            self.resolved_existing_path(&model_path(input.path.as_deref().unwrap_or(".")))?;
        let mut output = Self::glob_path(input, &base)?;
        if self.skills_context {
            self.append_skills_context(&base, true, context, &mut output)?;
        }
        Ok(output)
    }

    fn glob_path(input: &GlobInput, base: &Path) -> Result<String> {
        let mut matches = Vec::new();
        Self::collect_glob_matches(base, base, &input.pattern, &mut matches)?;
        matches.sort();
        if matches.is_empty() {
            return Ok("No files found".to_owned());
        }
        Ok(matches
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Search files under the filesystem root using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem root or any child directory cannot be read.
    pub fn grep(&self, input: &GrepInput) -> Result<String> {
        let base = self.scoped_existing_path(&model_path(input.path.as_deref().unwrap_or(".")))?;
        Self::grep_path(input, &base)
    }

    pub(super) fn grep_unscoped(
        &self,
        input: &GrepInput,
        context: &mut AgentToolContext,
    ) -> Result<String> {
        let base =
            self.resolved_existing_path(&model_path(input.path.as_deref().unwrap_or(".")))?;
        let mut output = Self::grep_path(input, &base)?;
        if self.skills_context {
            self.append_skills_context(&base, true, context, &mut output)?;
        }
        Ok(output)
    }

    fn grep_path(input: &GrepInput, base: &Path) -> Result<String> {
        let mut matches = Vec::new();
        let search = GrepSearch {
            base,
            pattern: &input.pattern,
            include: input.include.as_deref(),
        };
        Self::collect_grep_matches(base, &search, &mut matches)?;
        matches.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.line_number.cmp(&right.line_number))
        });
        if matches.is_empty() {
            return Ok("No files found".to_owned());
        }

        let mut output = format!("Found {} matches", matches.len());
        let mut current_path = None::<PathBuf>;
        for grep_match in matches {
            if current_path.as_ref() != Some(&grep_match.path) {
                current_path = Some(grep_match.path.clone());
                write!(output, "\n{}:", grep_match.path.display()).expect("write string output");
            }
            write!(
                output,
                "\n  Line {}: {}",
                grep_match.line_number,
                truncate_line(&grep_match.line, MAX_LINE_LENGTH)
            )
            .expect("write string output");
        }
        Ok(output)
    }

    /// Replace exact file content using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the root, the file cannot be read or written, or the
    /// old string is not found exactly once.
    pub fn edit(&self, input: &EditInput) -> Result<String> {
        let path = self.scoped_existing_path(&model_path(&input.file_path))?;
        Self::edit_path(input, &path)
    }

    pub(super) fn edit_unscoped(&self, input: &EditInput) -> Result<String> {
        let path = self.resolved_existing_path(&model_path(&input.file_path))?;
        Self::edit_path(input, &path)
    }

    fn edit_path(input: &EditInput, path: &Path) -> Result<String> {
        if input.old_string == input.new_string {
            return Err(filesystem_error(
                "oldString and newString must be different",
            ));
        }

        let content = fs::read_to_string(path)?;
        let count = content.matches(&input.old_string).count();

        if count == 0 || (count > 1 && !input.replace_all.unwrap_or(false)) {
            return Err(filesystem_error(format!(
                "expected exactly one match for edit, found {count}"
            )));
        }

        let updated = if input.replace_all.unwrap_or(false) {
            content.replace(&input.old_string, &input.new_string)
        } else {
            content.replacen(&input.old_string, &input.new_string, 1)
        };
        fs::write(path, updated)?;
        Ok("Edit applied successfully.".to_owned())
    }

    /// Write a file using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the root or the file cannot be written.
    pub fn write(&self, input: &WriteInput) -> Result<String> {
        let path = self.scoped_writable_path(&model_path(&input.file_path))?;
        Self::write_path(input, &path)
    }

    pub(super) fn write_unscoped(&self, input: &WriteInput) -> Result<String> {
        let path = self.resolved_writable_path(&model_path(&input.file_path))?;
        Self::write_path(input, &path)
    }

    fn write_path(input: &WriteInput, path: &Path) -> Result<String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &input.content)?;
        Ok("Wrote file successfully.".to_owned())
    }

    /// Delete a file using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the root or the file cannot be deleted.
    pub fn delete(&self, input: &DeleteInput) -> Result<String> {
        fs::remove_file(self.scoped_existing_path(&model_path(&input.file_path))?)?;
        Ok("Deleted file successfully.".to_owned())
    }

    pub(super) fn delete_unscoped(&self, input: &DeleteInput) -> Result<String> {
        fs::remove_file(self.resolved_existing_path(&model_path(&input.file_path))?)?;
        Ok("Deleted file successfully.".to_owned())
    }

    fn read_directory(path: &Path) -> Result<String> {
        let mut entries = fs::read_dir(path)?
            .map(|entry| {
                let entry = entry?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    name.push('/');
                }
                Ok(name)
            })
            .collect::<std::result::Result<Vec<_>, std::io::Error>>()?;
        entries.sort();
        Ok(format!(
            "<path>{}</path>\n<type>directory</type>\n<entries>\n{}\n({} entries)\n</entries>",
            path.display(),
            entries.join("\n"),
            entries.len()
        ))
    }

    fn collect_glob_matches(
        directory: &Path,
        base: &Path,
        pattern: &str,
        matches: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::collect_glob_matches(&path, base, pattern, matches)?;
                continue;
            }

            let relative = path
                .strip_prefix(base)
                .map_err(|error| filesystem_error(error.to_string()))?;
            if glob_matches(pattern, relative) {
                matches.push(path);
            }
        }

        Ok(())
    }

    fn collect_grep_matches(
        directory: &Path,
        search: &GrepSearch<'_>,
        matches: &mut Vec<GrepMatch>,
    ) -> Result<()> {
        if directory.is_file() {
            Self::collect_file_grep_matches(directory, search, matches)?;
            return Ok(());
        }

        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::collect_grep_matches(&path, search, matches)?;
                continue;
            }
            Self::collect_file_grep_matches(&path, search, matches)?;
        }

        Ok(())
    }

    fn collect_file_grep_matches(
        path: &Path,
        search: &GrepSearch<'_>,
        matches: &mut Vec<GrepMatch>,
    ) -> Result<()> {
        let Ok(content) = fs::read_to_string(path) else {
            return Ok(());
        };
        let relative = include_relative_path(path, search.base)?;
        if search
            .include
            .is_some_and(|include| !glob_matches(include, relative))
        {
            return Ok(());
        }
        for (index, line) in content.lines().enumerate() {
            if line.contains(search.pattern) {
                matches.push(GrepMatch {
                    path: path.to_path_buf(),
                    line_number: index + 1,
                    line: line.to_owned(),
                });
            }
        }
        Ok(())
    }
}

pub(super) fn glob_matches(pattern: &str, path: &Path) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.to_string_lossy().replace('\\', "/");
    glob_segments_match(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

fn glob_segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, None) => true,
        (Some((&"**", remaining_pattern)), _) => {
            glob_segments_match(remaining_pattern, path)
                || path
                    .split_first()
                    .is_some_and(|(_, remaining_path)| glob_segments_match(pattern, remaining_path))
        }
        (Some((pattern_segment, remaining_pattern)), Some((path_segment, remaining_path))) => {
            glob_segment_match(pattern_segment, path_segment)
                && glob_segments_match(remaining_pattern, remaining_path)
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn glob_segment_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    let mut remaining = value;
    let mut parts = pattern.split('*').peekable();
    let Some(first) = parts.next() else {
        return value.is_empty();
    };

    if !remaining.starts_with(first) {
        return false;
    }
    remaining = &remaining[first.len()..];

    while let Some(part) = parts.next() {
        if part.is_empty() {
            continue;
        }
        let Some(index) = remaining.find(part) else {
            return false;
        };
        remaining = &remaining[index + part.len()..];
        if parts.peek().is_none() && !pattern.ends_with('*') && !remaining.is_empty() {
            return false;
        }
    }

    pattern.ends_with('*') || remaining.is_empty()
}
