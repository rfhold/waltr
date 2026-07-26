use crate::middleware::{
    AgentMiddleware,
    context::{RequestContext, ToolDraft},
};

use super::{TASK_TOOL_NAME, metadata::metadata_depth};

/// Hides the provider-facing task tool once a session reaches the configured depth.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct TaskVisibilityMiddleware {
    pub(super) max_subagent_depth: usize,
}

impl AgentMiddleware for TaskVisibilityMiddleware {
    async fn list_tools(
        &self,
        ctx: &mut RequestContext<'_>,
        tools: &mut ToolDraft,
    ) -> crate::Result<()> {
        let current_depth = metadata_depth(&ctx.session_metadata_values).unwrap_or_default();
        if current_depth >= self.max_subagent_depth {
            tools.tools.retain(|tool| tool.name != TASK_TOOL_NAME);
        }
        Ok(())
    }
}
