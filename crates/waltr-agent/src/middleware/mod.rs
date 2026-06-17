//! Middleware extension points for agent runs.

use std::future::Future;

pub(crate) mod agents;
pub mod chain;
pub(crate) mod claims;
pub mod context;
pub(crate) mod prompt;
pub(crate) mod question_suspension;
pub(crate) mod skills;
pub(crate) mod tool_metadata;
pub(crate) mod tools;

use crate::{Result, ToolResult};

use self::context::{
    ModelRequestDraft, RequestContext, RunContext, SystemPromptParts, ToolCallContext, ToolDraft,
    ToolResultContext,
};

/// Provider-neutral external request kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRequestKind {
    /// Permission approval request.
    Permission,
    /// Structured user question request.
    Question,
    /// Middleware-defined request kind.
    Custom(String),
}

/// Provider-neutral external request status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRequestStatus {
    /// Request is awaiting an external response.
    Pending,
    /// Request has been answered.
    Answered,
    /// Request was canceled before an answer was applied.
    Canceled,
}

/// Provider-neutral external request draft for middleware suspensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRequestDraft {
    /// Runtime request identifier.
    pub id: String,
    /// Request kind.
    pub kind: ExternalRequestKind,
    /// Originating tool call id, when applicable.
    pub tool_call_id: Option<String>,
    /// Human-readable prompt for hosts and logs.
    pub prompt: String,
    /// Request-specific payload.
    pub payload: serde_json::Value,
    /// Current request status.
    pub status: ExternalRequestStatus,
}

/// Minimal runtime suspension draft for middleware hook outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspension {
    /// Human-readable suspension reason or routing key.
    pub reason: String,
    /// External request that caused the suspension, when any.
    pub external_request: Option<ExternalRequestDraft>,
}

impl Suspension {
    /// Create a suspension draft.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            external_request: None,
        }
    }

    /// Create a suspension draft backed by an external request.
    pub fn external(reason: impl Into<String>, external_request: ExternalRequestDraft) -> Self {
        Self {
            reason: reason.into(),
            external_request: Some(external_request),
        }
    }
}

/// Outcome returned by middleware hooks that can alter runtime progression.
#[derive(Debug)]
pub(crate) enum HookOutcome {
    /// Continue normal hook-chain or runtime progression.
    Continue,
    /// Replace the current tool execution with a synthetic result.
    ReplaceToolResult(ToolResult),
    /// Persist a suspension and stop runtime progression.
    Suspend(Suspension),
}

/// Middleware hooks for request construction, streaming, tools, and run lifecycle.
pub(crate) trait AgentMiddleware: Send + Sync {
    /// Add or edit system prompt fragments before provider request assembly.
    fn build_system_prompt(
        &self,
        _ctx: &mut RequestContext<'_>,
        _prompt: &mut SystemPromptParts,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
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

    /// Observe or alter a tool call before execution.
    fn before_tool_call(
        &self,
        _ctx: &mut ToolCallContext<'_>,
    ) -> impl Future<Output = Result<HookOutcome>> + Send {
        async { Ok(HookOutcome::Continue) }
    }

    /// Observe or alter a tool result after execution.
    fn after_tool_call(
        &self,
        _ctx: &mut ToolResultContext<'_>,
    ) -> impl Future<Output = Result<HookOutcome>> + Send {
        async { Ok(HookOutcome::Continue) }
    }

    /// Observe or edit a suspension before it is emitted.
    fn on_suspend(
        &self,
        _ctx: &mut RunContext<'_>,
        _suspension: &mut Suspension,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}
