//! Session template search for prompt template references.

use std::{cmp::Ordering, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    AgentRuntime, Result,
    middleware::context::{MiddlewareStateLog, TemplateSearchContext},
};

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
    /// Search target. Resources cover file/tool templates; prompts cover skill and future prompt templates.
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

    pub(crate) fn includes(&self, kind: SessionTemplateKind) -> bool {
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
    /// Prompt reference, such as an Agent Skill or MCP prompt.
    Prompt,
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

/// Mutable scored template-search results collected by middleware.
#[derive(Debug, Default)]
pub(crate) struct TemplateSearchResults {
    results: Vec<ScoredTemplateOption>,
}

impl TemplateSearchResults {
    /// Add one scored option.
    pub(crate) fn push(&mut self, score: u8, option: SessionTemplateOption) {
        self.results.push(ScoredTemplateOption { score, option });
    }

    /// Add one scored option while keeping only the best known matches.
    pub(crate) fn push_limited(&mut self, score: u8, option: SessionTemplateOption, limit: usize) {
        self.push(score, option);
        self.results.sort_by(compare_options);
        self.results.truncate(limit);
    }

    fn finish(mut self, limit: usize) -> Vec<SessionTemplateOption> {
        self.results.sort_by(compare_options);
        self.results
            .into_iter()
            .take(limit)
            .map(|result| result.option)
            .collect()
    }
}

/// Search prompt-template resources available to a session runtime.
pub(crate) async fn search_session_templates_with_state(
    root: &Path,
    runtime: &AgentRuntime,
    request: &SessionTemplateSearchRequest,
    middleware_state: Option<&MiddlewareStateLog>,
) -> Result<Vec<SessionTemplateOption>> {
    let limit = request.effective_limit();
    let mut results = TemplateSearchResults::default();

    if request.target == SessionTemplateSearchTarget::Resources && request.query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let ctx = TemplateSearchContext {
        root,
        request,
        middleware_state,
    };
    runtime
        .middleware_chain()
        .search_templates(&ctx, &mut results)
        .await?;

    Ok(results.finish(limit))
}

pub(crate) fn tool_template(name: &str, input_schema: &Value) -> String {
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

pub(crate) fn file_score(label: &str, detail: &str, query_lower: &str) -> Option<u8> {
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

pub(crate) fn tool_score(name: &str, description: &str, query_lower: &str) -> Option<u8> {
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

pub(crate) fn prompt_score(name: &str, description: &str, query_lower: &str) -> Option<u8> {
    if query_lower.is_empty() || "skill:".starts_with(query_lower) {
        return Some(4);
    }
    tool_score(name, description, query_lower)
}

fn compare_options(left: &ScoredTemplateOption, right: &ScoredTemplateOption) -> Ordering {
    left.score
        .cmp(&right.score)
        .then_with(|| left.option.kind.cmp(&right.option.kind))
        .then_with(|| left.option.detail.cmp(&right.option.detail))
        .then_with(|| left.option.label.cmp(&right.option.label))
}

pub(crate) fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn file_template(path: &str) -> String {
    if is_bare_file_ref(path) {
        format!("@{path}")
    } else {
        format!("@\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

pub(crate) fn is_bare_file_ref(path: &str) -> bool {
    !path.is_empty()
        && (path.contains('/') || path.contains('.'))
        && path.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '_' | '-' | '.')
        })
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use futures_util::stream;

    use super::{
        SessionTemplateKind, SessionTemplateSearchRequest, SessionTemplateSearchTarget,
        file_template, is_bare_file_ref, search_session_templates_with_state,
    };
    use crate::{
        AgentRuntime, BoxCompletionStream, BoxStreamFuture, CompletionEvent, CompletionRequest,
        ModelApi, ModelRef, ProviderId, SkillDefinition,
    };

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

    #[tokio::test]
    async fn prompt_search_returns_skill_templates() {
        let root = PathBuf::from("/workspace");
        let runtime = AgentRuntime::builder(
            ModelRef::new(ProviderId::OpenAi, "test-model", ModelApi::OpenAiResponses),
            Arc::new(TestProvider),
        )
        .with_skill_definitions(vec![SkillDefinition {
            name: "documenting".to_owned(),
            description: "Write repository documentation".to_owned(),
            root: root.clone(),
            skills_dir: root.join(".agents/skills"),
            skill_dir: root.join(".agents/skills/documenting"),
            skill_path: root.join(".agents/skills/documenting/SKILL.md"),
        }])
        .build();

        let options = search_session_templates_with_state(
            &root,
            &runtime,
            &SessionTemplateSearchRequest {
                query: "skill:doc".to_owned(),
                limit: 10,
                kinds: vec![SessionTemplateKind::Prompt],
                target: SessionTemplateSearchTarget::Prompts,
            },
            None,
        )
        .await
        .expect("template search should succeed");

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].kind, SessionTemplateKind::Prompt);
        assert_eq!(options[0].label, "skill:documenting");
        assert_eq!(options[0].template, "/skill:documenting");
        assert_eq!(options[0].metadata["promptType"], "skill");
    }

    struct TestProvider;

    impl crate::LlmStream for TestProvider {
        fn name(&self) -> &'static str {
            "test"
        }

        fn stream_request<'a>(&'a self, _request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
            Box::pin(async move {
                Ok(Box::pin(stream::iter(vec![Ok(CompletionEvent::Finished)]))
                    as BoxCompletionStream)
            })
        }
    }
}
