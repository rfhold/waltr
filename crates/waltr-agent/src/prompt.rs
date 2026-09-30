//! System prompt construction helpers.

use std::path::{Path, PathBuf};

/// Reusable base system prompt for Waltr agents.
pub const BASE_SYSTEM_PROMPT: &str = "You are Waltr Agent, a careful coding and operations assistant. Follow the user's instructions, use available tools when they are helpful, and report results clearly. Treat tool errors as recoverable information and continue when possible.";

/// Default local instruction filename.
pub const AGENTS_INSTRUCTION_FILE: &str = "AGENTS.md";

/// Builder for deterministic compatibility system prompt construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPromptBuilder {
    fragments: Vec<String>,
}

impl SystemPromptBuilder {
    /// Create an empty system prompt builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fragments: Vec::new(),
        }
    }

    /// Append the reusable base system prompt.
    #[must_use]
    pub fn with_base_prompt(self) -> Self {
        self.with_fragment(BASE_SYSTEM_PROMPT)
    }

    /// Append one prompt fragment. Empty fragments are ignored.
    #[must_use]
    pub fn with_fragment(mut self, fragment: impl Into<String>) -> Self {
        let fragment = fragment.into();
        let fragment = fragment.trim();
        if !fragment.is_empty() {
            self.fragments.push(fragment.to_owned());
        }
        self
    }

    /// Append deterministic date context.
    #[must_use]
    pub fn with_date(self, date: impl Into<String>) -> Self {
        self.with_fragment(format!("Current date: {}.", date.into()))
    }

    /// Build the joined system prompt.
    #[must_use]
    pub fn build(self) -> Option<String> {
        if self.fragments.is_empty() {
            None
        } else {
            Some(self.fragments.join("\n\n"))
        }
    }
}

/// Loaded instruction prompt fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsInstruction {
    /// Source path for this instruction file.
    pub path: PathBuf,
    /// Rendered prompt fragment.
    pub content: String,
}

/// `AGENTS.md` system prompt configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsPromptConfig {
    /// Root directory to check for `AGENTS.md`.
    pub root: PathBuf,
    /// Instruction filename to load.
    pub filename: String,
}

impl AgentsPromptConfig {
    /// Create an `AGENTS.md` prompt configuration rooted at one directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            filename: AGENTS_INSTRUCTION_FILE.to_owned(),
        }
    }

    /// Override the instruction filename.
    #[must_use]
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = filename.into();
        self
    }
}

/// Filesystem system prompt configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemPromptConfig {
    /// Root directory filesystem context is scoped to.
    pub root: PathBuf,
    /// Whether to include a bounded filesystem snapshot.
    pub include_snapshot: bool,
    /// Maximum snapshot entries.
    pub max_snapshot_entries: usize,
    /// Maximum snapshot traversal depth.
    pub max_snapshot_depth: usize,
    /// Maximum snapshot bytes.
    pub max_snapshot_bytes: usize,
}

impl FilesystemPromptConfig {
    /// Create filesystem prompt configuration without a snapshot.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            include_snapshot: false,
            max_snapshot_entries: 200,
            max_snapshot_depth: 4,
            max_snapshot_bytes: 16 * 1024,
        }
    }

    /// Include a bounded filesystem snapshot.
    #[must_use]
    pub const fn with_snapshot(mut self) -> Self {
        self.include_snapshot = true;
        self
    }

    /// Override maximum snapshot entries.
    #[must_use]
    pub const fn with_max_snapshot_entries(mut self, entries: usize) -> Self {
        self.max_snapshot_entries = entries;
        self
    }

    /// Override maximum snapshot traversal depth.
    #[must_use]
    pub const fn with_max_snapshot_depth(mut self, depth: usize) -> Self {
        self.max_snapshot_depth = depth;
        self
    }

    /// Override maximum snapshot bytes.
    #[must_use]
    pub const fn with_max_snapshot_bytes(mut self, bytes: usize) -> Self {
        self.max_snapshot_bytes = bytes;
        self
    }

    /// Render this configuration as a system prompt fragment.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        // Compatibility renderer for callers that need a prompt fragment directly.
        let root = display_path(&self.root);
        let prompt = format!(
            "Filesystem tools are available when exposed in the tool list. Paths are scoped to this filesystem root: {root}. Prefer relative paths under the root unless the user gives an absolute path inside the root."
        );

        prompt
    }
}

#[cfg(feature = "shell")]
/// Shell system prompt configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPromptConfig {
    /// Root directory shell context is scoped to.
    pub root: PathBuf,
    /// Default working directory for shell commands.
    pub default_workdir: PathBuf,
    /// Operating system identifier.
    pub os: String,
    /// Shell executable path.
    pub shell_program: String,
    /// Full shell invocation used by the tool.
    pub shell_invocation: String,
}

#[cfg(feature = "shell")]
impl ShellPromptConfig {
    /// Create shell prompt configuration rooted at one directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            default_workdir: root.clone(),
            root,
            os: std::env::consts::OS.to_owned(),
            shell_program: "/bin/bash".to_owned(),
            shell_invocation: "/bin/bash -lc".to_owned(),
        }
    }

    /// Override the default working directory.
    #[must_use]
    pub fn with_default_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.default_workdir = workdir.into();
        self
    }

    /// Override the operating system label.
    #[must_use]
    pub fn with_os(mut self, os: impl Into<String>) -> Self {
        self.os = os.into();
        self
    }

    /// Override the shell program.
    #[must_use]
    pub fn with_shell_program(mut self, shell_program: impl Into<String>) -> Self {
        self.shell_program = shell_program.into();
        self
    }

    /// Override the shell invocation.
    #[must_use]
    pub fn with_shell_invocation(mut self, shell_invocation: impl Into<String>) -> Self {
        self.shell_invocation = shell_invocation.into();
        self
    }

    /// Render this configuration as a system prompt fragment.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        // Compatibility renderer for callers that need a prompt fragment directly.
        format!(
            "The shell tool executes commands on {} using {}. The shell program is {}. The default working directory is {}. Relative workdirs and path-like command arguments resolve under root {}. Use Bash/POSIX shell syntax.",
            self.os,
            self.shell_invocation,
            self.shell_program,
            display_path(&self.default_workdir),
            display_path(&self.root),
        )
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

pub(crate) fn render_agents_instruction(path: &Path, content: &str) -> String {
    format!("Instructions from: {}\n{}", path.display(), content.trim())
}

#[cfg(test)]
mod tests {
    use super::{BASE_SYSTEM_PROMPT, FilesystemPromptConfig, SystemPromptBuilder};

    #[test]
    fn builder_joins_non_empty_fragments() {
        let prompt = SystemPromptBuilder::new()
            .with_fragment(" alpha ")
            .with_fragment(" ")
            .with_date("2026-06-02")
            .build();

        assert_eq!(
            prompt.as_deref(),
            Some("alpha\n\nCurrent date: 2026-06-02.")
        );
    }

    #[test]
    fn builder_can_include_base_prompt() {
        let prompt = SystemPromptBuilder::new().with_base_prompt().build();

        assert_eq!(prompt.as_deref(), Some(BASE_SYSTEM_PROMPT));
    }

    #[test]
    fn filesystem_prompt_does_not_read_workspace() {
        let prompt = FilesystemPromptConfig::new("/workspace")
            .with_snapshot()
            .system_prompt();
        assert!(prompt.contains("/workspace"));
        assert!(!prompt.contains("Current filesystem snapshot:"));
    }

    #[test]
    fn agents_prompt_renders_instruction_source_and_content() {
        let path = std::path::Path::new("/workspace/AGENTS.md");
        let prompt = super::render_agents_instruction(path, "Follow local instructions.\n");
        assert_eq!(
            prompt,
            "Instructions from: /workspace/AGENTS.md\nFollow local instructions."
        );
    }

    #[cfg(feature = "shell")]
    #[test]
    fn shell_prompt_includes_os_shell_and_root() {
        let prompt = super::ShellPromptConfig::new("/tmp")
            .with_os("linux")
            .system_prompt();

        assert!(prompt.contains("linux"));
        assert!(prompt.contains("/bin/bash -lc"));
        assert!(prompt.contains("Bash/POSIX"));
    }
}
