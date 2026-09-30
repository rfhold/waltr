use std::{
    fmt::Write as _,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::Value;
use tokio::{io::AsyncReadExt, process::Command, sync::mpsc, time};
use tokio_util::sync::CancellationToken;
use tree_sitter::{Node, Parser};

use crate::tools::{Claim, Execution, Result, SandboxError, ShellDefaults};

fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(part.as_os_str()),
        }
    }
    result
}

// Resolve the existing prefix (including symlinks), leaving missing suffixes
// lexical so prospective writes can still be claimed before they exist.
fn claim_path(path: &Path) -> PathBuf {
    claim_path_inner(path, 0)
}

fn claim_path_inner(path: &Path, depth: usize) -> PathBuf {
    if depth > 64 {
        return normalized(path);
    }
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        if let Ok(metadata) = ancestor.symlink_metadata()
            && metadata.file_type().is_symlink()
            && let Ok(target) = std::fs::read_link(ancestor)
        {
            let target = if target.is_absolute() {
                target
            } else {
                ancestor
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .join(target)
            };
            let mut resolved = claim_path_inner(&target, depth + 1);
            for component in suffix.into_iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        if let Some(name) = ancestor.file_name() {
            suffix.push(name.to_os_string());
        }
        let Some(parent) = ancestor.parent() else {
            return normalized(path);
        };
        ancestor = parent;
    }
    let mut result = ancestor
        .canonicalize()
        .unwrap_or_else(|_| normalized(ancestor));
    for component in suffix.into_iter().rev() {
        result.push(component);
    }
    result
}

pub fn workdir(root: &Path, default_workdir: &Path, args: &Value) -> PathBuf {
    let Some(requested) = args.get("workdir").and_then(Value::as_str) else {
        let path = if default_workdir.is_absolute() {
            default_workdir.to_path_buf()
        } else {
            root.join(default_workdir)
        };
        return claim_path(&path);
    };
    let path = Path::new(
        requested
            .trim()
            .trim_matches(|c| matches!(c, '`' | '"' | '\'')),
    );
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    // The shell's cwd is not confined to the root; the host authorizes its claim.
    claim_path(&path)
}

fn resolve_path(path: &str, root: &Path, workdir: &Path) -> PathBuf {
    let path = path
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\''))
        .trim_end_matches(':');
    let path = Path::new(path);
    if path.starts_with("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return claim_path(&PathBuf::from(home).join(path.strip_prefix("~").unwrap_or(path)));
    }
    if path.is_absolute() {
        claim_path(path)
    } else if workdir.is_absolute() {
        claim_path(&workdir.join(path))
    } else {
        claim_path(&root.join(workdir).join(path))
    }
}

fn dynamic(token: &str) -> bool {
    token.contains('$') || token.contains('`') || token.contains("<(") || token.contains(">(")
}

#[derive(Debug)]
struct Parsed {
    source: String,
    tokens: Vec<String>,
    cwd: bool,
}

fn collect(node: Node<'_>, bytes: &[u8], commands: &mut Vec<Parsed>) {
    if node.kind() == "command" {
        let source_node = node
            .parent()
            .filter(|p| matches!(p.kind(), "redirected_statement" | "file_redirect"))
            .unwrap_or(node);
        if let (Ok(source), Ok(command)) = (source_node.utf8_text(bytes), node.utf8_text(bytes)) {
            let source = source.trim().to_owned();
            let tokens = words(command.trim());
            if !source.is_empty()
                && !tokens.is_empty()
                && !commands
                    .iter()
                    .any(|p| p.source == source && p.tokens == tokens)
            {
                let cwd = matches!(tokens[0].as_str(), "cd" | "chdir" | "pushd" | "popd");
                commands.push(Parsed {
                    source,
                    tokens,
                    cwd,
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, bytes, commands);
    }
}

fn words(source: &str) -> Vec<String> {
    let (mut words, mut current, mut quote) = (Vec::new(), String::new(), None);
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '\\' => {
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            }
            ' ' | '\t' | '\n' | '\r' => push_word(&mut words, &mut current),
            ';' => {
                push_word(&mut words, &mut current);
                break;
            }
            '&' | '|' if chars.peek() == Some(&c) => {
                push_word(&mut words, &mut current);
                break;
            }
            '>' | '<' => {
                push_word(&mut words, &mut current);
                if chars.peek() == Some(&c) {
                    chars.next();
                    words.push(format!("{c}{c}"));
                } else {
                    words.push(c.to_string());
                }
            }
            _ => current.push(c),
        }
    }
    push_word(&mut words, &mut current);
    words
}

fn push_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

fn claim(operation: &str, kind: &str, resource: impl Into<String>) -> Claim {
    Claim {
        operation: operation.into(),
        resource_kind: kind.into(),
        resource: resource.into(),
    }
}

pub fn claims(command: &str, root: &Path, workdir: &Path) -> Result<Vec<Claim>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|e| SandboxError(e.to_string()))?;
    let tree = parser
        .parse(command, None)
        .ok_or_else(|| SandboxError("failed to parse shell command".into()))?;
    let mut parsed = Vec::new();
    collect(tree.root_node(), command.as_bytes(), &mut parsed);
    let mut claims = vec![claim(
        "read",
        "working_directory",
        workdir.display().to_string(),
    )];
    for item in parsed {
        if item.cwd {
            if let Some(path) = item.tokens.get(1).filter(|p| !dynamic(p)) {
                claims.push(claim(
                    "read",
                    "working_directory",
                    resolve_path(path, root, workdir).display().to_string(),
                ));
            } else {
                // A dynamic cwd cannot be inferred: require command approval.
                claims.push(claim("execute", "shell_command", item.source.clone()));
            }
        } else {
            claims.push(claim("execute", "shell_command", item.source.clone()));
        }
        let operation = match item.tokens[0].as_str() {
            "cat" | "less" | "more" | "head" | "tail" | "grep" | "rg" | "ls" | "find" => {
                Some("read")
            }
            "touch" | "mkdir" | "cp" | "mv" => Some("write"),
            "rm" | "rmdir" => Some("delete"),
            _ => None,
        };
        if let Some(operation) = operation {
            for token in item.tokens.iter().skip(1).filter(|t| {
                !t.starts_with('-')
                    && !matches!(t.as_str(), ">" | ">>" | "<" | "<<")
                    && !dynamic(t)
                    && (t.contains('/') || t.starts_with('.') || t.starts_with('~'))
            }) {
                claims.push(claim(
                    operation,
                    "filesystem_path",
                    resolve_path(token, root, workdir).display().to_string(),
                ));
            }
        }
        let source_tokens = words(&item.source);
        for pair in source_tokens.windows(2) {
            if matches!(pair[0].as_str(), ">" | ">>") && !dynamic(&pair[1]) {
                claims.push(claim(
                    "write",
                    "filesystem_path",
                    resolve_path(&pair[1], root, workdir).display().to_string(),
                ));
            }
        }
    }
    let mut deduped = Vec::new();
    for claim in claims {
        if !deduped.contains(&claim) {
            deduped.push(claim);
        }
    }
    Ok(deduped)
}

async fn terminate(pid: u32) {
    #[cfg(unix)]
    {
        // The child starts its own group so both the shell and its children stop.
        let group = format!("-{pid}");
        drop(
            Command::new("/bin/bash")
                .arg("-c")
                .arg("kill -TERM -- \"$1\"")
                .arg("kill")
                .arg(&group)
                .stderr(Stdio::null())
                .status()
                .await,
        );
        time::sleep(Duration::from_millis(100)).await;
        drop(
            Command::new("/bin/bash")
                .arg("-c")
                .arg("kill -KILL -- \"$1\"")
                .arg("kill")
                .arg(&group)
                .stderr(Stdio::null())
                .status()
                .await,
        );
    }
    #[cfg(not(unix))]
    {
        let pid = pid.to_string();
        drop(Command::new("kill").arg("-TERM").arg(&pid).status().await);
        time::sleep(Duration::from_millis(100)).await;
        drop(Command::new("kill").arg("-KILL").arg(&pid).status().await);
    }
}

async fn read_output(
    mut reader: impl AsyncReadExt + Unpin,
    level: &str,
    progress: Option<mpsc::Sender<(String, String)>>,
    output: Arc<Mutex<Vec<u8>>>,
    max_output_bytes: usize,
) -> Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        {
            let mut output = output.lock().map_err(|e| SandboxError(e.to_string()))?;
            let remaining = max_output_bytes.saturating_sub(output.len());
            output.extend_from_slice(&buffer[..count.min(remaining)]);
        }
        if let Some(sender) = &progress {
            drop(sender.try_send((
                level.into(),
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn execute(
    root: &Path,
    defaults: &ShellDefaults,
    args: &Value,
    cancel: CancellationToken,
    progress: Option<mpsc::Sender<(String, String)>>,
) -> Result<Execution> {
    if cancel.is_cancelled() {
        return Err(SandboxError("shell command canceled".into()));
    }
    let workdir = workdir(root, &defaults.default_workdir, args);
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| SandboxError("missing or invalid command".into()))?;
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .map_or(defaults.default_timeout, Duration::from_secs);
    let wrapped = format!("trap 'jobs -pr | xargs -r kill -TERM; exit 143' TERM INT; {command}");
    let mut command = Command::new("/bin/bash");
    command
        .arg("-lc")
        .arg(wrapped)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| SandboxError("failed to identify shell process".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SandboxError("missing stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SandboxError("missing stderr".into()))?;
    let stdout_output = Arc::new(Mutex::new(Vec::new()));
    let stderr_output = Arc::new(Mutex::new(Vec::new()));
    let mut stdout_reader = tokio::spawn(read_output(
        stdout,
        "info",
        progress.clone(),
        Arc::clone(&stdout_output),
        defaults.max_output_bytes,
    ));
    let mut stderr_reader = tokio::spawn(read_output(
        stderr,
        "error",
        progress,
        Arc::clone(&stderr_output),
        defaults.max_output_bytes,
    ));
    let (code, timed_out) = tokio::select! {
        biased;
        () = cancel.cancelled() => { terminate(pid).await; (child.wait().await?.code(), false) },
        () = time::sleep(timeout) => { terminate(pid).await; (child.wait().await?.code(), true) },
        status = child.wait() => (status?.code(), false),
    };
    // A background descendant may retain the pipe after the shell exits.
    // Stop its process group and bound the final pipe drain in that case.
    let drain = async { tokio::join!(&mut stdout_reader, &mut stderr_reader) };
    if let Ok((stdout, stderr)) = time::timeout(Duration::from_millis(250), drain).await {
        stdout.map_err(|e| SandboxError(e.to_string()))??;
        stderr.map_err(|e| SandboxError(e.to_string()))??;
    } else {
        terminate(pid).await;
        stdout_reader.abort();
        stderr_reader.abort();
        drop(stdout_reader.await);
        drop(stderr_reader.await);
    }
    let stdout = stdout_output
        .lock()
        .map_err(|e| SandboxError(e.to_string()))?;
    let stderr = stderr_output
        .lock()
        .map_err(|e| SandboxError(e.to_string()))?;
    let mut content = String::new();
    if timed_out {
        content.push_str("Timed out: true\n");
    }
    writeln!(
        content,
        "Exit code: {}\n",
        code.map_or_else(|| "signal".to_owned(), |n| n.to_string())
    )
    .expect("write string");
    content.push_str(&String::from_utf8_lossy(&stdout));
    content.push_str(&String::from_utf8_lossy(&stderr));
    if content.len() > defaults.max_output_bytes {
        content.truncate(content.floor_char_boundary(defaults.max_output_bytes));
        content.push_str("\n[output truncated]");
    }
    Ok(Execution {
        content,
        is_error: false,
    })
}
