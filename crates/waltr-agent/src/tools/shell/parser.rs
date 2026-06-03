use std::path::Path;

use tree_sitter::{Node, Parser};

use super::{infer_permission_claims, shell_error};
use crate::{PermissionClaim, PermissionPattern, Result};

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

pub(super) struct ShellCommandParser {
    parser: Parser,
}

impl ShellCommandParser {
    pub(super) fn new() -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .map_err(|error| shell_error(format!("failed to initialize bash parser: {error}")))?;
        Ok(Self { parser })
    }

    pub(super) fn scan(mut self, command: &str, root: &Path, workdir: &Path) -> Result<ShellScan> {
        let tree = self
            .parser
            .parse(command, None)
            .ok_or_else(|| shell_error("failed to parse shell command"))?;
        let bytes = command.as_bytes();
        let mut parsed = Vec::new();
        collect_command_nodes(tree.root_node(), bytes, &mut parsed);

        dedupe_commands(&mut parsed);
        let (claims, patterns) = infer_permission_claims(&parsed, root, workdir);

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

pub(super) fn shell_words(source: &str) -> Vec<String> {
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
