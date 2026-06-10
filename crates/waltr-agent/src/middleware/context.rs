//! Middleware context and draft types.

#![allow(dead_code)]

use crate::{
    AgentState, CompletionTurn, ModelId, PermissionClaim, ProviderId, ToolCall, ToolDefinition,
    ToolResult,
};

/// Mutable system prompt fragments assembled by middleware.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SystemPromptParts {
    /// Ordered prompt fragments.
    pub(crate) parts: Vec<String>,
}

impl SystemPromptParts {
    /// Append one prompt fragment.
    pub(crate) fn push(&mut self, part: impl Into<String>) {
        self.parts.push(part.into());
    }
}

/// Mutable tool list assembled by middleware.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolDraft {
    /// Provider-neutral tool definitions visible to the model.
    pub(crate) tools: Vec<ToolDefinition>,
}

impl ToolDraft {
    /// Add one tool definition.
    pub(crate) fn push(&mut self, tool: ToolDefinition) {
        self.tools.push(tool);
    }
}

/// Provider-neutral request draft visible to middleware before dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelRequestDraft {
    /// User prompt or projected conversation input for this provider turn.
    pub(crate) prompt: String,
    /// Fully assembled system prompt, when present.
    pub(crate) system_prompt: Option<String>,
    /// Tool definitions visible to the model.
    pub(crate) tools: Vec<ToolDefinition>,
    /// Prior completed turns included for replay, in turn order.
    pub(crate) turns: Vec<CompletionTurn>,
}

/// Context shared by request-building middleware phases.
#[derive(Debug)]
pub(crate) struct RequestContext<'a> {
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current optimistic session revision.
    pub(crate) revision: u64,
    /// Model id selected for this request.
    pub(crate) model_id: &'a ModelId,
    /// Provider id, when the provider has already been selected.
    pub(crate) provider_id: Option<ProviderId>,
    /// Current provider turn number.
    pub(crate) turn: u64,
    /// Snapshot of durable session state for this phase.
    pub(crate) session: &'a AgentState,
}

/// Context shared by provider stream middleware phases.
#[derive(Debug)]
pub(crate) struct StreamContext<'a> {
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current optimistic session revision.
    pub(crate) revision: u64,
    /// Model id selected for this stream.
    pub(crate) model_id: &'a ModelId,
    /// Provider emitting stream events.
    pub(crate) provider_id: ProviderId,
    /// Current provider turn number.
    pub(crate) turn: u64,
    /// Snapshot of durable session state for this phase.
    pub(crate) session: &'a AgentState,
}

/// Context shared by tool-call middleware phases.
#[derive(Debug)]
pub(crate) struct ToolCallContext<'a> {
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current optimistic session revision.
    pub(crate) revision: u64,
    /// Current provider turn number.
    pub(crate) turn: u64,
    /// Snapshot of durable session state for this phase.
    pub(crate) session: &'a AgentState,
    /// Tool call currently being evaluated.
    pub(crate) call: &'a mut ToolCall,
    /// Host tool definition selected for this call, when known.
    pub(crate) tool_definition: Option<&'a ToolDefinition>,
    /// Permission claims derived for this call, when already available.
    pub(crate) permission_claims: Option<&'a [PermissionClaim]>,
}

/// Context shared by tool-result middleware phases.
#[derive(Debug)]
pub(crate) struct ToolResultContext<'a> {
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current optimistic session revision.
    pub(crate) revision: u64,
    /// Current provider turn number.
    pub(crate) turn: u64,
    /// Snapshot of durable session state for this phase.
    pub(crate) session: &'a AgentState,
    /// Tool call that produced this result, when available.
    pub(crate) call: Option<&'a ToolCall>,
    /// Tool result currently being evaluated.
    pub(crate) result: &'a mut ToolResult,
}

/// Context shared by run-level middleware phases.
#[derive(Debug)]
pub(crate) struct RunContext<'a> {
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current optimistic session revision.
    pub(crate) revision: u64,
    /// Current provider turn number.
    pub(crate) turn: u64,
    /// Snapshot of durable session state for this phase.
    pub(crate) session: &'a AgentState,
}
