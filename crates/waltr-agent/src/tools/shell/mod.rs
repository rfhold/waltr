//! Shell tool configuration, schemas, and permission scanning.

mod config;
mod parser;
mod path;
mod permissions;

pub use config::{ShellConfig, ShellInput};
pub use parser::{ParsedShellCommand, ShellScan};

use parser::{ShellCommandParser, shell_words};
use path::{is_dynamic_path, resolve_model_path};
use permissions::infer_permission_claims;

use crate::LlmError;

fn shell_error(message: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        provider: "shell",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PermissionOperation, PermissionResource, PermissionResourcePattern};

    #[test]
    fn scan_extracts_commands_and_path_claims() {
        let config = ShellConfig::new("/workspace");
        let scan = config
            .scan(&ShellInput::new("cat src/main.rs && echo done", "read"))
            .expect("scan shell command");
        assert!(
            scan.commands
                .iter()
                .any(|command| command.source == "cat src/main.rs")
        );
        assert!(scan.claims.iter().any(|claim| {
            claim.operation == PermissionOperation::Read
                && matches!(&claim.resource, PermissionResource::FilesystemPath(path)
                    if path == std::path::Path::new("/workspace/src/main.rs"))
        }));
    }

    #[test]
    fn scan_infers_nested_commands_redirections_and_reusable_patterns() {
        let config = ShellConfig::new("/workspace");
        let nested = config
            .scan(&ShellInput::new("echo $(cat /etc/passwd)", "nested"))
            .expect("nested command");
        assert!(
            nested
                .commands
                .iter()
                .any(|command| command.source == "cat /etc/passwd")
        );
        assert!(nested.claims.iter().any(|claim| matches!(
            &claim.resource,
            PermissionResource::FilesystemPath(path) if path == std::path::Path::new("/etc/passwd")
        )));

        let redirect = config
            .scan(&ShellInput::new("echo hi > output.txt", "redirect"))
            .expect("redirect");
        assert!(redirect.claims.iter().any(|claim| {
            claim.operation == PermissionOperation::Write
                && matches!(&claim.resource, PermissionResource::FilesystemPath(path)
                    if path == std::path::Path::new("/workspace/output.txt"))
        }));

        let prefix = config
            .scan(&ShellInput::new("git status --short", "status"))
            .expect("git status");
        assert!(prefix.always_patterns.iter().any(|pattern| matches!(
            &pattern.resource,
            PermissionResourcePattern::ShellCommandPrefix(prefix) if prefix == "git status "
        )));
    }

    #[test]
    fn scan_handles_workdirs_dynamic_paths_and_cwd_commands() {
        let config = ShellConfig::new("/workspace").with_default_workdir("/workspace/subdir");
        let scan = config
            .scan(&ShellInput::new("cargo test", "test"))
            .expect("scan");
        assert!(scan.claims.iter().any(|claim| {
            claim.operation == PermissionOperation::Read
                && matches!(&claim.resource, PermissionResource::WorkingDirectory(path)
                    if path == std::path::Path::new("/workspace/subdir"))
        }));
        let outside = config
            .scan(&ShellInput::new("cargo test", "test").with_workdir("/tmp"))
            .expect("outside workdir");
        assert!(outside.claims.iter().any(|claim| matches!(
            &claim.resource,
            PermissionResource::WorkingDirectory(path) if path == std::path::Path::new("/tmp")
        )));

        let dynamic = config
            .scan(&ShellInput::new("cat \"$FILE\"", "dynamic"))
            .expect("dynamic path");
        assert!(
            !dynamic
                .claims
                .iter()
                .any(|claim| matches!(claim.resource, PermissionResource::FilesystemPath(_)))
        );
        let cwd = config.scan(&ShellInput::new("cd ..", "cwd")).expect("cwd");
        assert!(
            !cwd.claims
                .iter()
                .any(|claim| matches!(claim.resource, PermissionResource::ShellCommand(_)))
        );
    }
}
