//! Shell tool configuration and implementation.

mod config;
mod execute;
mod formatting;
mod parser;
mod path;
mod permissions;

use std::{
    fmt::Write as _,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use tree_sitter::{Node, Parser};

use crate::{
    AgentTool, LlmError, PermissionClaim, PermissionOperation, PermissionPattern,
    PermissionResource, PermissionResourcePattern, Result, ToolCall, ToolDefinition,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 20 * 1024;
const SHELL_TOOL_NAME: &str = "shell";

/// Shell tool configuration for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    /// Root directory used for relative workdir and path resolution.
    pub root: PathBuf,
    /// Default working directory for shell commands.
    pub default_workdir: PathBuf,
    /// Default command timeout.
    pub default_timeout: Duration,
    /// Maximum output bytes returned to the model.
    pub max_output_bytes: usize,
}

impl ShellConfig {
    /// Create shell tool configuration rooted at one directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            default_workdir: root.clone(),
            root,
            default_timeout: DEFAULT_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Override the default working directory.
    #[must_use]
    pub fn with_default_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.default_workdir = workdir.into();
        self
    }

    /// Override the default timeout.
    #[must_use]
    pub const fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Override the maximum output bytes returned to the model.
    #[must_use]
    pub const fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Convert this configuration into the `shell` agent tool.
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        let mut definition = ToolDefinition::new(
            SHELL_TOOL_NAME,
            "Execute a Bash/POSIX shell command on the host after permission checks. Commands run with /bin/bash -lc. Provide a short description explaining why the command is needed.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute."
                    },
                    "description": {
                        "type": "string",
                        "description": "Short reason for running the command."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional timeout in seconds."
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Optional working directory. Relative paths resolve under the configured root."
                    }
                },
                "required": ["command", "description"],
                "additionalProperties": false
            }),
        );
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        write!(
            definition.description,
            " Relative workdirs and path-like command arguments resolve from root: {}.",
            root.display()
        )
        .expect("write tool description");

        let claims_config = self.clone();
        AgentTool::new(definition, move |call| self.call_agent_tool(call))
            .with_permission_claims(move |call| claims_config.permission_claims(call))
    }

    /// Scan a shell command and return parsed commands, claims, and reusable patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if the Bash parser cannot be initialized or the requested workdir cannot be
    /// resolved.
    pub fn scan(&self, input: &ShellInput) -> Result<ShellScan> {
        let workdir = self.resolved_workdir(input.workdir.as_deref());
        ShellCommandParser::new()?.scan(&input.command, &self.root_path(), &workdir)
    }

    fn call_agent_tool(&self, call: &ToolCall) -> Result<String> {
        self.execute(&tool_input(call)?)
    }

    fn permission_claims(&self, call: &ToolCall) -> Result<Vec<PermissionClaim>> {
        let input = tool_input::<ShellInput>(call)?;
        Ok(self.scan(&input)?.claims)
    }

    /// Execute a shell command using the configured root and limits.
    ///
    /// # Errors
    ///
    /// Returns an error if command input is invalid or the process cannot be spawned.
    pub fn execute(&self, input: &ShellInput) -> Result<String> {
        let workdir = self.resolved_workdir(input.workdir.as_deref());
        let timeout = input
            .timeout
            .map_or(self.default_timeout, Duration::from_secs);
        let mut child = Command::new("/bin/bash")
            .arg("-lc")
            .arg(&input.command)
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let start = Instant::now();
        loop {
            if child.try_wait()?.is_some() {
                let output = child.wait_with_output()?;
                return Ok(format_process_output(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                    self.max_output_bytes,
                    false,
                ));
            }

            if start.elapsed() >= timeout {
                drop(child.kill());
                let output = child.wait_with_output()?;
                return Ok(format_process_output(
                    output.status.code(),
                    &output.stdout,
                    &output.stderr,
                    self.max_output_bytes,
                    true,
                ));
            }

            thread::sleep(Duration::from_millis(10));
        }
    }

    fn root_path(&self) -> PathBuf {
        self.root
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(&self.root))
    }

    fn resolved_workdir(&self, requested: Option<&str>) -> PathBuf {
        let root = self.root_path();
        let workdir = requested.map_or_else(|| self.default_workdir.clone(), model_path);
        if workdir.is_absolute() {
            normalize_path(&workdir)
        } else {
            normalize_path(&root.join(workdir))
        }
    }
}

/// Input for the `shell` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellInput {
    /// Bash command to execute.
    pub command: String,
    /// Short model-supplied reason for the command.
    pub description: String,
    /// Optional timeout in seconds.
    pub timeout: Option<u64>,
    /// Optional working directory.
    pub workdir: Option<String>,
}

impl ShellInput {
    /// Create shell input.
    pub fn new(command: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            description: description.into(),
            timeout: None,
            workdir: None,
        }
    }

    /// Set a timeout in seconds.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: u64) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set a working directory.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<String>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }
}

/// Parsed shell command scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellScan {
    /// Parsed shell commands.
    pub commands: Vec<ParsedShellCommand>,
    /// Permission claims required by the command.
    pub claims: Vec<PermissionClaim>,
    /// Suggested reusable permission patterns for allow-always decisions.
    pub always_patterns: Vec<PermissionPattern>,
}

/// One parsed shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedShellCommand {
    /// Source command used for permission matching.
    pub source: String,
    /// Tokenized command words.
    pub tokens: Vec<String>,
    /// Whether this is a pure cwd command.
    pub is_cwd_command: bool,
}

struct ShellCommandParser {
    parser: Parser,
}

impl ShellCommandParser {
    fn new() -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .map_err(|error| shell_error(format!("failed to initialize bash parser: {error}")))?;
        Ok(Self { parser })
    }

    fn scan(mut self, command: &str, root: &Path, workdir: &Path) -> Result<ShellScan> {
        let tree = self
            .parser
            .parse(command, None)
            .ok_or_else(|| shell_error("failed to parse shell command"))?;
        let bytes = command.as_bytes();
        let mut parsed = Vec::new();
        collect_command_nodes(tree.root_node(), bytes, &mut parsed);

        dedupe_commands(&mut parsed);

        let mut claims = Vec::new();
        let mut patterns = Vec::new();
        for parsed_command in &parsed {
            if parsed_command.is_cwd_command {
                claims.extend(cwd_claims(parsed_command, root, workdir));
            } else {
                claims.push(PermissionClaim::new(
                    PermissionOperation::Execute,
                    PermissionResource::ShellCommand(parsed_command.source.clone()),
                    format!("execute shell command {}", parsed_command.source),
                ));
                if let Some(prefix) = arity_prefix(&parsed_command.tokens) {
                    patterns.push(PermissionPattern {
                        operation: Some(PermissionOperation::Execute),
                        resource: PermissionResourcePattern::ShellCommandPrefix(prefix),
                    });
                }
            }
            claims.extend(path_claims(parsed_command, root, workdir));
        }
        dedupe_claims(&mut claims);
        dedupe_patterns(&mut patterns);

        Ok(ShellScan {
            commands: parsed,
            claims,
            always_patterns: patterns,
        })
    }
}

fn collect_command_nodes(node: Node<'_>, bytes: &[u8], commands: &mut Vec<ParsedShellCommand>) {
    if node.kind() == "command"
        && let Some(parsed) = parsed_command(node, bytes)
    {
        commands.push(parsed);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_command_nodes(child, bytes, commands);
    }
}

fn parsed_command(node: Node<'_>, bytes: &[u8]) -> Option<ParsedShellCommand> {
    let source_node = redirected_parent(node).unwrap_or(node);
    let source = source_node.utf8_text(bytes).ok()?.trim().to_owned();
    if source.is_empty() {
        return None;
    }
    let command_source = node.utf8_text(bytes).ok()?.trim();
    let tokens = shell_words(command_source);
    if tokens.is_empty() {
        return None;
    }
    let is_cwd_command = matches!(tokens[0].as_str(), "cd" | "chdir" | "pushd" | "popd");
    Some(ParsedShellCommand {
        source,
        tokens,
        is_cwd_command,
    })
}

fn redirected_parent(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    matches!(parent.kind(), "redirected_statement" | "file_redirect").then_some(parent)
}

fn shell_words(source: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = source.chars().peekable();
    let mut quote = None::<char>;

    while let Some(character) = chars.next() {
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else if character == '\\' && active_quote == '"' {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            } else {
                current.push(character);
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' | '\n' | '\r' => push_current_word(&mut words, &mut current),
            ';' => {
                push_current_word(&mut words, &mut current);
                break;
            }
            '&' | '|' if chars.peek() == Some(&character) => {
                push_current_word(&mut words, &mut current);
                break;
            }
            '>' | '<' => {
                push_current_word(&mut words, &mut current);
                if chars.peek() == Some(&character) {
                    chars.next();
                    words.push(format!("{character}{character}"));
                } else {
                    words.push(character.to_string());
                }
            }
            _ => current.push(character),
        }
    }
    push_current_word(&mut words, &mut current);
    words
}

fn push_current_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

fn cwd_claims(command: &ParsedShellCommand, root: &Path, workdir: &Path) -> Vec<PermissionClaim> {
    command
        .tokens
        .get(1)
        .filter(|path| !is_dynamic_path(path))
        .map(|path| resolve_model_path(path, root, workdir))
        .map(|path| {
            vec![PermissionClaim::new(
                PermissionOperation::Read,
                PermissionResource::WorkingDirectory(path.clone()),
                format!("access working directory {}", path.display()),
            )]
        })
        .unwrap_or_default()
}

fn path_claims(command: &ParsedShellCommand, root: &Path, workdir: &Path) -> Vec<PermissionClaim> {
    let mut claims = Vec::new();
    let Some(name) = command.tokens.first().map(String::as_str) else {
        return claims;
    };

    let operation = match name {
        "cat" | "less" | "more" | "head" | "tail" | "grep" | "rg" | "ls" | "find" => {
            Some(PermissionOperation::Read)
        }
        "touch" | "mkdir" | "cp" | "mv" => Some(PermissionOperation::Write),
        "rm" | "rmdir" => Some(PermissionOperation::Delete),
        _ => None,
    };

    if let Some(operation) = operation {
        for token in command
            .tokens
            .iter()
            .skip(1)
            .filter(|token| is_path_argument(token))
        {
            let path = resolve_model_path(token, root, workdir);
            claims.push(PermissionClaim::new(
                operation,
                PermissionResource::FilesystemPath(path.clone()),
                format!("{operation:?} filesystem path {}", path.display()),
            ));
        }
    }

    let source_tokens = shell_words(&command.source);
    for token in redirect_targets(&source_tokens) {
        let path = resolve_model_path(token, root, workdir);
        claims.push(PermissionClaim::new(
            PermissionOperation::Write,
            PermissionResource::FilesystemPath(path.clone()),
            format!("Write filesystem path {}", path.display()),
        ));
    }

    claims
}

fn redirect_targets(tokens: &[String]) -> impl Iterator<Item = &str> {
    tokens
        .windows(2)
        .filter_map(|window| matches!(window[0].as_str(), ">" | ">>").then_some(window[1].as_str()))
}

fn is_path_argument(token: &str) -> bool {
    !token.starts_with('-')
        && !matches!(token, ">" | ">>" | "<" | "<<")
        && !is_dynamic_path(token)
        && (token.contains('/') || token.starts_with('.') || token.starts_with('~'))
}

fn is_dynamic_path(token: &str) -> bool {
    token.contains('$') || token.contains('`') || token.contains("<(") || token.contains(">(")
}

fn resolve_model_path(path: &str, root: &Path, workdir: &Path) -> PathBuf {
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

fn arity_prefix(tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    let arity = match first.as_str() {
        "npm" if tokens.get(1).is_some_and(|token| token == "run") => 3,
        "git" | "cargo" | "docker" | "kubectl" | "npm" => 2,
        _ => 1,
    };
    let take = tokens.len().min(arity);
    Some(format!("{} ", tokens[..take].join(" ")))
}

fn dedupe_commands(commands: &mut Vec<ParsedShellCommand>) {
    let mut deduped = Vec::new();
    for command in commands.drain(..) {
        if !deduped.iter().any(|existing: &ParsedShellCommand| {
            existing.source == command.source && existing.tokens == command.tokens
        }) {
            deduped.push(command);
        }
    }
    *commands = deduped;
}

fn dedupe_claims(claims: &mut Vec<PermissionClaim>) {
    let mut deduped = Vec::new();
    for claim in claims.drain(..) {
        if !deduped.contains(&claim) {
            deduped.push(claim);
        }
    }
    *claims = deduped;
}

fn dedupe_patterns(patterns: &mut Vec<PermissionPattern>) {
    let mut deduped = Vec::new();
    for pattern in patterns.drain(..) {
        if !deduped.contains(&pattern) {
            deduped.push(pattern);
        }
    }
    *patterns = deduped;
}

fn format_process_output(
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    max_output_bytes: usize,
    timed_out: bool,
) -> String {
    let mut output = String::new();
    if timed_out {
        output.push_str("Timed out: true\n");
    }
    writeln!(
        output,
        "Exit code: {}\n",
        exit_code.map_or_else(|| "signal".to_owned(), |code| code.to_string())
    )
    .expect("write process output");
    output.push_str(&String::from_utf8_lossy(stdout));
    output.push_str(&String::from_utf8_lossy(stderr));
    truncate_output(output, max_output_bytes)
}

fn truncate_output(mut output: String, max_output_bytes: usize) -> String {
    if output.len() <= max_output_bytes {
        return output;
    }
    output.truncate(max_output_bytes);
    output.push_str("\n[output truncated]");
    output
}

fn tool_input<T>(call: &ToolCall) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(call.arguments.clone()).map_err(Into::into)
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

fn shell_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "shell",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, path::Path};

    use crate::{
        PermissionOperation, PermissionResource, PermissionResourcePattern,
        tools::shell::{ShellConfig, ShellInput},
    };

    #[test]
    fn parser_extracts_single_command_claim() {
        let root = temp_root("single");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("echo hello", "test"))
            .expect("scan");

        assert_eq!(scan.commands.len(), 1);
        assert!(scan.claims.iter().any(|claim| matches!(
            &claim.resource,
            PermissionResource::ShellCommand(command) if command == "echo hello"
        )));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn parser_extracts_multiple_command_claims() {
        let root = temp_root("multi");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("echo one && echo two", "test"))
            .expect("scan");

        assert!(
            scan.commands
                .iter()
                .any(|command| command.source == "echo one")
        );
        assert!(
            scan.commands
                .iter()
                .any(|command| command.source == "echo two")
        );
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn parser_detects_nested_command() {
        let root = temp_root("nested");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("echo $(cat /etc/passwd)", "test"))
            .expect("scan");

        assert!(
            scan.commands
                .iter()
                .any(|command| command.source == "cat /etc/passwd")
        );
        assert!(scan.claims.iter().any(|claim| matches!(
            &claim.resource,
            PermissionResource::FilesystemPath(path) if path == &PathBuf::from("/etc/passwd")
        )));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn redirected_command_adds_write_claim() {
        let root = temp_root("redirect");
        let path = root.join("output.txt");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("echo hi > output.txt", "test"))
            .expect("scan");

        assert!(
            scan.commands
                .iter()
                .any(|command| command.source == "echo hi > output.txt")
        );
        assert!(scan.claims.iter().any(|claim| {
            claim.operation == PermissionOperation::Write
                && matches!(&claim.resource, PermissionResource::FilesystemPath(claim_path) if claim_path == &path)
        }));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn dynamic_paths_are_not_inferred() {
        let root = temp_root("dynamic");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("cat \"$FILE\"", "test"))
            .expect("scan");

        assert!(
            !scan
                .claims
                .iter()
                .any(|claim| matches!(claim.resource, PermissionResource::FilesystemPath(_)))
        );
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn cwd_command_has_no_shell_execute_claim() {
        let root = temp_root("cwd");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("cd ..", "test"))
            .expect("scan");

        assert!(
            !scan
                .claims
                .iter()
                .any(|claim| matches!(claim.resource, PermissionResource::ShellCommand(_)))
        );
        assert!(
            scan.claims
                .iter()
                .any(|claim| matches!(claim.resource, PermissionResource::WorkingDirectory(_)))
        );
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn git_status_has_reusable_prefix() {
        let root = temp_root("prefix");
        let scan = ShellConfig::new(&root)
            .scan(&ShellInput::new("git status --short", "test"))
            .expect("scan");

        assert!(scan.always_patterns.iter().any(|pattern| matches!(
            &pattern.resource,
            PermissionResourcePattern::ShellCommandPrefix(prefix) if prefix == "git status "
        )));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn execution_returns_stdout() {
        let root = temp_root("exec");
        let output = ShellConfig::new(&root)
            .execute(&ShellInput::new("printf waltr-shell-ok", "test"))
            .expect("execute");

        assert!(output.contains("Exit code: 0"));
        assert!(output.contains("waltr-shell-ok"));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn execution_uses_workdir() {
        let root = temp_root("workdir");
        fs::create_dir_all(root.join("subdir")).expect("subdir");
        let output = ShellConfig::new(&root)
            .execute(&ShellInput::new("pwd", "test").with_workdir("subdir"))
            .expect("execute");

        assert!(output.contains(&root.join("subdir").display().to_string()));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn execution_times_out() {
        let root = temp_root("timeout");
        let output = ShellConfig::new(&root)
            .execute(&ShellInput::new("sleep 5", "test").with_timeout(1))
            .expect("execute");

        assert!(output.contains("Timed out: true"));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn execution_truncates_output() {
        let root = temp_root("truncate");
        let output = ShellConfig::new(&root)
            .with_max_output_bytes(20)
            .execute(&ShellInput::new("printf 1234567890123456789012345", "test"))
            .expect("execute");

        assert!(output.contains("[output truncated]"));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn execution_reports_nonzero_as_output() {
        let root = temp_root("nonzero");
        let output = ShellConfig::new(&root)
            .execute(&ShellInput::new("exit 7", "test"))
            .expect("execute");

        assert!(output.contains("Exit code: 7"));
        drop(fs::remove_dir_all(root));
    }

    #[test]
    fn scan_extracts_delete_permission_claim() {
        let root = temp_root("tool-claims");
        let input = ShellInput::new("rm /tmp/waltr-shell-delete-test", "test delete claim");

        let scan = ShellConfig::new(&root).scan(&input).expect("scan");
        assert!(scan.claims.iter().any(|claim| {
            claim.operation == PermissionOperation::Delete
                && matches!(&claim.resource, PermissionResource::FilesystemPath(path) if path == Path::new("/tmp/waltr-shell-delete-test"))
        }));
        drop(fs::remove_dir_all(root));
    }

    use std::path::PathBuf;

    fn temp_root(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "waltr-agent-shell-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }
}
