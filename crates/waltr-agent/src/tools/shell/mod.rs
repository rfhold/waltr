//! Shell tool configuration and implementation.

mod config;
mod execute;
mod formatting;
mod parser;
mod path;
mod permissions;

use serde::de::DeserializeOwned;

pub use config::{ShellConfig, ShellInput};
pub use parser::{ParsedShellCommand, ShellScan};

use formatting::format_process_output;
use parser::{ShellCommandParser, shell_words};
use path::{is_dynamic_path, resolve_model_path};
use permissions::infer_permission_claims;

use crate::{LlmError, Result, ToolCall};

fn tool_input<T>(call: &ToolCall) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(call.arguments.clone()).map_err(Into::into)
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
