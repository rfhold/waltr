//! Session template search for prompt template references.

use std::{cmp::Ordering, path::Path};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::AgentRuntime;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

/// Search request for session-scoped prompt templates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTemplateSearchRequest {
    /// User-entered query after `@`.
    pub query: String,
    /// Maximum result count. Zero uses a default limit.
    pub limit: usize,
    /// Template kinds to include. Empty searches all supported kinds.
    pub kinds: Vec<SessionTemplateKind>,
    /// Search target. Resource-backed templates are available today; prompt-backed templates are reserved.
    pub target: SessionTemplateSearchTarget,
}

impl SessionTemplateSearchRequest {
    /// Effective bounded result limit.
    #[must_use]
    pub const fn effective_limit(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_LIMIT
        } else if self.limit > MAX_LIMIT {
            MAX_LIMIT
        } else {
            self.limit
        }
    }

    fn includes(&self, kind: SessionTemplateKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// Session template search target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTemplateSearchTarget {
    /// File and tool references available to the session runtime.
    Resources,
    /// Prompt templates such as skills or MCP prompts.
    Prompts,
}

impl Default for SessionTemplateSearchTarget {
    fn default() -> Self {
        Self::Resources
    }
}

/// Session template kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTemplateKind {
    /// Workspace file reference.
    File,
    /// Agent tool reference, including MCP-backed tools.
    Tool,
}

/// Template option suitable for insertion into a prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTemplateOption {
    /// Template kind.
    pub kind: SessionTemplateKind,
    /// Short display label.
    pub label: String,
    /// Longer display detail.
    pub detail: String,
    /// Full prompt template reference to insert.
    pub template: String,
    /// Template-specific metadata for richer UIs.
    pub metadata: Value,
}

#[derive(Debug)]
struct ScoredTemplateOption {
    score: u8,
    option: SessionTemplateOption,
}

/// Search files and tools visible to a session runtime.
#[must_use]
pub fn search_session_templates(
    root: &Path,
    runtime: &AgentRuntime,
    request: &SessionTemplateSearchRequest,
) -> Vec<SessionTemplateOption> {
    if request.target == SessionTemplateSearchTarget::Prompts {
        return Vec::new();
    }

    let query = request.query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let limit = request.effective_limit();
    let mut results = Vec::new();
    if request.includes(SessionTemplateKind::File) {
        results.extend(search_files(root, query, limit));
    }
    if request.includes(SessionTemplateKind::Tool) {
        results.extend(search_tools(runtime, query, limit));
    }

    results.sort_by(compare_options);
    results
        .into_iter()
        .take(limit)
        .map(|result| result.option)
        .collect()
}

fn search_files(root: &Path, query: &str, limit: usize) -> Vec<ScoredTemplateOption> {
    if !root.is_dir() {
        return Vec::new();
    }

    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for entry in WalkBuilder::new(root)
        .standard_filters(true)
        .build()
        .filter_map(Result::ok)
    {
        let file_type = entry.file_type();
        if !file_type.is_some_and(|file_type| file_type.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let detail = normalize_path(relative);
        let label = relative
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(detail.as_str())
            .to_owned();
        let Some(score) = file_score(&label, &detail, &query_lower) else {
            continue;
        };
        push_limited(
            &mut results,
            limit,
            ScoredTemplateOption {
                score,
                option: SessionTemplateOption {
                    kind: SessionTemplateKind::File,
                    label,
                    detail: detail.clone(),
                    template: file_template(&detail),
                    metadata: json!({ "path": detail }),
                },
            },
        );
    }

    results
}

fn search_tools(runtime: &AgentRuntime, query: &str, limit: usize) -> Vec<ScoredTemplateOption> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    for tool in runtime.tools() {
        let definition = tool.definition();
        let Some(score) = tool_score(&definition.name, &definition.description, &query_lower)
        else {
            continue;
        };
        let template = tool_template(&definition.name, &definition.input_schema);
        push_limited(
            &mut results,
            limit,
            ScoredTemplateOption {
                score,
                option: SessionTemplateOption {
                    kind: SessionTemplateKind::Tool,
                    label: definition.name.clone(),
                    detail: definition.description.clone(),
                    template,
                    metadata: json!({
                        "name": definition.name.clone(),
                        "description": definition.description.clone(),
                        "inputSchema": definition.input_schema.clone(),
                    }),
                },
            },
        );
    }
    results
}

fn tool_template(name: &str, input_schema: &Value) -> String {
    let mut arguments = Map::new();
    if let Some(required) = input_schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            arguments.insert(field.to_owned(), Value::String(String::new()));
        }
    }

    let arguments = Value::Object(arguments);
    let arguments = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_owned());
    format!("@tool({name}, {arguments})")
}

fn push_limited(
    results: &mut Vec<ScoredTemplateOption>,
    limit: usize,
    option: ScoredTemplateOption,
) {
    results.push(option);
    results.sort_by(compare_options);
    results.truncate(limit);
}

fn file_score(label: &str, detail: &str, query_lower: &str) -> Option<u8> {
    let label_lower = label.to_lowercase();
    let detail_lower = detail.to_lowercase();
    if label_lower == query_lower {
        Some(0)
    } else if label_lower.starts_with(query_lower) {
        Some(1)
    } else if detail_lower
        .split('/')
        .any(|component| component.starts_with(query_lower))
    {
        Some(2)
    } else if label_lower.contains(query_lower) {
        Some(3)
    } else if detail_lower.contains(query_lower) {
        Some(4)
    } else {
        None
    }
}

fn tool_score(name: &str, description: &str, query_lower: &str) -> Option<u8> {
    let name_lower = name.to_lowercase();
    let description_lower = description.to_lowercase();
    if name_lower == query_lower {
        Some(0)
    } else if name_lower.starts_with(query_lower) {
        Some(1)
    } else if name_lower.contains(query_lower) {
        Some(2)
    } else if description_lower.contains(query_lower) {
        Some(5)
    } else {
        None
    }
}

fn compare_options(left: &ScoredTemplateOption, right: &ScoredTemplateOption) -> Ordering {
    left.score
        .cmp(&right.score)
        .then_with(|| left.option.kind.cmp(&right.option.kind))
        .then_with(|| left.option.detail.cmp(&right.option.detail))
        .then_with(|| left.option.label.cmp(&right.option.label))
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn file_template(path: &str) -> String {
    if is_bare_file_ref(path) {
        format!("@{path}")
    } else {
        format!("@\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn is_bare_file_ref(path: &str) -> bool {
    !path.is_empty()
        && (path.contains('/') || path.contains('.'))
        && path.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '-' | '.')
        })
}

#[cfg(test)]
mod tests {
    use super::{file_template, is_bare_file_ref};

    #[test]
    fn file_templates_quote_when_needed() {
        assert_eq!(file_template("src/main.rs"), "@src/main.rs");
        assert_eq!(
            file_template("notes/two words.md"),
            "@\"notes/two words.md\""
        );
        assert_eq!(file_template("notes/quote\".md"), "@\"notes/quote\\\".md\"");
    }

    #[test]
    fn bare_file_refs_allow_path_safe_chars() {
        assert!(is_bare_file_ref("src/lib-test_1.rs"));
        assert!(!is_bare_file_ref("src/two words.rs"));
        assert!(!is_bare_file_ref(""));
    }
}
