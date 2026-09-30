//! Bounded filesystem context from the connected worker.

use super::{
    AgentMiddleware, HookOutcome,
    context::{MiddlewareStateAccess, RequestContext, SystemPromptParts},
};
use crate::{Result, prompt::FilesystemPromptConfig, sandbox_gateway::SandboxHandle};

#[derive(Debug, Clone)]
pub(crate) struct SandboxSnapshotMiddleware {
    config: FilesystemPromptConfig,
    sandbox: SandboxHandle,
}

impl SandboxSnapshotMiddleware {
    pub(crate) fn new(config: FilesystemPromptConfig, sandbox: SandboxHandle) -> Self {
        Self { config, sandbox }
    }
}

impl AgentMiddleware for SandboxSnapshotMiddleware {
    async fn build_system_prompt(
        &self,
        ctx: &mut RequestContext<'_>,
        prompt: &mut SystemPromptParts,
    ) -> Result<HookOutcome> {
        if let Some(snapshot) = ctx.state_as::<String>("filesystem_snapshot")? {
            prompt.push(snapshot);
            return Ok(HookOutcome::Continue);
        }
        let data = self
            .sandbox
            .snapshot(
                &self.config.root.to_string_lossy(),
                self.config.max_snapshot_entries,
                self.config.max_snapshot_depth,
                self.config.max_snapshot_bytes,
            )
            .await?;
        let output = if data.content.is_empty() {
            if self.config.max_snapshot_bytes >= "- <snapshot truncated>\n".len() {
                "Current filesystem snapshot:\n- <empty>".to_owned()
            } else {
                "Current filesystem snapshot:".to_owned()
            }
        } else {
            format!("Current filesystem snapshot:\n{}", data.content)
        };
        ctx.set_state_as("filesystem_snapshot", &output)?;
        prompt.push(output);
        Ok(HookOutcome::Continue)
    }
}
