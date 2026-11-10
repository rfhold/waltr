use crate::agent_host::middleware::{
    AgentMiddleware,
    context::{RequestContext, ToolDraft},
};

use super::TASK_TOOL_NAME;

/// Hides the provider-facing task tool once a session reaches the configured depth.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct TaskVisibilityMiddleware {
    pub(super) max_subagent_depth: usize,
    pub(super) current_depth: usize,
}

impl AgentMiddleware for TaskVisibilityMiddleware {
    async fn list_tools(
        &self,
        _ctx: &mut RequestContext<'_>,
        tools: &mut ToolDraft,
    ) -> crate::agent_host::Result<()> {
        if self.current_depth >= self.max_subagent_depth {
            tools.tools.retain(|tool| tool.name != TASK_TOOL_NAME);
        }
        Ok(())
    }
}
