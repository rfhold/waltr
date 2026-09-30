//! Filesystem tool configuration and provider-facing schemas.

mod config;
pub mod lsp;
mod profile;
mod schema;

pub use config::FilesystemConfig;
pub use profile::{FilesystemToolProfile, FilesystemToolSet, ResolvedFilesystemToolProfile};
pub use schema::{
    ApplyPatchInput, DeleteInput, EditInput, FilesystemToolKind, GlobInput, GrepInput, GrepMatch,
    ReadInput, WriteInput,
};

use crate::{
    AgentTool, ModelRef, middleware::tool_metadata::RootHintsMiddleware,
    sandbox_gateway::SandboxHandle,
};

impl FilesystemConfig {
    /// Convert exposed filesystem capabilities into sandbox-backed agent tools.
    #[must_use]
    pub fn into_agent_tools(self, model: &ModelRef, sandbox: SandboxHandle) -> Vec<AgentTool> {
        self.exposed_tools(model)
            .all()
            .into_iter()
            .map(|kind| crate::tools::sandbox::agent_tool(kind.definition(), sandbox.clone()))
            .collect()
    }

    pub(crate) fn root_hints_middleware(&self, model: &ModelRef) -> RootHintsMiddleware {
        let tool_names = self
            .exposed_tools(model)
            .all()
            .into_iter()
            .map(|kind| kind.name().to_owned())
            .collect();
        RootHintsMiddleware::new(
            tool_names,
            format!(
                " Paths resolve relative to the filesystem root unless absolute. Filesystem root: {}. Use path '.' or omit optional path fields to search the root. You may use absolute paths or '..' when needed;",
                self.root.display()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelApi, ProviderId};

    #[test]
    fn profiles_select_model_tools_without_workspace_access() {
        let gpt = ModelRef::new(ProviderId::OpenAi, "gpt-5-nano", ModelApi::OpenAiResponses);
        let claude = ModelRef::new(
            ProviderId::Anthropic,
            "claude-haiku-4-5",
            ModelApi::AnthropicMessages,
        );
        let config = FilesystemConfig::new(".");
        assert_eq!(
            config.resolve_profile(&gpt),
            ResolvedFilesystemToolProfile::StructuredPatch
        );
        assert_eq!(
            config.resolve_profile(&claude),
            ResolvedFilesystemToolProfile::ExactEdit
        );
        assert_eq!(
            config.exposed_tools(&gpt).all(),
            vec![
                FilesystemToolKind::Read,
                FilesystemToolKind::Glob,
                FilesystemToolKind::Grep,
                FilesystemToolKind::ApplyPatch,
            ]
        );
        let config = config.with_profile(FilesystemToolProfile::WholeFileWrite);
        assert_eq!(
            config.resolve_profile(&gpt),
            ResolvedFilesystemToolProfile::WholeFileWrite
        );
    }
}
