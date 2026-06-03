use std::path::Path;

use super::{ParsedShellCommand, is_dynamic_path, resolve_model_path, shell_words};
use crate::{
    PermissionClaim, PermissionOperation, PermissionPattern, PermissionResource,
    PermissionResourcePattern,
};

pub(super) fn infer_permission_claims(
    parsed: &[ParsedShellCommand],
    root: &Path,
    workdir: &Path,
) -> (Vec<PermissionClaim>, Vec<PermissionPattern>) {
    let mut claims = Vec::new();
    let mut patterns = Vec::new();
    for parsed_command in parsed {
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
    (claims, patterns)
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
