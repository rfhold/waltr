//! Neutral host autocomplete contracts.
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
    /// Cursor byte offset relative to `query`, when the client wants context-aware completions.
    pub cursor: Option<usize>,
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

    pub fn includes(&self, kind: SessionTemplateKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// Session template search target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SessionTemplateSearchTarget {
    /// File and tool references available to the session runtime.
    #[default]
    Resources,
    /// Prompt templates such as skills or MCP prompts.
    Prompts,
}

/// Session template kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTemplateKind {
    /// Workspace file reference.
    File,
    /// Agent tool reference, including MCP-backed tools.
    Tool,
    /// MCP resource reference.
    Resource,
    /// Prompt reference, such as an Agent Skill or MCP prompt.
    Prompt,
}

/// Template option suitable for insertion into a prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
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
