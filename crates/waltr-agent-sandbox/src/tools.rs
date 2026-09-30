use std::{
    collections::{BTreeSet, HashMap},
    fmt::{self, Write as _},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Error returned by worker-side validation, execution or workspace I/O.
#[derive(Debug)]
pub struct SandboxError(pub String);

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SandboxError {}

impl From<std::io::Error> for SandboxError {
    fn from(value: std::io::Error) -> Self {
        Self(value.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SandboxError>;

/// Permission claim inferred while preparing a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// Read, write, delete or execute.
    pub operation: String,
    /// Resource category consumed by the host permission engine.
    pub resource_kind: String,
    /// Resolved path or command text.
    pub resource: String,
}

/// Single-use authorization ticket and the claims to approve before execution.
#[derive(Debug)]
pub struct Prepared {
    /// Opaque ticket, bound to the invocation id and stored arguments.
    pub ticket: String,
    /// Permission claims for the host to evaluate.
    pub claims: Vec<Claim>,
}

/// Tool result returned to the host.
#[derive(Debug)]
pub struct Execution {
    /// Text output of the operation.
    pub content: String,
    /// Whether this is a tool-level error rather than an RPC error.
    pub is_error: bool,
}

/// Read-only workspace context result.
#[derive(Debug)]
pub struct WorkspaceResult {
    /// Text content or search results.
    pub content: String,
    /// Paths or skill catalog records for listing operations.
    pub entries: Vec<String>,
    /// Worker-local resolved path, if requested.
    pub canonical_path: String,
    /// File, directory, or other for a resolved path.
    pub path_kind: String,
    /// Worker-local canonical workspace root.
    pub canonical_root: String,
}

#[derive(Debug)]
struct Invocation {
    created: Instant,
    id: String,
    tool: String,
    args: Value,
    claims: Vec<Claim>,
    #[cfg_attr(not(feature = "shell"), allow(dead_code))]
    shell_defaults: Option<ShellDefaults>,
}

#[cfg_attr(not(feature = "shell"), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct ShellDefaults {
    pub default_workdir: PathBuf,
    pub default_timeout: Duration,
    pub max_output_bytes: usize,
}

#[cfg(feature = "shell")]
impl ShellDefaults {
    fn for_root(root: &Path) -> Self {
        Self {
            default_workdir: root.to_path_buf(),
            default_timeout: Duration::from_secs(30),
            max_output_bytes: 20 * 1024,
        }
    }
}

const MAX_RESULTS: usize = 1024;
const MAX_LIST_BYTES: usize = 256 * 1024;
const RESULTS_TRUNCATED: &str = "<results truncated>";
const MAX_SNAPSHOT_DEPTH: usize = 16;
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024;
const SNAPSHOT_TRUNCATED: &str = "- <snapshot truncated>\n";
const TICKET_TTL: Duration = Duration::from_mins(5);

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Local worker, never a host-side fallback. Tickets bind a single execution
/// to exactly the tool and arguments that were inspected during preparation.
#[derive(Debug)]
pub struct Sandbox {
    root: PathBuf,
    tickets: Mutex<HashMap<String, Invocation>>,
}

impl Sandbox {
    /// Construct a worker rooted at an existing directory.
    ///
    /// # Errors
    /// Returns an error if the root is missing or not a directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().canonicalize()?;
        if !root.is_dir() {
            return Err(SandboxError("workspace root is not a directory".into()));
        }
        Ok(Self {
            root,
            tickets: Mutex::new(HashMap::new()),
        })
    }

    /// Canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate input without performing any writes or executing commands.
    /// The returned ticket is single-use and cannot be supplied with new args.
    ///
    /// # Errors
    /// Returns an error for malformed input or a full ticket table.
    #[allow(clippy::too_many_lines)]
    pub fn prepare(&self, id: &str, tool: &str, arguments_json: &str) -> Result<Prepared> {
        self.prepare_with_shell_defaults(id, tool, arguments_json, None)
    }

    #[allow(clippy::too_many_lines)]
    #[cfg_attr(not(feature = "shell"), allow(clippy::needless_pass_by_value))]
    pub(crate) fn prepare_with_shell_defaults(
        &self,
        id: &str,
        tool: &str,
        arguments_json: &str,
        shell_defaults: Option<ShellDefaults>,
    ) -> Result<Prepared> {
        if tool != "shell" && shell_defaults.is_some() {
            return Err(SandboxError("shell defaults on non-shell tool".into()));
        }
        if id.is_empty() {
            return Err(SandboxError("empty invocation id".into()));
        }
        let args: Value =
            serde_json::from_str(arguments_json).map_err(|e| SandboxError(e.to_string()))?;
        if !args.is_object() {
            return Err(SandboxError("tool arguments must be an object".into()));
        }
        let (operation, resource_kind, resource) = match tool {
            "read" | "edit" | "write" | "delete" => {
                let path = resolved(&self.root, str_arg(&args, "filePath")?, tool == "write")?;
                if tool == "read" {
                    optional_integer(&args, "offset", 1)?;
                    optional_integer(&args, "limit", 0)?;
                }
                if tool == "edit" {
                    str_arg(&args, "oldString")?;
                    str_arg(&args, "newString")?;
                    if args
                        .get("replaceAll")
                        .is_some_and(|value| !value.is_boolean())
                    {
                        return Err(SandboxError("invalid replaceAll".into()));
                    }
                }
                if tool == "write" {
                    str_arg(&args, "content")?;
                }
                (
                    match tool {
                        "read" => "read",
                        "delete" => "delete",
                        _ => "write",
                    },
                    "filesystem_path",
                    path.display().to_string(),
                )
            }
            "glob" | "grep" => {
                let pattern = str_arg(&args, "pattern")?;
                optional_string(&args, "path")?;
                optional_string(&args, "include")?;
                if tool == "glob" {
                    glob_pattern(pattern)?;
                }
                if tool == "grep"
                    && let Some(include) = args.get("include").and_then(Value::as_str)
                {
                    glob_pattern(include)?;
                }
                let path = resolved(
                    &self.root,
                    args.get("path").and_then(Value::as_str).unwrap_or("."),
                    false,
                )?;
                ("read", "filesystem_path", path.display().to_string())
            }
            "skill" => {
                let name = str_arg(&args, "name")?;
                if !skill_catalog(&self.root)
                    .iter()
                    .any(|(dir, _, _)| dir == name)
                {
                    return Err(SandboxError("skill is not visible".into()));
                }
                let path = self.scoped(&skill_path(name)?, false)?;
                ("read", "filesystem_path", path.display().to_string())
            }
            "apply_patch" => {
                let claims = crate::patch::claims(&self.root, str_arg(&args, "patchText")?)?
                    .into_iter()
                    .map(|(operation, resource)| Claim {
                        operation,
                        resource_kind: "filesystem_path".into(),
                        resource,
                    })
                    .collect();
                return self.store_prepared(id, tool, args, claims, None);
            }
            #[cfg(feature = "shell")]
            "shell" => {
                let command = str_arg(&args, "command")?;
                str_arg(&args, "description")?;
                optional_string(&args, "workdir")?;
                optional_integer(&args, "timeout", 1)?;
                let shell_defaults =
                    shell_defaults.unwrap_or_else(|| ShellDefaults::for_root(&self.root));
                let workdir =
                    crate::shell::workdir(&self.root, &shell_defaults.default_workdir, &args);
                let claims = crate::shell::claims(command, &self.root, &workdir)?;
                return self.store_prepared(id, tool, args, claims, Some(shell_defaults));
            }
            _ => return Err(SandboxError(format!("unsupported tool: {tool}"))),
        };
        let claims = vec![Claim {
            operation: operation.into(),
            resource_kind: resource_kind.into(),
            resource,
        }];
        self.store_prepared(id, tool, args, claims, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn store_prepared(
        &self,
        id: &str,
        tool: &str,
        args: Value,
        claims: Vec<Claim>,
        shell_defaults: Option<ShellDefaults>,
    ) -> Result<Prepared> {
        let mut tickets = self
            .tickets
            .lock()
            .map_err(|e| SandboxError(e.to_string()))?;
        tickets.retain(|_, invocation| invocation.created.elapsed() < TICKET_TTL);
        if tickets.values().any(|invocation| invocation.id == id) {
            return Err(SandboxError("invocation already prepared".into()));
        }
        if tickets.len() >= 128 {
            return Err(SandboxError("too many pending invocations".into()));
        }
        let ticket = uuid::Uuid::new_v4().to_string();
        tickets.insert(
            ticket.clone(),
            Invocation {
                created: Instant::now(),
                id: id.into(),
                tool: tool.into(),
                args,
                claims: claims.clone(),
                shell_defaults,
            },
        );
        drop(tickets);
        Ok(Prepared { ticket, claims })
    }

    /// Consume the prepared ticket, rechecking paths immediately before I/O.
    ///
    /// # Errors
    /// Returns an error if the ticket is invalid, canceled or the operation fails.
    pub async fn execute(
        &self,
        id: &str,
        ticket: &str,
        cancel: CancellationToken,
    ) -> Result<Execution> {
        self.execute_with_progress(id, ticket, cancel, None).await
    }

    pub(crate) async fn execute_with_progress(
        &self,
        id: &str,
        ticket: &str,
        cancel: CancellationToken,
        progress: Option<tokio::sync::mpsc::Sender<(String, String)>>,
    ) -> Result<Execution> {
        if cancel.is_cancelled() {
            return Err(SandboxError("canceled".into()));
        }
        let invocation = {
            let mut tickets = self
                .tickets
                .lock()
                .map_err(|e| SandboxError(e.to_string()))?;
            if tickets
                .get(ticket)
                .is_none_or(|inv| inv.id != id || inv.created.elapsed() >= TICKET_TTL)
            {
                return Err(SandboxError("invalid or expired ticket".into()));
            }
            tickets
                .remove(ticket)
                .ok_or_else(|| SandboxError("invalid ticket".into()))?
        };
        if invocation.tool == "shell" {
            #[cfg(feature = "shell")]
            {
                let defaults = invocation
                    .shell_defaults
                    .as_ref()
                    .expect("shell ticket has defaults");
                let workdir =
                    crate::shell::workdir(&self.root, &defaults.default_workdir, &invocation.args);
                let claims = crate::shell::claims(
                    str_arg(&invocation.args, "command")?,
                    &self.root,
                    &workdir,
                )?;
                if claims != invocation.claims {
                    return Err(SandboxError("path changed since preparation".into()));
                }
            }
            #[cfg(feature = "shell")]
            return Box::pin(
                self.shell(
                    &invocation.args,
                    invocation
                        .shell_defaults
                        .as_ref()
                        .expect("shell ticket has defaults"),
                    cancel,
                    progress,
                ),
            )
            .await;
            #[cfg(not(feature = "shell"))]
            {
                drop(progress);
                return Err(SandboxError("shell is disabled".into()));
            }
        }
        let root = self.root.clone();
        let guard = CancelOnDrop(cancel.clone());
        let task = tokio::task::spawn_blocking(move || {
            if cancel.is_cancelled() {
                return Err(SandboxError("canceled".into()));
            }
            Self::validate_claims(
                &root,
                &invocation.tool,
                &invocation.args,
                &invocation.claims,
            )?;
            Self::filesystem(&root, &invocation.tool, &invocation.args, false, &cancel)
        });
        // A synchronous filesystem operation cannot be interrupted safely. Wait
        // for it to finish before reporting cancellation to the host.
        let result = task.await.map_err(|e| SandboxError(e.to_string()))??;
        if guard.0.is_cancelled() {
            return Err(SandboxError("canceled".into()));
        }
        Ok(Execution {
            content: result,
            is_error: false,
        })
    }

    /// Invalidate pending tickets for an invocation.
    pub fn cancel_prepared(&self, id: &str) {
        if let Ok(mut tickets) = self.tickets.lock() {
            tickets.retain(|_, inv| inv.id != id);
        }
    }

    /// Workspace context read/list/search; never exposes mutating operations.
    ///
    /// # Errors
    /// Returns an error if input is invalid or workspace I/O fails.
    pub async fn workspace(&self, operation: &str, args: &Value) -> Result<WorkspaceResult> {
        let root = self.root.clone();
        let operation = operation.to_owned();
        let args = args.clone();
        tokio::task::spawn_blocking(move || {
            if operation == "info" || operation == "resolve" {
                let path = if operation == "info" {
                    root.clone()
                } else {
                    resolved(&root, str_arg(&args, "filePath")?, false)?
                };
                let metadata = fs::metadata(&path)?;
                let path_kind = if metadata.is_file() {
                    "file"
                } else if metadata.is_dir() {
                    "directory"
                } else {
                    "other"
                };
                return Ok(WorkspaceResult {
                    content: String::new(),
                    entries: Vec::new(),
                    canonical_path: path.to_string_lossy().into_owned(),
                    path_kind: path_kind.into(),
                    canonical_root: root.to_string_lossy().into_owned(),
                });
            }
            let tool = match operation.as_str() {
                "read" | "formatted_read" => "read",
                "skill" => "skill",
                "list" | "glob" => "glob",
                "search_files" => "search_files",
                "search" | "grep" => "grep",
                "skills" => "skills",
                _ => return Err(SandboxError("unsupported workspace operation".into())),
            };
            let content = if operation == "read" {
                let path = scoped(&root, str_arg(&args, "filePath")?, false)?;
                fs::read_to_string(path)?
            } else {
                Self::filesystem(&root, tool, &args, true, &CancellationToken::new())?
            };
            let entries = if tool == "skills" {
                skill_catalog(&root)
                    .into_iter()
                    .map(|(directory, _, _)| directory)
                    .collect()
            } else if matches!(tool, "glob" | "search_files") && content != "No files found" {
                content
                    .lines()
                    .filter(|line| *line != RESULTS_TRUNCATED)
                    .map(str::to_owned)
                    .collect()
            } else {
                Vec::new()
            };
            Ok(WorkspaceResult {
                content,
                entries,
                canonical_path: String::new(),
                path_kind: String::new(),
                canonical_root: root.to_string_lossy().into_owned(),
            })
        })
        .await
        .map_err(|e| SandboxError(e.to_string()))?
    }

    /// Walk only as far as the requested bounds permit, without a glob or a full listing.
    /// Hidden and gitignored paths are excluded; symlinks are never followed.
    ///
    /// # Errors
    /// Returns an error if the requested directory escapes the workspace or is unreadable.
    pub async fn snapshot(
        &self,
        path: &str,
        max_entries: usize,
        max_depth: usize,
        max_bytes: usize,
    ) -> Result<WorkspaceResult> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let base = scoped(&root, if path.is_empty() { "." } else { &path }, false)?;
            if !base.is_dir() {
                return Err(SandboxError("snapshot path is not a directory".into()));
            }
            let max_entries = max_entries.min(MAX_RESULTS);
            let max_depth = max_depth.min(MAX_SNAPSHOT_DEPTH);
            let max_bytes = max_bytes.min(MAX_SNAPSHOT_BYTES);
            let mut entries = Vec::new();
            let mut bytes = 0usize;
            let mut truncated = false;
            let mut builder = WalkBuilder::new(&base);
            builder
                .standard_filters(true)
                .require_git(false)
                .max_depth(Some(max_depth))
                .sort_by_file_name(std::cmp::Ord::cmp);
            for item in builder.build() {
                let Ok(item) = item else { continue };
                if item.depth() == 0 || item.file_type().is_none_or(|kind| kind.is_symlink()) {
                    continue;
                }
                let relative = item
                    .path()
                    .strip_prefix(&base)
                    .map_err(|e| SandboxError(e.to_string()))?;
                let mut entry = relative.to_string_lossy().replace('\\', "/");
                if item.file_type().is_some_and(|kind| kind.is_dir()) {
                    entry.push('/');
                }
                let line_bytes = 2 + entry.len() + 1;
                if entries.len() == max_entries || bytes.saturating_add(line_bytes) > max_bytes {
                    truncated = true;
                    break;
                }
                bytes += line_bytes;
                entries.push(entry);
            }
            if truncated && max_bytes >= SNAPSHOT_TRUNCATED.len() {
                while bytes + SNAPSHOT_TRUNCATED.len() > max_bytes {
                    let removed: String = entries
                        .pop()
                        .ok_or_else(|| SandboxError("snapshot marker exceeds budget".into()))?;
                    bytes -= 2 + removed.len() + 1;
                }
            }
            let mut content = String::new();
            for entry in &entries {
                content.push_str("- ");
                content.push_str(entry);
                content.push('\n');
            }
            if truncated && max_bytes >= SNAPSHOT_TRUNCATED.len() {
                content.push_str(SNAPSHOT_TRUNCATED);
            }
            Ok(WorkspaceResult {
                content: content.trim_end().to_owned(),
                entries,
                canonical_path: String::new(),
                path_kind: String::new(),
                canonical_root: root.to_string_lossy().into_owned(),
            })
        })
        .await
        .map_err(|e| SandboxError(e.to_string()))?
    }

    fn scoped(&self, path: &str, writable: bool) -> Result<PathBuf> {
        scoped(&self.root, path, writable)
    }

    fn validate_claims(root: &Path, tool: &str, args: &Value, claims: &[Claim]) -> Result<()> {
        let paths = match tool {
            "read" | "edit" | "write" | "delete" => {
                vec![resolved(root, str_arg(args, "filePath")?, tool == "write")?]
            }
            "glob" | "grep" => vec![resolved(
                root,
                args.get("path").and_then(Value::as_str).unwrap_or("."),
                false,
            )?],
            "skill" => vec![scoped(root, &skill_path(str_arg(args, "name")?)?, false)?],
            "apply_patch" => crate::patch::claims(root, str_arg(args, "patchText")?)?
                .into_iter()
                .map(|(_, path)| PathBuf::from(path))
                .collect(),
            _ => return Err(SandboxError("unsupported tool".into())),
        };
        if paths.len() != claims.len()
            || paths.iter().zip(claims).any(|(path, claim)| {
                claim.resource_kind != "filesystem_path" || claim.resource != path.to_string_lossy()
            })
        {
            return Err(SandboxError("path changed since preparation".into()));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn filesystem(
        root: &Path,
        tool: &str,
        args: &Value,
        restricted: bool,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let path_for = |raw: &str, writable| {
            if restricted {
                scoped(root, raw, writable)
            } else {
                resolved(root, raw, writable)
            }
        };
        match tool {
            "apply_patch" => crate::patch::apply(root, str_arg(args, "patchText")?, cancel),
            "skill" => {
                let path = scoped(root, &skill_path(str_arg(args, "name")?)?, false)?;
                Ok(fs::read_to_string(path)?)
            }
            "skills" => Ok(skill_catalog(root)
                .into_iter()
                .map(|(_, name, description)| format!("{name}: {description}"))
                .collect::<Vec<_>>()
                .join("\n")),
            "read" => {
                let path = path_for(str_arg(args, "filePath")?, false)?;
                if path.is_dir() {
                    let mut entries = fs::read_dir(&path)?
                        .map(|item| {
                            let item = item?;
                            let mut name = item.file_name().to_string_lossy().into_owned();
                            if item.path().is_dir() {
                                name.push('/');
                            }
                            Ok(name)
                        })
                        .collect::<std::io::Result<Vec<_>>>()?;
                    entries.sort();
                    return Ok(format!(
                        "<path>{}</path>\n<type>directory</type>\n<entries>\n{}\n({} entries)\n</entries>",
                        path.display(),
                        entries.join("\n"),
                        entries.len()
                    ));
                }
                let content = fs::read_to_string(&path)?;
                let lines: Vec<_> = content.lines().collect();
                let offset = usize::try_from(
                    args.get("offset")
                        .and_then(Value::as_u64)
                        .unwrap_or(1)
                        .max(1),
                )
                .unwrap_or(usize::MAX);
                let limit =
                    usize::try_from(args.get("limit").and_then(Value::as_u64).unwrap_or(2_000))
                        .unwrap_or(usize::MAX);
                let start = offset.saturating_sub(1).min(lines.len());
                let end = start.saturating_add(limit).min(lines.len());
                let mut result = format!(
                    "<path>{}</path>\n<type>file</type>\n<content>\n",
                    path.display()
                );
                for (i, line) in lines[start..end].iter().enumerate() {
                    writeln!(result, "{}: {}", start + i + 1, truncate(line))
                        .expect("write string");
                }
                result.push('\n');
                if end == lines.len() {
                    writeln!(result, "(End of file - total {} lines)", lines.len())
                        .expect("write string");
                } else {
                    writeln!(
                        result,
                        "(Showing lines {offset}-{end} of {}. Use offset={} to continue.)\n",
                        lines.len(),
                        end + 1
                    )
                    .expect("write string");
                }
                result.push_str("</content>");
                Ok(result)
            }
            "glob" | "grep" | "search_files" => {
                let base = path_for(
                    args.get("path").and_then(Value::as_str).unwrap_or("."),
                    false,
                )?;
                let pattern = str_arg(args, "pattern")?;
                let query = (tool == "search_files").then(|| pattern.to_lowercase());
                let glob = if tool == "glob" {
                    Some(glob_pattern(pattern)?)
                } else {
                    None
                };
                let include = args
                    .get("include")
                    .and_then(Value::as_str)
                    .map(glob_pattern)
                    .transpose()?;
                let mut results = BTreeSet::new();
                let mut ranked = BTreeSet::new();
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(MAX_RESULTS)
                    .min(MAX_RESULTS);
                let mut truncated = false;
                let mut walks = vec![WalkBuilder::new(&base).standard_filters(true).build()];
                // A second pass visits only skill subtrees, including nested hidden
                // .agents directories that the ordinary ignore walk cannot enter.
                let walk_base = base.clone();
                let base_in_skills = base
                    .components()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .any(|pair| {
                        pair[0].as_os_str() == ".agents" && pair[1].as_os_str() == "skills"
                    });
                walks.push(
                    WalkBuilder::new(&base)
                        .standard_filters(false)
                        .filter_entry(move |entry| {
                            let relative = entry
                                .path()
                                .strip_prefix(&walk_base)
                                .unwrap_or_else(|_| entry.path());
                            let parts: Vec<_> = relative.components().collect();
                            let in_skills = base_in_skills
                                || parts.windows(2).any(|pair| {
                                    pair[0].as_os_str() == ".agents"
                                        && pair[1].as_os_str() == "skills"
                                });
                            in_skills
                                || (entry.file_type().is_some_and(|kind| kind.is_dir())
                                    && (entry.depth() == 0
                                        || entry.file_name() == ".agents"
                                        || !entry.file_name().to_string_lossy().starts_with('.')))
                        })
                        .build(),
                );
                for walk in walks {
                    for entry in walk {
                        if cancel.is_cancelled() {
                            return Err(SandboxError("canceled".into()));
                        }
                        let entry = entry.map_err(|e| SandboxError(e.to_string()))?;
                        if !entry.file_type().is_some_and(|t| t.is_file()) {
                            continue;
                        }
                        let path = entry.path();
                        // Ignore symlinked files that leave the workspace, including
                        // explicit paths resolved before the walk.
                        if restricted && !path.canonicalize()?.starts_with(root) {
                            continue;
                        }
                        let relative = path.strip_prefix(&base).unwrap_or(path);
                        if tool == "search_files" {
                            let detail = relative.to_string_lossy().replace('\\', "/");
                            if let Some(score) = file_score(&detail, query.as_deref().unwrap_or(""))
                            {
                                ranked.insert((score, path.to_path_buf()));
                                if ranked.len() > limit {
                                    ranked.pop_last();
                                    truncated = true;
                                }
                            }
                        } else if tool == "glob" {
                            if glob.as_ref().is_some_and(|g| g.is_match(relative)) {
                                insert_result(
                                    &mut results,
                                    &mut truncated,
                                    (path.to_path_buf(), 0, String::new()),
                                );
                            }
                        } else if include.as_ref().is_none_or(|g| {
                            g.is_match(if base.is_file() {
                                path.file_name().map_or(relative, Path::new)
                            } else {
                                relative
                            })
                        }) && let Ok(file) = fs::File::open(path)
                        {
                            for (i, line) in BufReader::new(file).lines().enumerate() {
                                if cancel.is_cancelled() {
                                    return Err(SandboxError("canceled".into()));
                                }
                                let Ok(line) = line else {
                                    break;
                                };
                                if line.contains(pattern) {
                                    insert_result(
                                        &mut results,
                                        &mut truncated,
                                        (path.to_path_buf(), i + 1, truncate(&line)),
                                    );
                                }
                            }
                        }
                    }
                }
                if tool == "search_files" {
                    if ranked.is_empty() && !truncated {
                        return Ok("No files found".into());
                    }
                    return Ok(bounded_paths(
                        ranked.iter().map(|(_, path)| path),
                        truncated,
                    ));
                }
                if results.is_empty() {
                    return Ok("No files found".into());
                }
                if tool == "glob" {
                    Ok(bounded_paths(
                        results.iter().map(|(path, _, _)| path),
                        truncated,
                    ))
                } else {
                    let mut output = format!(
                        "Found {} matches{}",
                        results.len(),
                        if truncated { " (truncated)" } else { "" }
                    );
                    let mut current = None;
                    for (path, line, text) in &results {
                        if current != Some(path) {
                            write!(output, "\n{}:", path.display()).expect("write string");
                            current = Some(path);
                        }
                        write!(output, "\n  Line {line}: {text}").expect("write string");
                    }
                    Ok(output)
                }
            }
            "write" => {
                check_cancel(cancel)?;
                let path = path_for(str_arg(args, "filePath")?, true)?;
                let content = str_arg(args, "content")?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Recheck after directory creation: preexisting symlinks may
                // redirect a parent outside the workspace.
                let path = path_for(str_arg(args, "filePath")?, true)?;
                check_cancel(cancel)?;
                fs::write(path, content)?;
                Ok("Wrote file successfully.".into())
            }
            "edit" => {
                let path = path_for(str_arg(args, "filePath")?, false)?;
                let old = str_arg(args, "oldString")?;
                let new = str_arg(args, "newString")?;
                if old == new {
                    return Err(SandboxError(
                        "oldString and newString must be different".into(),
                    ));
                }
                let content = fs::read_to_string(&path)?;
                let count = content.matches(old).count();
                if count == 0
                    || (count > 1 && args.get("replaceAll").and_then(Value::as_bool) != Some(true))
                {
                    return Err(SandboxError(format!(
                        "expected exactly one match for edit, found {count}"
                    )));
                }
                let updated = if args.get("replaceAll").and_then(Value::as_bool) == Some(true) {
                    content.replace(old, new)
                } else {
                    content.replacen(old, new, 1)
                };
                check_cancel(cancel)?;
                fs::write(path, updated)?;
                Ok("Edit applied successfully.".into())
            }
            "delete" => {
                let path = path_for(str_arg(args, "filePath")?, false)?;
                check_cancel(cancel)?;
                fs::remove_file(path)?;
                Ok("Deleted file successfully.".into())
            }
            _ => Err(SandboxError("unsupported tool".into())),
        }
    }

    #[cfg(feature = "shell")]
    async fn shell(
        &self,
        args: &Value,
        defaults: &ShellDefaults,
        cancel: CancellationToken,
        progress: Option<tokio::sync::mpsc::Sender<(String, String)>>,
    ) -> Result<Execution> {
        crate::shell::execute(&self.root, defaults, args, cancel, progress).await
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| SandboxError(format!("missing or invalid {key}")))
}

fn optional_string(args: &Value, key: &str) -> Result<()> {
    if args.get(key).is_some_and(|value| !value.is_string()) {
        return Err(SandboxError(format!("invalid {key}")));
    }
    Ok(())
}

fn optional_integer(args: &Value, key: &str, minimum: u64) -> Result<()> {
    if args
        .get(key)
        .is_some_and(|value| value.as_u64().is_none_or(|n| n < minimum))
    {
        return Err(SandboxError(format!("invalid {key}")));
    }
    Ok(())
}

pub fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(SandboxError("canceled".into()))
    } else {
        Ok(())
    }
}

fn insert_result(
    results: &mut BTreeSet<(PathBuf, usize, String)>,
    truncated: &mut bool,
    item: (PathBuf, usize, String),
) {
    results.insert(item);
    if results.len() > MAX_RESULTS {
        results.pop_last();
        *truncated = true;
    }
}

fn bounded_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>, mut truncated: bool) -> String {
    let mut output = String::new();
    for path in paths {
        let path = path.to_string_lossy();
        if output.len() + path.len() + 1 + RESULTS_TRUNCATED.len() + 1 > MAX_LIST_BYTES {
            truncated = true;
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&path);
    }
    if truncated {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(RESULTS_TRUNCATED);
    }
    output
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn skill_path(name: &str) -> Result<String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains('/') || name.contains('\\') {
        return Err(SandboxError("invalid skill name".into()));
    }
    Ok(format!(".agents/skills/{name}/SKILL.md"))
}

pub fn scoped(root: &Path, raw: &str, writable: bool) -> Result<PathBuf> {
    let path = resolved(root, raw, writable)?;
    if !path.starts_with(root) {
        return Err(SandboxError("path escapes workspace root".into()));
    }
    Ok(path)
}

pub fn resolved(root: &Path, raw: &str, writable: bool) -> Result<PathBuf> {
    let raw = raw.trim();
    let raw = raw.split_once("<path>").map_or(raw, |(_, after)| {
        after.split_once("</path>").map_or(raw, |(path, _)| path)
    });
    let raw = raw
        .trim()
        .trim_matches(|c| matches!(c, '`' | '"' | '\''))
        .trim_end_matches(':');
    if raw.is_empty() {
        return Err(SandboxError("empty path".into()));
    }
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canonical = if writable {
        let mut ancestor = path.as_path();
        let mut suffix = Vec::new();
        while fs::symlink_metadata(ancestor).is_err() {
            suffix.push(
                ancestor
                    .file_name()
                    .ok_or_else(|| SandboxError("invalid path".into()))?
                    .to_os_string(),
            );
            ancestor = ancestor
                .parent()
                .ok_or_else(|| SandboxError("invalid path".into()))?;
        }
        let mut resolved = ancestor.canonicalize()?;
        for part in suffix.into_iter().rev() {
            if part == ".." || part == "." {
                return Err(SandboxError("invalid path component".into()));
            }
            resolved.push(part);
        }
        resolved
    } else {
        path.canonicalize()?
    };
    Ok(canonical)
}

fn truncate(line: &str) -> String {
    if line.len() <= 2_000 {
        return line.to_owned();
    }
    format!("{}...", &line[..line.floor_char_boundary(2_000)])
}

fn file_score(detail: &str, query: &str) -> Option<u8> {
    let detail = detail.to_lowercase();
    let label = detail.rsplit('/').next().unwrap_or(&detail);
    if label == query {
        Some(0)
    } else if label.starts_with(query) {
        Some(1)
    } else if detail.split('/').any(|part| part.starts_with(query)) {
        Some(2)
    } else if label.contains(query) {
        Some(3)
    } else if detail.contains(query) {
        Some(4)
    } else {
        None
    }
}

fn glob_pattern(pattern: &str) -> Result<GlobSet> {
    let glob = Glob::new(&pattern.replace('\\', "/")).map_err(|e| SandboxError(e.to_string()))?;
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    builder.build().map_err(|e| SandboxError(e.to_string()))
}

fn skill_metadata(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    let (mut name, mut description) = (None, None);
    for line in lines {
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(value)
            .trim();
        match key.trim() {
            "name" if valid_skill_name(value) => name = Some(value.to_owned()),
            "description" if !value.is_empty() => description = Some(value.to_owned()),
            _ => {}
        }
    }
    Some((name?, description?))
}

fn skill_catalog(root: &Path) -> Vec<(String, String, String)> {
    let Ok(entries) = fs::read_dir(root.join(".agents/skills")) else {
        return Vec::new();
    };
    let mut catalog = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let skill = entry.path().join("SKILL.md");
        if scoped(root, &skill.display().to_string(), false).is_err() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(skill)
            && let Some((name, description)) = skill_metadata(&text)
        {
            catalog.push((
                entry.file_name().to_string_lossy().into_owned(),
                name,
                description,
            ));
        }
    }
    catalog.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn ticket_is_bound_single_use_and_prepare_is_pure() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let args = json!({"filePath":"nested/note.txt", "content":"hello"}).to_string();
        let prepared = sandbox.prepare("1", "write", &args).unwrap();
        assert!(!dir.path().join("nested").exists());
        assert_eq!(prepared.claims[0].operation, "write");
        assert!(
            sandbox
                .execute("2", &prepared.ticket, CancellationToken::new())
                .await
                .is_err()
        );
        sandbox
            .execute("1", &prepared.ticket, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("nested/note.txt")).unwrap(),
            "hello"
        );
        assert!(
            sandbox
                .execute("1", &prepared.ticket, CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_escape_and_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        assert!(
            sandbox
                .prepare(
                    "outside",
                    "write",
                    &json!({"filePath":"../bad", "content":"x"}).to_string()
                )
                .unwrap()
                .claims[0]
                .resource
                .ends_with("/bad")
        );
        assert!(
            sandbox
                .workspace("read", &json!({"filePath":"../bad"}))
                .await
                .is_err()
        );
        let prepared = sandbox
            .prepare(
                "1",
                "write",
                &json!({"filePath":"ok", "content":"x"}).to_string(),
            )
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            sandbox
                .execute("1", &prepared.ticket, cancel)
                .await
                .is_err()
        );
        assert!(!dir.path().join("ok").exists());
    }

    #[tokio::test]
    async fn workspace_reads_searches_and_lists() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), "hello\n").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        assert!(
            sandbox
                .workspace("read", &json!({"filePath":"note.txt"}))
                .await
                .unwrap()
                .content
                .eq("hello\n")
        );
        assert_eq!(
            sandbox
                .workspace("list", &json!({"pattern":"*.txt"}))
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(
            sandbox
                .workspace("search", &json!({"pattern":"hello"}))
                .await
                .unwrap()
                .content
                .contains("Found 1 matches")
        );
    }

    #[tokio::test]
    async fn snapshot_is_relative_sorted_ignored_and_bounded_before_reply() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep/too-deep")).unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "ignore me").unwrap();
        fs::write(dir.path().join(".hidden"), "ignore me").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        fs::write(dir.path().join("src/deep/too-deep/file.rs"), "").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();

        let full = sandbox.snapshot(".", 200, 2, 16 * 1024).await.unwrap();
        assert_eq!(
            full.content,
            "- Cargo.toml\n- src/\n- src/deep/\n- src/lib.rs"
        );
        assert_eq!(full.entries.len(), 4);
        let subdir = sandbox.snapshot("src", 200, 1, 16 * 1024).await.unwrap();
        assert_eq!(subdir.content, "- deep/\n- lib.rs");
        assert!(sandbox.snapshot("..", 200, 4, 1024).await.is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().parent().unwrap(), dir.path().join("outside"))
                .unwrap();
            assert!(sandbox.snapshot("outside", 200, 4, 1024).await.is_err());
            assert!(
                !sandbox
                    .snapshot(".", 200, 4, 1024)
                    .await
                    .unwrap()
                    .content
                    .contains("outside")
            );
        }

        let limited = sandbox.snapshot(".", 2, 4, 1024).await.unwrap();
        assert_eq!(limited.entries.len(), 2);
        assert!(limited.content.ends_with("- <snapshot truncated>"));
        assert!(limited.entries.windows(2).all(|pair| pair[0] < pair[1]));
        let bytes = sandbox.snapshot(".", 200, 4, 40).await.unwrap();
        assert!(bytes.content.ends_with("- <snapshot truncated>"));
        assert!(bytes.content.len() <= 40);
        let tiny = sandbox.snapshot(".", 200, 4, 0).await.unwrap();
        assert!(tiny.content.is_empty());
        assert!(tiny.entries.is_empty());
        let none = sandbox.snapshot(".", 0, 4, 1024).await.unwrap();
        assert_eq!(none.content, "- <snapshot truncated>");
        assert!(
            sandbox
                .snapshot(".", 200, 0, 1024)
                .await
                .unwrap()
                .content
                .is_empty()
        );
    }

    #[tokio::test]
    async fn snapshot_defaults_cap_large_workspaces_without_a_glob() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..250 {
            fs::write(dir.path().join(format!("file-{index:04}")), "").unwrap();
        }
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let snapshot = sandbox.snapshot(".", 200, 4, 16 * 1024).await.unwrap();
        assert_eq!(snapshot.entries.len(), 200);
        assert!(snapshot.content.ends_with("- <snapshot truncated>"));
        assert!(snapshot.content.len() <= 16 * 1024);
        let capped = sandbox
            .snapshot(".", usize::MAX, usize::MAX, usize::MAX)
            .await
            .unwrap();
        assert_eq!(capped.entries.len(), 250);
        assert!(capped.content.len() <= MAX_SNAPSHOT_BYTES);
    }

    #[tokio::test]
    async fn snapshot_selects_sorted_siblings_before_entry_and_byte_limits() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("z-last"), "").unwrap();
        fs::write(dir.path().join("a-first"), "").unwrap();
        fs::write(dir.path().join("b-next"), "").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        assert_eq!(
            sandbox.snapshot(".", 1, 1, 1024).await.unwrap().entries,
            ["a-first"]
        );
        assert_eq!(
            sandbox.snapshot(".", 10, 1, 18).await.unwrap().entries,
            ["a-first"]
        );
    }

    #[tokio::test]
    async fn nested_hidden_skills_and_snapshot_glob_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".agents/skills/demo");
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("SKILL.md"), "instructions").unwrap();
        let nested = dir.path().join("packages/app/.agents/skills/other");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("SKILL.md"), "nested instructions").unwrap();
        for i in 0..(MAX_RESULTS + 20) {
            fs::write(dir.path().join(format!("file-{i:04}.txt")), "match\n").unwrap();
        }
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let skill = sandbox
            .workspace("glob", &json!({"pattern":"**/SKILL.md"}))
            .await
            .unwrap();
        assert_eq!(skill.entries.len(), 2);
        assert!(
            skill.entries.contains(
                &nested
                    .join("SKILL.md")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string()
            ),
            "{:?}",
            skill.entries
        );
        assert_eq!(
            sandbox
                .workspace("glob", &json!({"path":"packages", "pattern":"**/SKILL.md"}))
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(
            sandbox
                .workspace("grep", &json!({"pattern":"nested instructions"}))
                .await
                .unwrap()
                .content
                .contains("other/SKILL.md")
        );
        let snapshot = sandbox
            .workspace("list", &json!({"pattern":"**/*"}))
            .await
            .unwrap();
        assert_eq!(snapshot.entries.len(), MAX_RESULTS);
        assert!(snapshot.content.ends_with(RESULTS_TRUNCATED));
        assert!(snapshot.entries.windows(2).all(|pair| pair[0] < pair[1]));
        let prepared = sandbox
            .prepare("glob-many", "glob", &json!({"pattern":"**/*"}).to_string())
            .unwrap();
        let tool_output = sandbox
            .execute("glob-many", &prepared.ticket, CancellationToken::new())
            .await
            .unwrap();
        assert!(tool_output.content.ends_with(RESULTS_TRUNCATED));
        let search = sandbox
            .workspace("search", &json!({"pattern":"match"}))
            .await
            .unwrap();
        assert!(search.content.starts_with("Found 1024 matches (truncated)"));
        let filtered = sandbox
            .workspace("search_files", &json!({"pattern":"file-1043", "limit": 3}))
            .await
            .unwrap();
        assert_eq!(filtered.entries.len(), 1);
        assert!(filtered.entries[0].ends_with("file-1043.txt"));
        let limited = sandbox
            .workspace("search_files", &json!({"pattern":"file-", "limit": 2}))
            .await
            .unwrap();
        assert_eq!(limited.entries.len(), 2);
        assert!(limited.content.ends_with(RESULTS_TRUNCATED));
    }

    #[tokio::test]
    async fn formatted_context_read_is_bounded_but_instruction_read_is_raw() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "line\n".repeat(2002)).unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let args = json!({"filePath":"AGENTS.md"});
        let formatted = sandbox
            .workspace("formatted_read", &args)
            .await
            .unwrap()
            .content;
        assert!(formatted.contains("<path>"));
        assert!(formatted.contains("2000: line"));
        assert!(!formatted.contains("2001: line"));
        assert!(formatted.contains("Use offset=2001 to continue."));
        assert_eq!(
            sandbox.workspace("read", &args).await.unwrap().content,
            "line\n".repeat(2002)
        );
    }

    #[test]
    fn expired_tickets_are_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox
            .prepare(
                "old",
                "write",
                &json!({"filePath":"new", "content":"x"}).to_string(),
            )
            .unwrap();
        sandbox
            .tickets
            .lock()
            .unwrap()
            .get_mut(&prepared.ticket)
            .unwrap()
            .created = Instant::now().checked_sub(TICKET_TTL).unwrap();
        assert!(
            sandbox
                .prepare(
                    "old",
                    "write",
                    &json!({"filePath":"new", "content":"x"}).to_string()
                )
                .is_ok()
        );
    }

    #[test]
    fn canceled_blocking_mutation_stops_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            Sandbox::filesystem(
                dir.path(),
                "write",
                &json!({"filePath":"nested/note", "content":"x"}),
                false,
                &token
            )
            .is_err()
        );
        assert!(!dir.path().join("nested").exists());
        let patch = "*** Begin Patch\n*** Add File: note\n+x\n*** End Patch";
        assert!(crate::patch::apply(dir.path(), patch, &token).is_err());
        assert!(!dir.path().join("note").exists());
    }

    #[tokio::test]
    async fn skill_is_worker_owned_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".agents/skills/example");
        fs::create_dir_all(&skills).unwrap();
        fs::write(
            skills.join("SKILL.md"),
            "---\nname: example\ndescription: Use example.\n---\nInstructions",
        )
        .unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox
            .prepare("skill-1", "skill", &json!({"name":"example"}).to_string())
            .unwrap();
        assert!(prepared.claims[0].resource.ends_with("SKILL.md"));
        assert!(
            sandbox
                .execute("skill-1", &prepared.ticket, CancellationToken::new())
                .await
                .unwrap()
                .content
                .contains("Instructions")
        );
        assert!(
            sandbox
                .prepare(
                    "skill-2",
                    "skill",
                    &json!({"name":"../example"}).to_string()
                )
                .is_err()
        );
        assert_eq!(
            sandbox
                .workspace("skills", &json!({}))
                .await
                .unwrap()
                .entries,
            vec!["example"]
        );
        let alternate = dir.path().join(".agents/skills/example_dir");
        fs::create_dir(&alternate).unwrap();
        fs::write(
            alternate.join("SKILL.md"),
            "---\nname: alternate\ndescription: An alternate.\n---\nContent",
        )
        .unwrap();
        let catalog = sandbox.workspace("skills", &json!({})).await.unwrap();
        assert_eq!(catalog.entries, vec!["example_dir", "example"]);
        assert!(catalog.content.contains("alternate: An alternate."));
        assert!(
            sandbox
                .workspace("skill", &json!({"name":"example_dir"}))
                .await
                .unwrap()
                .content
                .contains("Content")
        );
    }

    #[tokio::test]
    async fn patch_prepares_every_path_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("existing"), "old\n").unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let patch = "*** Begin Patch\n*** Update File: existing\n@@\n-old\n+new\n*** Add File: added\n+created\n*** End Patch";
        let prepared = sandbox
            .prepare(
                "patch-1",
                "apply_patch",
                &json!({"patchText": patch}).to_string(),
            )
            .unwrap();
        assert_eq!(prepared.claims.len(), 2);
        assert!(!dir.path().join("added").exists());
        sandbox
            .execute("patch-1", &prepared.ticket, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("existing")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("added")).unwrap(),
            "created\n"
        );
        assert_eq!(sandbox.prepare("patch-2", "apply_patch", &json!({"patchText": "*** Begin Patch\n*** Add File: ../escape\n+no\n*** End Patch"}).to_string()).unwrap().claims[0].operation, "write");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("outside")).unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        assert!(
            sandbox
                .prepare(
                    "1",
                    "write",
                    &json!({"filePath":"outside/file", "content":"x"}).to_string()
                )
                .unwrap()
                .claims[0]
                .resource
                .starts_with(&outside.path().canonicalize().unwrap().display().to_string())
        );
        assert!(
            sandbox
                .workspace("read", &json!({"filePath":"outside"}))
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_redirected_path_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox
            .prepare(
                "write",
                "write",
                &json!({"filePath":"parent/note", "content":"safe"}).to_string(),
            )
            .unwrap();
        fs::remove_dir(&parent).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        assert!(
            sandbox
                .execute("write", &prepared.ticket, CancellationToken::new())
                .await
                .is_err()
        );
        assert!(!outside.path().join("note").exists());
    }

    #[cfg(all(feature = "shell", unix))]
    #[tokio::test]
    async fn shell_claims_symlinks_and_dynamic_cd_without_in_root_guess() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox.prepare("shell", "shell", &json!({"command":"cd link; cat link/file; cd $NEXT", "description":"test", "workdir":"link"}).to_string()).unwrap();
        assert!(
            prepared
                .claims
                .iter()
                .any(|c| c.resource_kind == "working_directory"
                    && c.resource == outside.path().canonicalize().unwrap().display().to_string())
        );
        assert!(prepared.claims.iter().any(|c| {
            c.resource_kind == "filesystem_path"
                && c.resource
                    .starts_with(&outside.path().canonicalize().unwrap().display().to_string())
        }));
        assert!(
            prepared
                .claims
                .iter()
                .any(|c| c.resource_kind == "shell_command" && c.resource == "cd $NEXT")
        );
        let traversal = sandbox
            .prepare(
                "traversal",
                "shell",
                &json!({"command":"cat link/../missing", "description":"test"}).to_string(),
            )
            .unwrap();
        assert!(traversal.claims.iter().any(|c| {
            c.resource_kind == "filesystem_path"
                && c.resource
                    == outside
                        .path()
                        .parent()
                        .unwrap()
                        .canonicalize()
                        .unwrap()
                        .join("missing")
                        .display()
                        .to_string()
        }));
        std::os::unix::fs::symlink(outside.path().join("new-file"), dir.path().join("dangling"))
            .unwrap();
        let dangling = sandbox
            .prepare(
                "dangling",
                "shell",
                &json!({"command":"printf x > dangling", "description":"test"}).to_string(),
            )
            .unwrap();
        assert!(dangling.claims.iter().any(|c| {
            c.resource_kind == "filesystem_path"
                && c.resource
                    == outside
                        .path()
                        .canonicalize()
                        .unwrap()
                        .join("new-file")
                        .display()
                        .to_string()
        }));
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_ticket_preserves_defaults_for_claims_and_execution() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        fs::create_dir(dir.path().join("default")).unwrap();
        fs::create_dir(dir.path().join("other")).unwrap();
        let defaults = ShellDefaults {
            default_workdir: PathBuf::from("default"),
            default_timeout: Duration::from_millis(500),
            max_output_bytes: 256,
        };
        let args =
            json!({"command":"pwd; printf 'hello' > ./note; sleep 2", "description":"defaults"});
        let prepared = sandbox
            .prepare_with_shell_defaults(
                "custom",
                "shell",
                &args.to_string(),
                Some(defaults.clone()),
            )
            .unwrap();
        let expected = sandbox.root().join("default");
        assert!(
            prepared
                .claims
                .iter()
                .any(|c| c.resource_kind == "working_directory"
                    && c.resource == expected.display().to_string())
        );
        assert!(
            prepared
                .claims
                .iter()
                .any(|c| c.resource_kind == "filesystem_path"
                    && c.resource == expected.join("note").display().to_string())
        );
        let output = sandbox
            .execute("custom", &prepared.ticket, CancellationToken::new())
            .await
            .unwrap()
            .content;
        assert!(output.contains("Timed out: true"), "{output}");
        assert!(output.contains(&expected.display().to_string()), "{output}");
        assert_eq!(fs::read_to_string(expected.join("note")).unwrap(), "hello");

        let override_args = json!({"command":"pwd; printf 'x' > ./override; printf '%0200d' 1", "description":"override", "workdir":"other", "timeout":2});
        let prepared = sandbox
            .prepare_with_shell_defaults(
                "override",
                "shell",
                &override_args.to_string(),
                Some(defaults),
            )
            .unwrap();
        let other = sandbox.root().join("other");
        assert!(
            prepared
                .claims
                .iter()
                .any(|c| c.resource_kind == "working_directory"
                    && c.resource == other.display().to_string())
        );
        assert!(
            prepared
                .claims
                .iter()
                .any(|c| c.resource_kind == "filesystem_path"
                    && c.resource == other.join("override").display().to_string())
        );
        let output = sandbox
            .execute("override", &prepared.ticket, CancellationToken::new())
            .await
            .unwrap()
            .content;
        assert!(output.contains(&other.display().to_string()), "{output}");
        assert!(output.contains("[output truncated]"), "{output}");
        assert!(!output.contains("Timed out: true"), "{output}");
        assert_eq!(fs::read_to_string(other.join("override")).unwrap(), "x");
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_caps_output_and_does_not_wait_for_background_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let output = run(
            &sandbox,
            "large",
            "shell",
            json!({"command":"yes x | head -c 100000", "description":"large"}),
        )
        .await
        .unwrap();
        assert!(output.len() <= 20 * 1024 + 64);
        let prepared = sandbox
            .prepare(
                "background",
                "shell",
                &json!({"command":"printf hello; sleep 5 &", "description":"background"})
                    .to_string(),
            )
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            sandbox.execute("background", &prepared.ticket, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.content.contains("hello"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_dangling_symlink_write() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let target = outside.path().join("not-created");
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        assert!(
            sandbox
                .prepare(
                    "write",
                    "write",
                    &json!({"filePath":"link", "content":"bad"}).to_string()
                )
                .is_err()
        );
        assert!(!target.exists());
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_is_opt_in_and_ticket_bound() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox
            .prepare(
                "shell-1",
                "shell",
                &json!({"command":"printf sandbox", "description":"test output"}).to_string(),
            )
            .unwrap();
        assert_eq!(prepared.claims[0].resource_kind, "working_directory");
        assert_eq!(prepared.claims[1].resource_kind, "shell_command");
        assert!(
            sandbox
                .execute("shell-1", &prepared.ticket, CancellationToken::new())
                .await
                .unwrap()
                .content
                .contains("sandbox")
        );
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_emits_progress_and_limits_captured_output() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox
            .prepare(
                "shell-2",
                "shell",
                &json!({"command":"printf hello; printf warning >&2", "description":"test progress"}).to_string(),
            )
            .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        sandbox
            .execute_with_progress(
                "shell-2",
                &prepared.ticket,
                CancellationToken::new(),
                Some(sender),
            )
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|(level, text)| level == "info" && text == "hello")
        );
        assert!(
            events
                .iter()
                .any(|(level, text)| level == "error" && text == "warning")
        );
    }

    async fn run(sandbox: &Sandbox, id: &str, tool: &str, args: Value) -> Result<String> {
        let prepared = sandbox.prepare(id, tool, &args.to_string())?;
        Ok(sandbox
            .execute(id, &prepared.ticket, CancellationToken::new())
            .await?
            .content)
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn filesystem_outputs_and_outside_root_claims_match_agent_tools() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        fs::write(dir.path().join("note.txt"), "one\ntwo\none\n").unwrap();
        assert_eq!(
            run(
                &sandbox,
                "read",
                "read",
                json!({"filePath":"note.txt", "offset":2,"limit":1})
            )
            .await
            .unwrap(),
            format!(
                "<path>{}</path>\n<type>file</type>\n<content>\n2: two\n\n(Showing lines 2-2 of 3. Use offset=3 to continue.)\n\n</content>",
                dir.path()
                    .join("note.txt")
                    .canonicalize()
                    .unwrap()
                    .display()
            )
        );
        assert_eq!(
            run(
                &sandbox,
                "grep",
                "grep",
                json!({"pattern":"one", "include":"*.txt"})
            )
            .await
            .unwrap(),
            format!(
                "Found 2 matches\n{}:\n  Line 1: one\n  Line 3: one",
                dir.path()
                    .join("note.txt")
                    .canonicalize()
                    .unwrap()
                    .display()
            )
        );
        assert_eq!(
            run(&sandbox, "glob", "glob", json!({"pattern":"*.txt"}))
                .await
                .unwrap(),
            dir.path()
                .join("note.txt")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        assert!(
            run(
                &sandbox,
                "edit-fails",
                "edit",
                json!({"filePath":"note.txt", "oldString":"one", "newString":"1"})
            )
            .await
            .is_err()
        );
        assert_eq!(run(&sandbox, "edit", "edit", json!({"filePath":"note.txt", "oldString":"one", "newString":"1", "replaceAll":true})).await.unwrap(), "Edit applied successfully.");
        let target = outside.path().join("new.txt");
        let prep = sandbox
            .prepare(
                "outside",
                "write",
                &json!({"filePath": target, "content":"outside"}).to_string(),
            )
            .unwrap();
        assert_eq!(
            prep.claims[0].resource,
            outside
                .path()
                .canonicalize()
                .unwrap()
                .join("new.txt")
                .display()
                .to_string()
        );
        sandbox
            .execute("outside", &prep.ticket, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "outside");
        assert!(
            sandbox
                .workspace("read", &json!({"filePath":target}))
                .await
                .is_err()
        );
        assert_eq!(
            run(&sandbox, "delete", "delete", json!({"filePath":target}))
                .await
                .unwrap(),
            "Deleted file successfully."
        );
    }

    #[tokio::test]
    async fn patch_accepts_both_path_forms_and_insertion_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        fs::write(dir.path().join("original"), "first\nlast\n").unwrap();
        fs::write(dir.path().join("obsolete"), "old").unwrap();
        let patch = "*** Begin Patch\n*** Update File\nFile: original\n@@ first\n+middle\n@@\n-last\n+end\n*** Add File\nFile: fresh\n+new\n*** Delete File: obsolete\n*** End Patch";
        let prepared = sandbox
            .prepare(
                "patch",
                "apply_patch",
                &json!({"patchText":patch}).to_string(),
            )
            .unwrap();
        assert_eq!(
            prepared
                .claims
                .iter()
                .map(|c| c.operation.as_str())
                .collect::<Vec<_>>(),
            ["write", "write", "delete"]
        );
        assert_eq!(
            sandbox
                .execute("patch", &prepared.ticket, CancellationToken::new())
                .await
                .unwrap()
                .content,
            "Success. Updated the following files:\nM original\nA fresh\nD obsolete"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("original")).unwrap(),
            "first\nmiddle\nend\n"
        );
    }

    #[cfg(feature = "shell")]
    #[tokio::test]
    async fn shell_scans_compound_commands_and_reports_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let command = "cd sub; cat ./note | rg word; printf hello > output";
        let prepared = sandbox
            .prepare(
                "scan",
                "shell",
                &json!({"command":command, "description":"scan", "workdir":dir.path()}).to_string(),
            )
            .unwrap();
        let claims = prepared.claims;
        assert!(
            claims
                .iter()
                .any(|c| c.resource_kind == "working_directory" && c.resource.ends_with("/sub"))
        );
        assert!(
            claims
                .iter()
                .any(|c| c.operation == "read" && c.resource.ends_with("/note"))
        );
        assert!(
            claims
                .iter()
                .any(|c| c.operation == "write" && c.resource.ends_with("/output"))
        );
        assert!(
            claims
                .iter()
                .any(|c| c.operation == "execute" && c.resource == "cat ./note")
        );
        let result = run(
            &sandbox,
            "timeout",
            "shell",
            json!({"command":"sleep 2", "description":"timeout", "timeout":1}),
        )
        .await
        .unwrap();
        assert!(
            result.starts_with("Timed out: true\nExit code:"),
            "{result}"
        );
    }

    #[cfg(all(feature = "shell", unix))]
    #[tokio::test]
    async fn shell_cancellation_terminates_child_processes() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let prepared = sandbox
            .prepare(
                "cancel-shell",
                "shell",
                &json!({"command":"sleep 10", "description":"test cancellation"}).to_string(),
            )
            .unwrap();
        let token = CancellationToken::new();
        let future = sandbox.execute("cancel-shell", &prepared.ticket, token.clone());
        let trigger = async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            token.cancel();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(future, trigger)
        })
        .await
        .unwrap();
        assert!(result.unwrap().content.starts_with("Exit code:"));
    }
}
