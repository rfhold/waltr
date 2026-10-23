//! Direct local execution of trusted configuration, never shell construction.
//!
//! Only `{{ text }}` and `{{ invocationId }}` are templates. Substitutions are
//! literal, nonrecursive, and never applied to argv[0]. Environment variables,
//! quoting, subshells, and tildes are not expanded. Explicitly configuring a
//! shell is privileged arbitrary code execution; configuration must be trusted.
//!
//! The caller must await `run` after signalling cancellation (including shutdown)
//! to observe kill-and-wait completion. Dropping the future also uses Tokio's
//! kill-on-drop safety net, but is not an acknowledged shutdown. Only the direct
//! child is owned; this is not a process-tree sandbox and cannot undo side effects.

use serde::Deserialize;
use std::{fmt, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

/// Maximum argv item count, including the executable.
pub const MAX_ARGUMENTS: usize = 64;
/// Maximum bytes per argument or stdin template before expansion.
pub const MAX_TEMPLATE_BYTES: usize = 16 * 1024;
/// Maximum final text input size, in bytes.
pub const MAX_TEXT_BYTES: usize = 128 * 1024;
/// Maximum combined argv and stdin size after expansion, in bytes.
pub const MAX_EXPANDED_BYTES: usize = 256 * 1024;
/// Maximum captured size per output stream, in bytes.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Maximum configurable command deadline, in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 300;

/// Trusted direct process specification; Debug deliberately omits its contents.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommandSpec {
    /// Fixed executable followed by literal, individually expanded arguments.
    #[serde(deserialize_with = "deserialize_argv")]
    pub argv: Vec<String>,
    /// Literal stdin template; defaults to empty and is closed after writing.
    #[serde(
        default,
        deserialize_with = "crate::bounded::string::<_, MAX_TEMPLATE_BYTES>"
    )]
    pub stdin: String,
    /// Deadline in 1..=300 seconds, defaulting to 10.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
#[derive(Deserialize)]
#[serde(transparent)]
struct TemplateArg(
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_TEMPLATE_BYTES>")] String,
);
fn deserialize_argv<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    crate::bounded::vec::<D, TemplateArg, MAX_ARGUMENTS>(deserializer)
        .map(|args| args.into_iter().map(|arg| arg.0).collect())
}
const fn default_timeout() -> u64 {
    10
}
impl fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSpec")
            .field("arguments", &self.argv.len())
            .field("timeout_seconds", &self.timeout_seconds)
            .finish_non_exhaustive()
    }
}

/// Structured errors intentionally exclude input, output, argv, and OS messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// Invalid specification, with a static nonsensitive reason.
    Invalid(&'static str),
    /// Text or invocation identifier exceeds its limit.
    InputLimit,
    /// Combined expanded argv and stdin exceeds its limit.
    ExpandedLimit,
    /// Cancellation was requested; any spawned child has been reaped.
    Cancelled,
    /// Deadline expired; the child has been killed and reaped.
    Timeout,
    /// One output pipe exceeded its bound.
    OutputLimit {
        /// Offending pipe, not its contents.
        stream: Stream,
    },
    /// Process or pipe I/O failure, without a potentially sensitive OS message.
    Io {
        /// Static operation identifier.
        operation: &'static str,
        /// OS error category.
        kind: std::io::ErrorKind,
    },
    /// Nonzero or signal exit; captured output is discarded.
    Exit {
        /// Exit code, or none for signal termination.
        code: Option<i32>,
    },
    /// Reaping failed; shutdown completion could not be acknowledged.
    Cleanup {
        /// OS error category from wait.
        kind: std::io::ErrorKind,
    },
}
/// Captured output pipe identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "local command: {self:?}")
    }
}
impl std::error::Error for CommandError {}

/// Captured bytes are sensitive and deliberately omitted from Debug.
pub struct CommandOutput {
    /// Bounded standard output from successful execution; may be sensitive.
    pub stdout: Vec<u8>,
    /// Bounded standard error from successful execution; may be sensitive.
    pub stderr: Vec<u8>,
}
impl fmt::Debug for CommandOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .finish()
    }
}

impl CommandSpec {
    /// Validate fixed executable, limits, timeout, and exact template spellings.
    ///
    /// # Errors
    /// Returns `Invalid` for invalid configuration or `ExpandedLimit` for size overflow.
    pub fn validate(&self) -> Result<(), CommandError> {
        if self.argv.is_empty() || self.argv.len() > MAX_ARGUMENTS {
            return Err(CommandError::Invalid("argument count"));
        }
        if self.argv[0].is_empty() || self.argv[0].contains(['{', '}', '\0']) {
            return Err(CommandError::Invalid("executable must be fixed"));
        }
        if !(1..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds) {
            return Err(CommandError::Invalid("timeout must be 1..300 seconds"));
        }
        for arg in &self.argv {
            if arg.len() > MAX_TEMPLATE_BYTES || arg.contains('\0') {
                return Err(CommandError::Invalid("argument size or NUL"));
            }
            expand(arg, "", "", &mut 0)?;
        }
        if self.stdin.len() > MAX_TEMPLATE_BYTES {
            return Err(CommandError::Invalid("stdin template size"));
        }
        expand(&self.stdin, "", "", &mut 0)?;
        Ok(())
    }
}

fn append(output: &mut String, value: &str, total: &mut usize) -> Result<(), CommandError> {
    *total = total
        .checked_add(value.len())
        .filter(|size| *size <= MAX_EXPANDED_BYTES)
        .ok_or(CommandError::ExpandedLimit)?;
    output.push_str(value);
    Ok(())
}
fn expand(
    template: &str,
    text: &str,
    invocation_id: &str,
    total: &mut usize,
) -> Result<String, CommandError> {
    let mut output = String::new();
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        let prefix = &remaining[..start];
        if prefix.contains("}}") {
            return Err(CommandError::Invalid("malformed template"));
        }
        append(&mut output, prefix, total)?;
        let rest = &remaining[start..];
        let (token, value) = if rest.starts_with("{{ text }}") {
            ("{{ text }}", text)
        } else if rest.starts_with("{{ invocationId }}") {
            ("{{ invocationId }}", invocation_id)
        } else {
            return Err(CommandError::Invalid("unknown or malformed template"));
        };
        append(&mut output, value, total)?;
        remaining = &rest[token.len()..];
    }
    if remaining.contains("}}") {
        return Err(CommandError::Invalid("malformed template"));
    }
    append(&mut output, remaining, total)?;
    Ok(output)
}

fn io_error(operation: &'static str, error: &std::io::Error) -> CommandError {
    CommandError::Io {
        operation,
        kind: error.kind(),
    }
}
async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    stream: Stream,
) -> Result<Vec<u8>, CommandError> {
    let mut output = Vec::new();
    let mut buffer = vec![0; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|e| io_error("read", &e))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > MAX_OUTPUT_BYTES {
            return Err(CommandError::OutputLimit { stream });
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

/// Execute once; callers own success gating and must never retry automatically.
///
/// `text` is at most 128 KiB, invocation ID at most 256 bytes. All expanded argv
/// and stdin together are at most 256 KiB, checked before spawning. Each output
/// stream is capped at 64 KiB; nonzero exit never returns captured output.
///
/// # Errors
/// Returns validation, size, cancellation, timeout, I/O, nonzero exit, or cleanup
/// errors. Every error after spawning kills and waits for the owned child.
///
/// # Panics
/// Panics if Tokio fails to provide a pipe explicitly requested at spawn.
pub async fn run(
    spec: &CommandSpec,
    text: &str,
    invocation_id: &str,
    cancel: &CancellationToken,
) -> Result<CommandOutput, CommandError> {
    spec.validate()?;
    if text.len() > MAX_TEXT_BYTES || invocation_id.len() > 256 {
        return Err(CommandError::InputLimit);
    }
    let mut total = 0;
    let mut argv = Vec::with_capacity(spec.argv.len());
    for argument in &spec.argv {
        let expanded = expand(argument, text, invocation_id, &mut total)?;
        if expanded.contains('\0') {
            return Err(CommandError::Invalid("NUL in expanded argument"));
        }
        argv.push(expanded);
    }
    let stdin = expand(&spec.stdin, text, invocation_id, &mut total)?;
    if cancel.is_cancelled() {
        return Err(CommandError::Cancelled);
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| io_error("spawn", &e))?;
    let mut input = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // These are borrowed concurrent futures, not spawned tasks. Any early error
    // drops all pipe futures before killing and reaping the direct child.
    let result = {
        let execution = async {
            let write = async {
                input
                    .write_all(stdin.as_bytes())
                    .await
                    .map_err(|e| io_error("stdin", &e))?;
                input
                    .shutdown()
                    .await
                    .map_err(|e| io_error("stdin close", &e))?;
                drop(input);
                Ok::<(), CommandError>(())
            };
            let wait = async { child.wait().await.map_err(|e| io_error("wait", &e)) };
            let ((), stdout, stderr, status) = tokio::try_join!(
                write,
                read_bounded(stdout, Stream::Stdout),
                read_bounded(stderr, Stream::Stderr),
                wait
            )?;
            if !status.success() {
                return Err(CommandError::Exit {
                    code: status.code(),
                });
            }
            Ok(CommandOutput { stdout, stderr })
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CommandError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(spec.timeout_seconds), execution) => result.unwrap_or(Err(CommandError::Timeout)),
        }
    };
    if result.is_err() {
        // start_kill can race an already exited/reaped process. wait is the
        // authoritative cleanup result and also works after a completed wait.
        let _kill_result = child.start_kill();
        child
            .wait()
            .await
            .map_err(|error| CommandError::Cleanup { kind: error.kind() })?;
    }
    result
}
