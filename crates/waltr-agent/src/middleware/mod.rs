//! Middleware extension points for agent runs.

#![allow(dead_code)]

use std::future::Future;

pub(crate) mod agents;
pub mod chain;
pub(crate) mod claims;
pub mod context;
pub mod cost;
pub mod otel;
pub mod pricing;
pub(crate) mod prompt;
pub(crate) mod question_suspension;
pub mod secrets;
pub(crate) mod skills;
pub(crate) mod tool_metadata;
pub(crate) mod tools;
pub mod usage;

use crate::{CompletionEvent, LlmError, Result, ToolResult};

use self::context::{
    ModelRequestDraft, RequestContext, RunContext, StreamContext, SystemPromptParts,
    ToolCallContext, ToolDraft, ToolResultContext,
};

/// Provider-neutral stream event exposed to middleware.
pub(crate) type ModelEvent = CompletionEvent;

/// Minimal resume input draft for middleware lifecycle hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeInput {
    /// Resume with a new user prompt.
    Prompt(String),
    /// Resume with externally completed tool results.
    ToolResults(Vec<ToolResult>),
    /// Request interruption of active work.
    Interrupt,
}

/// Provider-neutral external request kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRequestKind {
    /// Permission approval request.
    Permission,
    /// Structured user question request.
    Question,
    /// User confirmation request.
    Confirmation,
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
    /// Suppress the current request, event, tool call, or result.
    Suppress,
    /// Replace the current tool execution with a synthetic result.
    ReplaceToolResult(ToolResult),
    /// Persist a suspension and stop runtime progression.
    Suspend(Suspension),
    /// Abort runtime progression with an LLM error.
    Abort(LlmError),
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

    /// Observe or alter one provider stream event.
    fn on_provider_event(
        &self,
        _ctx: &mut StreamContext<'_>,
        _event: &mut ModelEvent,
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

    /// Observe or edit resume input before it is applied.
    fn on_resume(
        &self,
        _ctx: &mut RunContext<'_>,
        _input: &mut ResumeInput,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}
