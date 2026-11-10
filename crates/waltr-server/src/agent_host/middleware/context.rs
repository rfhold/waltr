//! Middleware context and draft types.

use std::{collections::BTreeMap, path::Path};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::agent_host::{
    CompletionTurn, PermissionClaim, PermissionPolicy, PromptTurn, Result, ToolCall,
    ToolDefinition, ToolResult, middleware::chain::MiddlewareChain,
    resource::SessionTemplateSearchRequest,
};
use tokio_util::sync::CancellationToken;

/// Loop-owned durable middleware state with pending journal writes.
#[derive(Debug, Default)]
pub(crate) struct MiddlewareStateLog {
    effects: Option<std::sync::Arc<dyn waltr_agent::HostEffectExecutor>>,
    current: BTreeMap<String, Value>,
    pending: Vec<(String, Value)>,
}

impl MiddlewareStateLog {
    pub(crate) fn from_values(current: BTreeMap<String, Value>) -> Self {
        Self {
            current,
            pending: Vec::new(),
            effects: None,
        }
    }

    pub(crate) fn flush_tool(&mut self, sink: &waltr_agent::ToolResultMetadata) -> Result<()> {
        for (namespace, value) in self.pending.drain(..) {
            sink.set_context(namespace, value)?;
        }
        Ok(())
    }
    pub(crate) fn from_host(state: &dyn waltr_agent::HostContextState) -> Self {
        Self {
            current: state.snapshot(),
            pending: Vec::new(),
            effects: state.effects(),
        }
    }

    pub(crate) async fn execute_effect(
        &mut self,
        tool: waltr_agent::AgentTool,
        call: ToolCall,
        turn: u64,
    ) -> Result<ToolResult> {
        let effects =
            self.effects
                .clone()
                .ok_or_else(|| crate::agent_host::LlmError::Protocol {
                    provider: "server-context",
                    message: "durable host effect journal is unavailable".into(),
                })?;
        let output = effects
            .execute(tool, call, turn, self.current.clone())
            .await?;
        for (namespace, value) in output.context {
            self.set(namespace, value);
        }
        output.result
    }

    pub(crate) fn flush_host(
        &mut self,
        state: &mut dyn waltr_agent::HostContextState,
    ) -> Result<()> {
        for (namespace, value) in self.pending.drain(..) {
            state.set(namespace, value)?;
        }
        Ok(())
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
            .map_err(|error| crate::agent_host::LlmError::Protocol {
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
            serde_json::to_value(state).map_err(|error| crate::agent_host::LlmError::Protocol {
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
    #[cfg(test)]
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
    /// Tool name authorized before entering the middleware chain.
    pub(crate) approved_tool_name: &'a str,
    /// Current policy for middleware that performs predispatch I/O.
    pub(crate) permission_policy: &'a PermissionPolicy,
    pub(crate) authorization: Option<LiveAuthorization>,
    /// Active run cancellation for predispatch middleware I/O.
    pub(crate) cancellation_token: CancellationToken,
    /// Durable session id.
    pub(crate) session_id: &'a str,
    /// Current provider turn number.
    #[allow(dead_code)]
    pub(crate) turn: u64,
    /// Tool call currently being evaluated.
    pub(crate) call: &'a mut ToolCall,
    /// Permission claims appended by claims-producing middleware earlier in the chain.
    #[allow(dead_code)]
    pub(crate) permission_claims: &'a mut Vec<PermissionClaim>,
    /// Durable middleware state for this run.
    pub(crate) middleware_state: &'a mut MiddlewareStateLog,
}

#[derive(Clone)]
pub(crate) struct LiveAuthorization {
    pub(crate) context: waltr_agent::ToolExecutionContext,
    pub(crate) policy: std::sync::Arc<dyn crate::agent_host::dispatcher::CapabilityPolicy>,
    pub(crate) identity: String,
}

impl std::fmt::Debug for LiveAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveAuthorization")
    }
}

impl ToolCallContext<'_> {
    pub(crate) async fn authorize(
        &self,
        claims: &[PermissionClaim],
    ) -> crate::agent_host::Result<()> {
        let live =
            self.authorization
                .as_ref()
                .ok_or_else(|| crate::agent_host::LlmError::Protocol {
                    provider: "permission",
                    message: "live operation authorization is unavailable".into(),
                })?;
        if self.call.name != self.approved_tool_name || self.call.name != live.context.call().name {
            return Err(crate::agent_host::LlmError::Protocol {
                provider: "permission",
                message: "capability identity changed".into(),
            });
        }
        let mut context = live.context.clone();
        context.replace_arguments(self.call.arguments.clone());
        match live
            .policy
            .authorize(&context, &live.identity, self.permission_policy, claims)
            .await?
        {
            None if !context.is_cancelled() => context.check_execution().await,
            message => Err(crate::agent_host::LlmError::Protocol {
                provider: "permission",
                message: message.unwrap_or_else(|| "operation cancelled".into()),
            }),
        }
    }
    pub(crate) async fn interact(
        &self,
        request: crate::interaction::Interaction,
    ) -> std::result::Result<
        crate::interaction::InteractionResponse,
        crate::interaction::InteractionError,
    > {
        let live = self
            .authorization
            .as_ref()
            .ok_or(crate::interaction::InteractionError::Unavailable)?;
        let response = live.policy.interact(&live.context, request).await?;
        live.context
            .check_execution()
            .await
            .map_err(|_| crate::interaction::InteractionError::Cancelled)?;
        Ok(response)
    }
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
    #[allow(dead_code)]
    #[allow(dead_code)]
    pub(crate) sandbox: Option<&'a crate::agent_host::sandbox_gateway::SandboxHandle>,
}

impl TemplateSearchContext<'_> {
    /// Return durable middleware state deserialized as `T`, when present.
    pub(crate) fn state_as<T: DeserializeOwned>(&self, namespace: &str) -> Result<Option<T>> {
        self.middleware_state
            .and_then(|state| state.get(namespace))
            .map(|state| serde_json::from_value(state.clone()))
            .transpose()
            .map_err(|error| crate::agent_host::LlmError::Protocol {
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
    #[allow(dead_code)]
    pub(crate) sandbox: Option<&'a crate::agent_host::sandbox_gateway::SandboxHandle>,
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
