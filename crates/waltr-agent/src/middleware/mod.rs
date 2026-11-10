//! Middleware extension points for agent runs.

use std::future::Future;

pub mod chain;
pub mod context;
pub(crate) mod prompt;

use crate::Result;

use self::context::{
    ModelRequestDraft, RequestContext, SystemPromptParts, ToolDraft, ToolResultContext,
};

/// Outcome returned by middleware hooks that can alter runtime progression.
#[derive(Debug)]
pub(crate) enum HookOutcome {
    /// Continue normal hook-chain or runtime progression.
    Continue,
    /// Replace the current tool execution with a synthetic result.
    #[cfg(test)]
    ReplaceToolResult(crate::ToolResult),
}

/// Middleware hooks for request construction, streaming, tools, and run lifecycle.
pub(crate) trait AgentMiddleware: Send + Sync {
    /// Add or edit system prompt fragments before provider request assembly.
    fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        _prompt: &mut SystemPromptParts,
    ) -> impl Future<Output = Result<HookOutcome>> + Send {
        async { Ok(HookOutcome::Continue) }
    }

    /// Add or remove tools before provider request assembly.
    fn list_tools(
        &self,
        _ctx: &mut RequestContext<'_>,
        _tools: &mut ToolDraft,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Edit tool metadata before provider request assembly.
    fn edit_tool_metadata(
        &self,
        _ctx: &mut RequestContext<'_>,
        _tools: &mut ToolDraft,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Observe or alter the provider-neutral request before dispatch.
    fn before_provider_request(
        &self,
        _ctx: &mut RequestContext<'_>,
        _request: &mut ModelRequestDraft,
    ) -> impl Future<Output = Result<HookOutcome>> + Send {
        async { Ok(HookOutcome::Continue) }
    }

    /// Append matching prompt-template options for host autocomplete.

    /// Render one parsed prompt-template reference when this middleware owns it.

    /// Observe or alter a tool result after execution.
    fn after_tool_call(
        &self,
        _ctx: &mut ToolResultContext<'_>,
    ) -> impl Future<Output = Result<HookOutcome>> + Send {
        async { Ok(HookOutcome::Continue) }
    }
}
