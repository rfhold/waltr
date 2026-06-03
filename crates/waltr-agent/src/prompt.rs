//! System prompt construction helpers.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Reusable base system prompt for Waltr agents.
pub const BASE_SYSTEM_PROMPT: &str = "You are Waltr Agent, a careful coding and operations assistant. Follow the user's instructions, use available tools when they are helpful, and report results clearly. Treat tool errors as recoverable information and continue when possible.";

/// Default local instruction filename.
pub const AGENTS_INSTRUCTION_FILE: &str = "AGENTS.md";

/// Builder for deterministic system prompt construction.
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

    /// Load configured instruction files.
    #[must_use]
    pub fn instructions(&self) -> Vec<AgentsInstruction> {
        let path = self.root.join(&self.filename);
        let Ok(content) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let path = canonicalize_existing(&path);
        vec![AgentsInstruction {
            content: render_agents_instruction(&path, &content),
            path,
        }]
    }

    /// Render this configuration as a system prompt fragment.
    #[must_use]
    pub fn system_prompt(&self) -> Option<String> {
        let instructions = self.instructions();
        if instructions.is_empty() {
            None
        } else {
            Some(
                instructions
                    .into_iter()
                    .map(|instruction| instruction.content)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        }
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
        let root = display_path(&self.root);
        let mut prompt = format!(
            "Filesystem tools are available when exposed in the tool list. Paths are scoped to this filesystem root: {root}. Prefer relative paths under the root unless the user gives an absolute path inside the root."
        );

        if self.include_snapshot {
            prompt.push_str("\n\nCurrent filesystem snapshot:");
            match filesystem_snapshot(self) {
                Some(snapshot) => {
                    prompt.push('\n');
                    prompt.push_str(&snapshot);
                }
                None => prompt.push_str("\n- <empty>"),
            }
        }

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
    canonicalize_existing(path).display().to_string()
}

pub(crate) fn canonicalize_existing(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn render_agents_instruction(path: &Path, content: &str) -> String {
    format!("Instructions from: {}\n{}", path.display(), content.trim())
}

fn filesystem_snapshot(config: &FilesystemPromptConfig) -> Option<String> {
    let root = config
        .root
        .canonicalize()
        .unwrap_or_else(|_| config.root.clone());
    let mut collector = SnapshotCollector::new(root, config);
    collector.collect(collector.root.clone(), 0);
    collector.entries.sort();

    let mut output = String::new();
    let mut bytes = 0usize;
    for entry in collector.entries {
        let line = format!("- {entry}\n");
        if bytes + line.len() > config.max_snapshot_bytes {
            collector.limited = true;
            break;
        }
        bytes += line.len();
        output.push_str(&line);
    }

    if collector.limited {
        output.push_str("- <snapshot truncated>\n");
    }

    if output.is_empty() {
        None
    } else {
        Some(output.trim_end().to_owned())
    }
}

struct SnapshotCollector<'a> {
    root: PathBuf,
    config: &'a FilesystemPromptConfig,
    entries: Vec<String>,
    limited: bool,
}

impl<'a> SnapshotCollector<'a> {
    const fn new(root: PathBuf, config: &'a FilesystemPromptConfig) -> Self {
        Self {
            root,
            config,
            entries: Vec::new(),
            limited: false,
        }
    }

    fn collect(&mut self, directory: PathBuf, depth: usize) {
        if self.limited || depth >= self.config.max_snapshot_depth {
            return;
        }

        let Ok(read_dir) = fs::read_dir(directory) else {
            return;
        };
        let mut children = read_dir
            .filter_map(std::result::Result::ok)
            .collect::<Vec<_>>();
        children.sort_by_key(std::fs::DirEntry::path);

        for child in children {
            if self.entries.len() >= self.config.max_snapshot_entries {
                self.limited = true;
                return;
            }

            let path = child.path();
            let Ok(file_type) = child.file_type() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }

            let mut entry = relative.to_string_lossy().replace('\\', "/");
            if file_type.is_dir() {
                entry.push('/');
            }
            self.entries.push(entry);

            if file_type.is_dir() {
                self.collect(path, depth + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        AgentsPromptConfig, BASE_SYSTEM_PROMPT, FilesystemPromptConfig, SystemPromptBuilder,
    };

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
    fn filesystem_snapshot_is_deterministic_and_bounded() {
        let root =
            std::env::temp_dir().join(format!("waltr-agent-prompt-test-{}", std::process::id()));
        drop(fs::remove_dir_all(&root));
        fs::create_dir_all(root.join("src")).expect("create src dir");
        fs::write(root.join("Cargo.toml"), "").expect("write Cargo.toml");
        fs::write(root.join("src/lib.rs"), "").expect("write lib.rs");

        let prompt = FilesystemPromptConfig::new(&root)
            .with_snapshot()
            .system_prompt();

        assert!(prompt.contains("Current filesystem snapshot:"));
        assert!(prompt.contains("- Cargo.toml"));
        assert!(prompt.contains("- src/"));
        assert!(prompt.contains("- src/lib.rs"));

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn agents_prompt_renders_instruction_source_and_content() {
        let root = std::env::temp_dir().join(format!(
            "waltr-agent-agents-prompt-test-{}",
            std::process::id()
        ));
        drop(fs::remove_dir_all(&root));
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("AGENTS.md"), "Follow local instructions.\n").expect("write AGENTS.md");

        let prompt = AgentsPromptConfig::new(&root)
            .system_prompt()
            .expect("agents prompt");

        assert!(prompt.contains(&format!(
            "Instructions from: {}",
            root.join("AGENTS.md").display()
        )));
        assert!(prompt.contains("Follow local instructions."));

        fs::remove_dir_all(&root).expect("remove temp dir");
    }

    #[test]
    fn agents_prompt_ignores_missing_file() {
        let root = std::env::temp_dir().join(format!(
            "waltr-agent-missing-agents-prompt-test-{}",
            std::process::id()
        ));
        drop(fs::remove_dir_all(&root));
        fs::create_dir_all(&root).expect("create root");

        assert_eq!(AgentsPromptConfig::new(&root).system_prompt(), None);

        fs::remove_dir_all(&root).expect("remove temp dir");
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
