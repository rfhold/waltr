//! Middleware context and draft types.

use std::{collections::BTreeMap, path::Path};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    CompletionTurn, PermissionClaim, PermissionPolicy, PromptTurn, Result, ToolCall,
    ToolDefinition, ToolResult,
    agent::session::{SessionJournalEvent, SessionSnapshot},
    middleware::chain::MiddlewareChain,
    resource::SessionTemplateSearchRequest,
};
use tokio_util::sync::CancellationToken;

use super::prompt::SystemPromptOverride;

/// Loop-owned durable middleware state with pending journal writes.
#[derive(Debug, Default)]
pub(crate) struct MiddlewareStateLog {
    current: BTreeMap<String, Value>,
    pending: Vec<(String, Value)>,
}

impl MiddlewareStateLog {
    /// Hydrate current state from a stored session snapshot.
    pub(crate) fn from_snapshot(snapshot: &SessionSnapshot) -> Self {
        Self {
            current: snapshot.middleware_state.clone(),
            pending: Vec::new(),
        }
    }

    /// Return the current state blob for a namespace.
    pub(crate) fn get(&self, namespace: &str) -> Option<&Value> {
        self.current.get(namespace)
    }

    /// Replace a namespace state blob and queue the durable write.
    pub(crate) fn set(&mut self, namespace: impl Into<String>, state: Value) {
        let namespace = namespace.into();
        self.current.insert(namespace.clone(), state.clone());
        self.pending.push((namespace, state));
    }

    /// Drain queued writes into journal events in write order.
    pub(crate) fn drain_pending(&mut self) -> Vec<SessionJournalEvent> {
        self.pending
            .drain(..)
            .map(
                |(namespace, state)| SessionJournalEvent::MiddlewareStateUpdated {
                    namespace,
                    state,
                },
            )
            .collect()
    }
}

/// Read/write access to durable middleware state from a middleware context.
pub(crate) trait MiddlewareStateAccess {
    /// Borrow the durable middleware state log.
    fn state_log(&self) -> &MiddlewareStateLog;

    /// Mutably borrow the durable middleware state log.
    fn state_log_mut(&mut self) -> &mut MiddlewareStateLog;

    /// Return durable middleware state deserialized as `T`, when present.
    fn state_as<T: DeserializeOwned>(&self, namespace: &str) -> Result<Option<T>> {
        self.state_log()
            .get(namespace)
            .map(|state| serde_json::from_value(state.clone()))
            .transpose()
            .map_err(|error| crate::LlmError::Protocol {
                provider: "middleware-state",
                message: error.to_string(),
            })
    }

    /// Replace durable middleware state for a namespace with serialized `T`.
    fn set_state_as<T: Serialize>(
        &mut self,
        namespace: impl Into<String>,
        state: &T,
    ) -> Result<()> {
        self.state_log_mut().set(
            namespace,
            serde_json::to_value(state).map_err(|error| crate::LlmError::Protocol {
                provider: "middleware-state",
                message: error.to_string(),
            })?,
        );
        Ok(())
    }
}

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
    /// User prompts included for chronological replay.
    pub(crate) prompt_turns: Vec<PromptTurn>,
    /// Prior completed turns included for replay, in turn order.
    pub(crate) turns: Vec<CompletionTurn>,
}

/// Context shared by request-building middleware phases.
#[derive(Debug)]
pub(crate) struct RequestContext<'a> {
    /// Durable middleware state for this run.
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
    /// Host-defined durable metadata values for the current session.
    pub(crate) session_metadata_values: BTreeMap<String, Value>,
    /// Pending per-run system prompt override, consumed by the system prompt middleware.
    pub(crate) system_prompt_override: &'a mut Option<SystemPromptOverride>,
    /// Middleware chain used by prompt template tool calls.
    pub(crate) middleware_chain: MiddlewareChain,
    /// Permission policy used by prompt template tool calls.
    pub(crate) permission_policy: PermissionPolicy,
    /// Shared cancellation token for prompt templating tool calls.
    pub(crate) cancellation_token: CancellationToken,
    /// Durable session id for prompt templating tool calls.
    pub(crate) session_id: &'a str,
    /// Current provider turn for prompt templating tool calls.
    pub(crate) turn: u64,
}

impl RequestContext<'_> {
    /// Take the pending per-run system prompt override, when any.
    pub(crate) const fn take_system_prompt_override(&mut self) -> Option<SystemPromptOverride> {
        self.system_prompt_override.take()
    }
}

impl MiddlewareStateAccess for RequestContext<'_> {
    fn state_log(&self) -> &MiddlewareStateLog {
        self.middleware_state
    }

    fn state_log_mut(&mut self) -> &mut MiddlewareStateLog {
        self.middleware_state
    }
}

/// Context shared by tool-call middleware phases.
#[derive(Debug)]
pub(crate) struct ToolCallContext<'a> {
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current provider turn number.
    pub(crate) turn: u64,
    /// Tool call currently being evaluated.
    pub(crate) call: &'a mut ToolCall,
    /// Permission claims appended by claims-producing middleware earlier in the chain.
    #[allow(dead_code)]
    pub(crate) permission_claims: &'a mut Vec<PermissionClaim>,
    /// Durable middleware state for this run.
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
}

impl MiddlewareStateAccess for ToolCallContext<'_> {
    fn state_log(&self) -> &MiddlewareStateLog {
        self.middleware_state
    }

    fn state_log_mut(&mut self) -> &mut MiddlewareStateLog {
        self.middleware_state
    }
}

/// Context shared by tool-result middleware phases.
#[derive(Debug)]
pub(crate) struct ToolResultContext<'a> {
    /// Tool call that produced this result, when available.
    pub(crate) call: Option<&'a ToolCall>,
    /// Tool result currently being evaluated.
    pub(crate) result: &'a mut ToolResult,
    /// Durable middleware state for this run.
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
    /// Middleware chain used by prompt template tool calls.
    pub(crate) middleware_chain: MiddlewareChain,
    /// Permission policy used by prompt template tool calls.
    pub(crate) permission_policy: PermissionPolicy,
    /// Shared cancellation token for prompt templating tool calls.
    pub(crate) cancellation_token: CancellationToken,
    /// Durable session id for prompt templating tool calls.
    pub(crate) session_id: &'a str,
    /// Current provider turn for prompt templating tool calls.
    pub(crate) turn: u64,
}

impl MiddlewareStateAccess for ToolResultContext<'_> {
    fn state_log(&self) -> &MiddlewareStateLog {
        self.middleware_state
    }

    fn state_log_mut(&mut self) -> &mut MiddlewareStateLog {
        self.middleware_state
    }
}

/// Context shared by template autocomplete middleware.
#[derive(Debug)]
pub(crate) struct TemplateSearchContext<'a> {
    /// Workspace root for resource-style searches.
    pub(crate) root: &'a Path,
    /// Original search request from the host.
    pub(crate) request: &'a SessionTemplateSearchRequest,
    /// Durable middleware state from the session snapshot, when available.
    pub(crate) middleware_state: Option<&'a MiddlewareStateLog>,
    pub(crate) sandbox: Option<&'a crate::sandbox_gateway::SandboxHandle>,
}

impl TemplateSearchContext<'_> {
    /// Return durable middleware state deserialized as `T`, when present.
    pub(crate) fn state_as<T: DeserializeOwned>(&self, namespace: &str) -> Result<Option<T>> {
        self.middleware_state
            .and_then(|state| state.get(namespace))
            .map(|state| serde_json::from_value(state.clone()))
            .transpose()
            .map_err(|error| crate::LlmError::Protocol {
                provider: "middleware-state",
                message: error.to_string(),
            })
    }
}

/// Context shared by template rendering middleware.
#[derive(Debug)]
pub(crate) struct TemplateRenderContext<'a> {
    /// Base directory used for relative file references.
    pub(crate) base_dir: &'a Path,
    /// Durable middleware state for this run.
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
    /// Middleware chain used by template-owned tool calls.
    pub(crate) middleware_chain: &'a MiddlewareChain,
    /// Permission policy used by template-owned tool calls.
    pub(crate) permission_policy: &'a PermissionPolicy,
    /// Shared cancellation token for template-owned tool calls.
    pub(crate) cancellation_token: CancellationToken,
    /// Durable session id for template-owned tool calls.
    pub(crate) session_id: &'a str,
    /// Current provider turn for template-owned tool calls.
    pub(crate) turn: u64,
    pub(crate) sandbox: Option<&'a crate::sandbox_gateway::SandboxHandle>,
    pub(crate) tool_call_id: Option<&'a str>,
}

impl MiddlewareStateAccess for TemplateRenderContext<'_> {
    fn state_log(&self) -> &MiddlewareStateLog {
        self.middleware_state
    }

    fn state_log_mut(&mut self) -> &mut MiddlewareStateLog {
        self.middleware_state
    }
}

/// Context shared by run-level middleware phases.
#[derive(Debug)]
pub(crate) struct RunContext<'a> {
    /// Durable middleware state for this run.
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
}

impl MiddlewareStateAccess for RunContext<'_> {
    fn state_log(&self) -> &MiddlewareStateLog {
        self.middleware_state
    }

    fn state_log_mut(&mut self) -> &mut MiddlewareStateLog {
        self.middleware_state
    }
}
