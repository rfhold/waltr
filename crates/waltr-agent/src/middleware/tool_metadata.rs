//! Tool metadata middleware.

use crate::Result;

use super::{
    AgentMiddleware,
    context::{RequestContext, ToolDraft},
};

/// Middleware that appends an allowed-roots hint to selected tool descriptions per request.
///
/// Stored `AgentTool` definitions stay pristine; the hint text exists only in the
/// per-request tool projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootHintsMiddleware {
    tool_names: Vec<String>,
    hint: String,
}

impl RootHintsMiddleware {
    /// Create middleware that appends `hint` to the named tools' descriptions.
    pub fn new(tool_names: Vec<String>, hint: impl Into<String>) -> Self {
        Self {
            tool_names,
            hint: hint.into(),
        }
    }
}

impl AgentMiddleware for RootHintsMiddleware {
    async fn edit_tool_metadata(
        &self,
        _ctx: &mut RequestContext<'_>,
        tools: &mut ToolDraft,
    ) -> Result<()> {
        for tool in &mut tools.tools {
            if self.tool_names.iter().any(|name| name == &tool.name) {
                tool.description.push_str(&self.hint);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        ToolDefinition,
        middleware::context::{MiddlewareStateLog, RequestContext},
    };

    use super::*;

    #[tokio::test]
    async fn appends_hint_only_to_named_tools() {
        let middleware = RootHintsMiddleware::new(vec!["read".to_owned()], " Root: /workspace.");
        let mut middleware_state = MiddlewareStateLog::default();
        let mut system_prompt_override = None;
        let mut ctx = RequestContext {
            middleware_state: &mut middleware_state,
            system_prompt_override: &mut system_prompt_override,
        };
        let mut draft = ToolDraft::default();
        draft.push(ToolDefinition::new("read", "Read a file.", json!({})));
        draft.push(ToolDefinition::new("other", "Other tool.", json!({})));

        middleware
            .edit_tool_metadata(&mut ctx, &mut draft)
            .await
            .expect("metadata middleware should run");

        assert_eq!(draft.tools[0].description, "Read a file. Root: /workspace.");
        assert_eq!(draft.tools[1].description, "Other tool.");
    }
}
