//! Filesystem tool configuration.

mod config;
mod context;
mod formatting;
mod lsp_hints;
mod ops;
mod patch;
mod path;
mod permissions;
mod profile;
mod schema;

use std::{
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    AgentTool, AgentToolContext, LlmError, ModelRef, PermissionClaim, PermissionOperation,
    PermissionResource, ProviderId, Result, ToolCall, ToolDefinition,
    prompt::{AGENTS_INSTRUCTION_FILE, canonicalize_existing, render_agents_instruction},
    tools::filesystem_skills::{render_skills_reminder, reveal_skills_root},
};

const DEFAULT_READ_LIMIT: usize = 2_000;
const MAX_LINE_LENGTH: usize = 2_000;

/// Filesystem tool configuration for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemConfig {
    /// Root directory filesystem tools are scoped to.
    pub root: PathBuf,
    /// Tool profile to expose to the model.
    pub profile: FilesystemToolProfile,
    /// Whether file reads append relevant `AGENTS.md` context.
    pub agents_read_context: bool,
    /// Whether filesystem operations reveal newly encountered Agent Skills catalogs.
    pub skills_context: bool,
}

impl FilesystemConfig {
    /// Create filesystem tool configuration with automatic profile selection.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            profile: FilesystemToolProfile::Auto,
            agents_read_context: false,
            skills_context: false,
        }
    }

    /// Override the filesystem tool profile.
    #[must_use]
    pub const fn with_profile(mut self, profile: FilesystemToolProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Append relevant `AGENTS.md` content when the agent read tool reads files.
    #[must_use]
    pub const fn with_agents_read_context(mut self) -> Self {
        self.agents_read_context = true;
        self
    }

    /// Reveal newly encountered `.agents/skills` catalogs from filesystem operations.
    #[must_use]
    pub const fn with_skills_context(mut self) -> Self {
        self.skills_context = true;
        self
    }

    /// Resolve the configured profile for a specific model.
    #[must_use]
    pub fn resolve_profile(&self, model: &ModelRef) -> ResolvedFilesystemToolProfile {
        self.profile.resolve(model)
    }

    /// Return the filesystem tools that should be exposed for a model.
    #[must_use]
    pub fn exposed_tools(&self, model: &ModelRef) -> FilesystemToolSet {
        FilesystemToolSet::for_profile(self.resolve_profile(model))
    }

    /// Convert exposed filesystem capabilities into generic agent tools.
    #[must_use]
    pub fn into_agent_tools(self, model: &ModelRef) -> Vec<AgentTool> {
        let tools = self.exposed_tools(model).all();
        tools
            .into_iter()
            .map(|kind| self.clone().into_agent_tool(kind))
            .collect()
    }

    fn into_agent_tool(self, kind: FilesystemToolKind) -> AgentTool {
        let mut definition = kind.definition();
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        write!(
            definition.description,
            " Paths must be relative to the filesystem root or absolute paths inside the filesystem root. Filesystem root: {}. Use path '.' or omit optional path fields to search the root. Never use '/', a parent directory, or any path outside this root.",
            root.display()
        )
        .expect("write tool description");
        let claims_config = self.clone();
        AgentTool::with_context(definition, move |call, context| {
            self.call_agent_tool(kind, call, context)
        })
        .with_permission_claims(move |call| claims_config.permission_claims(kind, call))
    }

    fn call_agent_tool(
        &self,
        kind: FilesystemToolKind,
        call: &ToolCall,
        context: &mut AgentToolContext,
    ) -> Result<String> {
        match kind {
            FilesystemToolKind::Read => self.read_unscoped(&tool_input(call)?, context),
            FilesystemToolKind::Glob => self.glob_unscoped(&tool_input(call)?, context),
            FilesystemToolKind::Grep => self.grep_unscoped(&tool_input(call)?, context),
            FilesystemToolKind::Edit => self.edit_unscoped(&tool_input(call)?),
            FilesystemToolKind::Write => self.write_unscoped(&tool_input(call)?),
            FilesystemToolKind::Delete => self.delete_unscoped(&tool_input(call)?),
            FilesystemToolKind::ApplyPatch => self.apply_patch_unscoped(&tool_input(call)?),
        }
    }

    fn permission_claims(
        &self,
        kind: FilesystemToolKind,
        call: &ToolCall,
    ) -> Result<Vec<PermissionClaim>> {
        match kind {
            FilesystemToolKind::Read => {
                let input = tool_input::<ReadInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    true,
                )?])
            }
            FilesystemToolKind::Glob => {
                let input = tool_input::<GlobInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(input.path.as_deref().unwrap_or(".")),
                    true,
                )?])
            }
            FilesystemToolKind::Grep => {
                let input = tool_input::<GrepInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(input.path.as_deref().unwrap_or(".")),
                    true,
                )?])
            }
            FilesystemToolKind::Edit => {
                let input = tool_input::<EditInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    true,
                )?])
            }
            FilesystemToolKind::Write => {
                let input = tool_input::<WriteInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    false,
                )?])
            }
            FilesystemToolKind::Delete => {
                let input = tool_input::<DeleteInput>(call)?;
                Ok(vec![self.path_claim(
                    kind.permission_operation(),
                    &model_path(&input.file_path),
                    true,
                )?])
            }
            FilesystemToolKind::ApplyPatch => {
                let input = tool_input::<ApplyPatchInput>(call)?;
                self.apply_patch_claims(&input)
            }
        }
    }

    fn path_claim(
        &self,
        operation: PermissionOperation,
        path: &Path,
        existing: bool,
    ) -> Result<PermissionClaim> {
        let resolved = if existing {
            self.resolved_existing_path(path)?
        } else {
            self.resolved_writable_path(path)?
        };
        Ok(PermissionClaim::new(
            operation,
            PermissionResource::FilesystemPath(resolved.clone()),
            format!("{operation:?} filesystem path {}", resolved.display()),
        ))
    }

    /// Read a file or directory using opencode-compatible input and output.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the root or cannot be read.
    pub fn read(&self, input: &ReadInput) -> Result<String> {
        let path = self.scoped_existing_path(&model_path(&input.file_path))?;
        Self::read_path(input, &path)
    }

    fn read_unscoped(&self, input: &ReadInput, context: &mut AgentToolContext) -> Result<String> {
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

    fn glob_unscoped(&self, input: &GlobInput, context: &mut AgentToolContext) -> Result<String> {
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

    fn grep_unscoped(&self, input: &GrepInput, context: &mut AgentToolContext) -> Result<String> {
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

    fn edit_unscoped(&self, input: &EditInput) -> Result<String> {
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

    fn write_unscoped(&self, input: &WriteInput) -> Result<String> {
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

    fn delete_unscoped(&self, input: &DeleteInput) -> Result<String> {
        fs::remove_file(self.resolved_existing_path(&model_path(&input.file_path))?)?;
        Ok("Deleted file successfully.".to_owned())
    }

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

    fn apply_patch_unscoped(&self, input: &ApplyPatchInput) -> Result<String> {
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

    fn apply_patch_claims(&self, input: &ApplyPatchInput) -> Result<Vec<PermissionClaim>> {
        if input.patch_text.trim().is_empty() {
            return Err(filesystem_error("patchText cannot be empty"));
        }

        let lines = input.patch_text.lines().collect::<Vec<_>>();
        if lines.first().copied() != Some("*** Begin Patch") {
            return Err(filesystem_error("patch must start with *** Begin Patch"));
        }

        let mut claims = Vec::new();
        let mut index = 1;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            match line {
                "*** End Patch" => return Ok(claims),
                _ if line.starts_with("*** Add File: ") => {
                    let path = line.trim_start_matches("*** Add File: ");
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                "*** Add File" => {
                    let path = patch_path(&lines, &mut index)?;
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(&path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                _ if line.starts_with("*** Delete File: ") => {
                    let path = line.trim_start_matches("*** Delete File: ");
                    claims.push(self.path_claim(
                        PermissionOperation::Delete,
                        &model_path(path),
                        true,
                    )?);
                }
                "*** Delete File" => {
                    let path = patch_path(&lines, &mut index)?;
                    claims.push(self.path_claim(
                        PermissionOperation::Delete,
                        &model_path(&path),
                        true,
                    )?);
                }
                _ if line.starts_with("*** Update File: ") => {
                    let path = line.trim_start_matches("*** Update File: ");
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                "*** Update File" => {
                    let path = patch_path(&lines, &mut index)?;
                    claims.push(self.path_claim(
                        PermissionOperation::Write,
                        &model_path(&path),
                        false,
                    )?);
                    skip_patch_body(&lines, &mut index);
                }
                _ => return Err(filesystem_error(format!("unsupported patch line: {line}"))),
            }
        }

        Err(filesystem_error("patch missing *** End Patch"))
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

    fn append_agents_read_context(
        &self,
        target: &Path,
        context: &mut AgentToolContext,
        output: &mut String,
    ) -> Result<()> {
        let root = self.root.canonicalize()?;
        if !target.starts_with(&root)
            || target
                .file_name()
                .is_some_and(|name| name == AGENTS_INSTRUCTION_FILE)
        {
            return Ok(());
        }

        let Some(mut directory) = target.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        let mut reminders = Vec::new();
        loop {
            if !directory.starts_with(&root) {
                break;
            }
            let instruction_path = directory.join(AGENTS_INSTRUCTION_FILE);
            if instruction_path.is_file() {
                let instruction_path = canonicalize_existing(&instruction_path);
                if instruction_path != target
                    && !context.agents_system_paths.contains(&instruction_path)
                    && !context.agents_loaded_paths.contains(&instruction_path)
                {
                    let instruction_text = fs::read_to_string(&instruction_path)?;
                    reminders.push(render_agents_instruction(
                        &instruction_path,
                        &instruction_text,
                    ));
                    context.agents_loaded_paths.insert(instruction_path);
                }
            }
            if directory == root || !directory.pop() {
                break;
            }
        }

        if !reminders.is_empty() {
            output.push_str("\n\n<system-reminder>\n");
            output.push_str(&reminders.join("\n\n"));
            output.push_str("\n</system-reminder>");
        }
        Ok(())
    }

    fn append_skills_context(
        &self,
        target: &Path,
        walk_children: bool,
        context: &mut AgentToolContext,
        output: &mut String,
    ) -> Result<()> {
        if !context.skills_enabled {
            return Ok(());
        }
        let root = self.root.canonicalize()?;
        if !target.starts_with(&root) {
            return Ok(());
        }

        let mut reminders = Vec::new();
        let start = if target.is_dir() {
            target.to_path_buf()
        } else {
            target.parent().map_or_else(PathBuf::new, Path::to_path_buf)
        };
        Self::collect_skills_context_for_ancestors(&root, &start, context, &mut reminders);
        if walk_children && target.is_dir() {
            Self::collect_skills_context_for_tree(target, context, &mut reminders)?;
        }

        if !reminders.is_empty() {
            output.push_str("\n\n<system-reminder>\n");
            output.push_str(&reminders.join("\n\n"));
            output.push_str("\n</system-reminder>");
        }
        Ok(())
    }

    fn collect_skills_context_for_ancestors(
        root: &Path,
        start: &Path,
        context: &mut AgentToolContext,
        reminders: &mut Vec<String>,
    ) {
        let mut directory = start.to_path_buf();
        loop {
            if !directory.starts_with(root) {
                break;
            }
            Self::push_skills_reminder(&directory, context, reminders);
            if directory == root || !directory.pop() {
                break;
            }
        }
    }

    fn collect_skills_context_for_tree(
        directory: &Path,
        context: &mut AgentToolContext,
        reminders: &mut Vec<String>,
    ) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|name| name == ".agents") {
                if let Some(skills) = reveal_skills_root(context, directory)
                    && let Some(reminder) = render_skills_reminder(&skills, directory)
                {
                    reminders.push(reminder);
                }
                continue;
            }
            Self::collect_skills_context_for_tree(&path, context, reminders)?;
        }
        Ok(())
    }

    fn push_skills_reminder(
        root: &Path,
        context: &mut AgentToolContext,
        reminders: &mut Vec<String>,
    ) {
        if let Some(skills) = reveal_skills_root(context, root)
            && let Some(reminder) = render_skills_reminder(&skills, root)
        {
            reminders.push(reminder);
        }
    }

    fn scoped_existing_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        let path = self.resolved_existing_path(path)?;
        if !path.starts_with(&root) {
            return Err(root_escape_error(&path, &root));
        }
        Ok(path)
    }

    fn scoped_writable_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        let full = self.resolved_writable_path(path)?;
        if !full.starts_with(&root) {
            return Err(root_escape_error(&full, &root));
        }

        Ok(full)
    }

    fn resolved_existing_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        if path.is_absolute() {
            Ok(path.canonicalize()?)
        } else {
            Ok(root.join(path).canonicalize()?)
        }
    }

    fn resolved_writable_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.root.canonicalize()?;
        Ok(if path.is_absolute() {
            normalize_path(path)
        } else {
            normalize_path(&root.join(path))
        })
    }
}

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

struct GrepSearch<'a> {
    base: &'a Path,
    pattern: &'a str,
    include: Option<&'a str>,
}

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
            ResolvedFilesystemToolProfile::StructuredPatch => vec![
                FilesystemToolKind::Edit,
                FilesystemToolKind::Write,
                FilesystemToolKind::Delete,
                FilesystemToolKind::ApplyPatch,
            ],
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

    fn definition(self) -> ToolDefinition {
        ToolDefinition::new(self.name(), self.description(), self.input_schema())
    }

    const fn permission_operation(self) -> PermissionOperation {
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

fn tool_input<T>(call: &ToolCall) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(call.arguments.clone()).map_err(Into::into)
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

fn glob_matches(pattern: &str, path: &Path) -> bool {
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

fn model_path(path: &str) -> PathBuf {
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

fn include_relative_path<'a>(path: &'a Path, base: &'a Path) -> Result<&'a Path> {
    if base.is_file() {
        return path
            .file_name()
            .map(Path::new)
            .ok_or_else(|| filesystem_error("path has no file name"));
    }
    path.strip_prefix(base)
        .map_err(|error| filesystem_error(error.to_string()))
}

fn truncate_line(line: &str, max_length: usize) -> String {
    if line.len() <= max_length {
        return line.to_owned();
    }

    format!("{}...", &line[..max_length])
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

fn patch_path(lines: &[&str], index: &mut usize) -> Result<String> {
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

fn skip_patch_body(lines: &[&str], index: &mut usize) {
    while *index < lines.len() && !lines[*index].starts_with("*** ") {
        *index += 1;
    }
}

fn root_escape_error(path: &Path, root: &Path) -> LlmError {
    filesystem_error(format!(
        "path escapes filesystem root: {} is outside {}. Use a path relative to the filesystem root, or an absolute path that starts with the filesystem root.",
        path.display(),
        root.display()
    ))
}

fn filesystem_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "filesystem",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{ModelApi, ModelRef, ProviderId};

    use super::{
        ApplyPatchInput, DeleteInput, EditInput, FilesystemConfig, FilesystemToolKind,
        FilesystemToolProfile, GlobInput, GrepInput, ReadInput, ResolvedFilesystemToolProfile,
        WriteInput,
    };

    #[test]
    fn auto_profile_uses_structured_patch_for_newer_gpt_models() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);
        let config = FilesystemConfig::new(".");

        assert_eq!(
            config.resolve_profile(&model),
            ResolvedFilesystemToolProfile::StructuredPatch
        );
        assert_eq!(
            config.exposed_tools(&model).edit_tools,
            vec![
                FilesystemToolKind::Edit,
                FilesystemToolKind::Write,
                FilesystemToolKind::Delete,
                FilesystemToolKind::ApplyPatch,
            ]
        );
    }

    #[test]
    fn auto_profile_uses_exact_edit_for_gpt_4_and_anthropic() {
        let gpt4 = ModelRef::new(ProviderId::OpenAi, "gpt-4.1", ModelApi::OpenAiResponses);
        let claude = ModelRef::new(
            ProviderId::Anthropic,
            "claude-haiku-4-5",
            ModelApi::AnthropicMessages,
        );
        let config = FilesystemConfig::new(".");

        assert_eq!(
            config.resolve_profile(&gpt4),
            ResolvedFilesystemToolProfile::ExactEdit
        );
        assert_eq!(
            config.resolve_profile(&claude),
            ResolvedFilesystemToolProfile::ExactEdit
        );
    }

    #[test]
    fn explicit_profile_overrides_auto_detection() {
        let model = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);
        let config = FilesystemConfig::new(".").with_profile(FilesystemToolProfile::WholeFileWrite);

        assert_eq!(
            config.resolve_profile(&model),
            ResolvedFilesystemToolProfile::WholeFileWrite
        );
    }

    #[test]
    fn read_returns_opencode_file_output() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "hello\nfrom waltr\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .read(&ReadInput::new("notes.txt"))
                .expect("read file"),
            format!(
                "<path>{}</path>\n<type>file</type>\n<content>\n1: hello\n2: from waltr\n\n(End of file - total 2 lines)\n</content>",
                temp.path().join("notes.txt").display()
            )
        );
    }

    #[test]
    fn read_accepts_absolute_paths_inside_root() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "hello\n");
        let config = FilesystemConfig::new(temp.path());

        assert!(
            config
                .read(&ReadInput::new(
                    temp.path().join("notes.txt").display().to_string()
                ))
                .expect("read absolute path")
                .contains("1: hello")
        );
    }

    #[test]
    fn read_accepts_pasted_path_tags() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "hello\n");
        let config = FilesystemConfig::new(temp.path());
        let file_path = format!("<path>{}</path>", temp.path().join("notes.txt").display());

        assert!(
            config
                .read(&ReadInput::new(file_path))
                .expect("read pasted path tag")
                .contains("1: hello")
        );
    }

    #[test]
    fn root_escape_error_reports_rejected_path_and_root() {
        let temp = TempRoot::new();
        let config = FilesystemConfig::new(temp.path());
        let error = config
            .glob(&GlobInput::new("**/*").with_path("/"))
            .expect_err("root path should escape configured root")
            .to_string();

        assert!(error.contains("path escapes filesystem root"));
        assert!(error.contains('/'));
        assert!(error.contains(&temp.path().display().to_string()));
        assert!(error.contains("Use a path relative to the filesystem root"));
    }

    #[test]
    fn read_returns_opencode_directory_output() {
        let temp = TempRoot::new();
        temp.write("src/lib.rs", "");
        temp.write("README.md", "");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config.read(&ReadInput::new(".")).expect("read directory"),
            format!(
                "<path>{}</path>\n<type>directory</type>\n<entries>\nREADME.md\nsrc/\n(2 entries)\n</entries>",
                temp.path().display()
            )
        );
    }

    #[test]
    fn glob_returns_opencode_absolute_path_output() {
        let temp = TempRoot::new();
        temp.write("src/lib.rs", "");
        temp.write("src/main.rs", "");
        temp.write("README.md", "");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config.glob(&GlobInput::new("**/*.rs")).expect("glob files"),
            format!(
                "{}\n{}",
                temp.path().join("src/lib.rs").display(),
                temp.path().join("src/main.rs").display()
            )
        );
    }

    #[test]
    fn grep_returns_opencode_match_output() {
        let temp = TempRoot::new();
        temp.write("a.txt", "alpha\nbeta\n");
        temp.write("nested/b.txt", "gamma\nalpha beta\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config.grep(&GrepInput::new("alpha")).expect("grep files"),
            format!(
                "Found 2 matches\n{}:\n  Line 1: alpha\n{}:\n  Line 2: alpha beta",
                temp.path().join("a.txt").display(),
                temp.path().join("nested/b.txt").display()
            )
        );
    }

    #[test]
    fn grep_supports_file_path_and_include() {
        let temp = TempRoot::new();
        temp.write("a.txt", "alpha\n");
        temp.write("a.md", "alpha\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .grep(
                    &GrepInput::new("alpha")
                        .with_path("a.txt")
                        .with_include("*.txt")
                )
                .expect("grep file"),
            format!(
                "Found 1 matches\n{}:\n  Line 1: alpha",
                temp.path().join("a.txt").display()
            )
        );
    }

    #[test]
    fn edit_replaces_exactly_one_match() {
        let temp = TempRoot::new();
        temp.write("notes.txt", "before\ntarget\nafter\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .edit(&EditInput::new("notes.txt", "target", "replacement"))
                .expect("edit file"),
            "Edit applied successfully."
        );

        assert_eq!(temp.read("notes.txt"), "before\nreplacement\nafter\n");
    }

    #[test]
    fn write_creates_parent_directories_and_file() {
        let temp = TempRoot::new();
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .write(&WriteInput::new("created by write\n", "nested/new.txt"))
                .expect("write file"),
            "Wrote file successfully."
        );

        assert_eq!(temp.read("nested/new.txt"), "created by write\n");
    }

    #[test]
    fn delete_removes_files() {
        let temp = TempRoot::new();
        temp.write("deleted.txt", "remove me\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .delete(&DeleteInput::new(
                    temp.path().join("deleted.txt").display().to_string(),
                ))
                .expect("delete file"),
            "Deleted file successfully."
        );

        assert!(!temp.path().join("deleted.txt").exists());
    }

    #[test]
    fn apply_patch_adds_files() {
        let temp = TempRoot::new();
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Add File: added.txt\n+hello\n+from patch\n*** End Patch",
                ))
                .expect("apply add patch"),
            "Success. Updated the following files:\nA added.txt"
        );

        assert_eq!(temp.read("added.txt"), "hello\nfrom patch\n");
    }

    #[test]
    fn apply_patch_updates_files() {
        let temp = TempRoot::new();
        temp.write("updated.txt", "old\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Update File: updated.txt\n@@\n-old\n+new\n*** End Patch",
                ))
                .expect("apply update patch"),
            "Success. Updated the following files:\nM updated.txt"
        );

        assert_eq!(temp.read("updated.txt"), "new\n");
    }

    #[test]
    fn apply_patch_deletes_files() {
        let temp = TempRoot::new();
        temp.write("deleted.txt", "remove me\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Delete File: deleted.txt\n*** End Patch",
                ))
                .expect("apply delete patch"),
            "Success. Updated the following files:\nD deleted.txt"
        );

        assert!(!temp.path().join("deleted.txt").exists());
    }

    #[test]
    fn apply_patch_supports_multiple_operations() {
        let temp = TempRoot::new();
        temp.write("updated.txt", "old\n");
        temp.write("deleted.txt", "remove me\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Update File: updated.txt\n@@\n-old\n+new\n*** Delete File: deleted.txt\n*** Add File: added.txt\n+created\n*** End Patch",
                ))
                .expect("apply multi patch"),
            "Success. Updated the following files:\nM updated.txt\nD deleted.txt\nA added.txt"
        );

        assert_eq!(temp.read("updated.txt"), "new\n");
        assert!(!temp.path().join("deleted.txt").exists());
        assert_eq!(temp.read("added.txt"), "created\n");
    }

    #[test]
    fn apply_patch_accepts_path_on_next_line() {
        let temp = TempRoot::new();
        temp.write("updated.txt", "old\n");
        let config = FilesystemConfig::new(temp.path());

        assert_eq!(
            config
                .apply_patch(&ApplyPatchInput::new(
                    "*** Begin Patch\n*** Update File\nupdated.txt\n@@\n-old\n+new\n*** End Patch",
                ))
                .expect("apply patch with path on next line"),
            "Success. Updated the following files:\nM updated.txt"
        );

        assert_eq!(temp.read("updated.txt"), "new\n");
    }

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "waltr-agent-filesystem-test-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, path: &str, content: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, content).expect("write temp file");
        }

        fn read(&self, path: &str) -> String {
            fs::read_to_string(self.path.join(path)).expect("read temp file")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.path));
        }
    }
}
