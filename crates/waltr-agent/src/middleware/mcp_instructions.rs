//! MCP server instructions middleware.

use std::sync::Arc;

use crate::{Result, mcp::McpClientRuntime};

use super::{AgentMiddleware, HookOutcome, context::RequestContext, context::SystemPromptParts};

#[derive(Debug, Clone)]
pub(crate) struct McpInstructionsMiddleware {
    runtime: Arc<McpClientRuntime>,
}

impl McpInstructionsMiddleware {
    pub(crate) fn new(runtime: Arc<McpClientRuntime>) -> Self {
        Self { runtime }
    }
}

impl AgentMiddleware for McpInstructionsMiddleware {
    async fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        for instructions in self.runtime.instructions().await {
            prompt.push(format!(
                "MCP server instructions from `{}`:\n{}",
                instructions.server_name, instructions.instructions
            ));
        }
        Ok(HookOutcome::Continue)
    }
}
