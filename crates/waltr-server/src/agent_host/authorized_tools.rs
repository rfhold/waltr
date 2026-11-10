//! Exact concrete-tool execution through the ordinary durable agent tool lifecycle.

#[cfg(test)]
mod tests;

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::dispatcher::CapabilityDispatcher;
use crate::permission::PermissionLevel;
use waltr_agent::{ToolCall, ToolDefinition, ToolResult};

/// Exact configured server and remote tool, never a sanitized model alias.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "TargetFields", into = "TargetFields")]
pub struct ConcreteToolTarget {
    server: String,
    tool: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetFields {
    server: String,
    tool: String,
}

impl ConcreteToolTarget {
    /// Construct an exact bounded identity.
    ///
    /// # Errors
    /// Rejects empty, excessive or control-character-bearing identifiers.
    pub fn new(server: impl Into<String>, tool: impl Into<String>) -> Result<Self, &'static str> {
        let target = Self {
            server: server.into(),
            tool: tool.into(),
        };
        if [&target.server, &target.tool].into_iter().any(|value| {
            value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
        }) {
            return Err("invalid concrete tool identity");
        }
        Ok(target)
    }

    /// Configured server identity.
    pub fn server(&self) -> &str {
        &self.server
    }
    /// Exact remote tool name.
    pub fn tool(&self) -> &str {
        &self.tool
    }
    /// Collision-free permission identity; length-prefixing preserves delimiters.
    #[must_use]
    pub fn permission_name(&self) -> String {
        format!(
            "mcp-exact:{}:{}:{}",
            self.server.len(),
            self.server,
            self.tool
        )
    }
}

impl TryFrom<TargetFields> for ConcreteToolTarget {
    type Error = &'static str;
    fn try_from(fields: TargetFields) -> Result<Self, Self::Error> {
        Self::new(fields.server, fields.tool)
    }
}

impl From<ConcreteToolTarget> for TargetFields {
    fn from(target: ConcreteToolTarget) -> Self {
        Self {
            server: target.server,
            tool: target.tool,
        }
    }
}

/// A concrete operation in a caller-bound durable execution session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedToolRequest {
    /// Exact configured target.
    pub target: ConcreteToolTarget,
    /// Unique caller-owned id and arguments; name must equal the target permission identity.
    pub call: ToolCall,
}

/// Schema-free concrete discovery item. Discovery is not an execution grant.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConcreteToolSummary {
    /// Exact configured target, not its model alias.
    pub target: ConcreteToolTarget,
    /// Bounded description without input schema publication.
    pub description: String,
}

impl CapabilityDispatcher {
    /// Discover a bounded exact-identity page under both concrete and alias policy.
    pub fn concrete_tool_page(
        &self,
        allowed: &[ConcreteToolTarget],
        after: Option<&ConcreteToolTarget>,
        limit: usize,
    ) -> Result<Vec<ConcreteToolSummary>, AuthorizedToolError> {
        if !(1..=16).contains(&limit) {
            return Err(AuthorizedToolError::Invalid);
        }
        let mut page = std::collections::BTreeMap::new();
        let policy = self.permission_policy();
        for capability in self.capabilities() {
            let Some(target) = &capability.concrete_target else {
                continue;
            };
            let tool = &capability.tool;
            if !allowed.contains(target)
                || after.is_some_and(|after| target <= after)
                || policy
                    .evaluate_target_tool_effect(&target.permission_name(), &tool.definition().name)
                    .level
                    != PermissionLevel::Allow
                || capability
                    .mcp_app_source
                    .as_ref()
                    .is_some_and(|source| !source.ui.visible_to(mcp::apps::UiVisibility::Model))
            {
                continue;
            }
            let description = tool.definition().description.chars().take(512).collect();
            if page
                .insert(
                    target.clone(),
                    ConcreteToolSummary {
                        target: target.clone(),
                        description,
                    },
                )
                .is_some()
            {
                return Err(AuthorizedToolError::Invalid);
            }
            if page.len() > limit {
                page.pop_last();
            }
        }
        Ok(page.into_values().collect())
    }
}

/// Trusted failure category. Raw provider or tool errors are not authorization diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizedToolError {
    /// Request or schema discovery is invalid or exceeds bounds.
    Invalid,
    /// Policy forbids this operation.
    Denied,
    /// Cancellation or transport loss after dispatch; never replay automatically.
    Uncertain,
    /// Session is unavailable or already owned.
    Unavailable,
}

/// Owned, substitutable executor future.
pub type AuthorizedToolFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, AuthorizedToolError>> + Send + 'static>>;

/// Consumer-neutral execution port. Implementations must use the complete agent lifecycle.
pub trait AuthorizedToolExecutor: Send + Sync {
    /// Execute one exact call with whole-tool and contextual authorization before dispatch.
    fn execute(
        &self,
        request: AuthorizedToolRequest,
        cancellation: CancellationToken,
    ) -> AuthorizedToolFuture<ToolResult>;
    /// Discover only explicitly requested permitted schemas. Discovery grants no execution.
    fn schemas(
        &self,
        targets: Vec<ConcreteToolTarget>,
        cancellation: CancellationToken,
    ) -> AuthorizedToolFuture<Vec<ToolDefinition>>;
    /// Join and stop owned work after request futures have been dropped.
    fn cleanup(&self) -> AuthorizedToolFuture<()>;
}

/// Real coordinator-backed executor. The supplied session is exclusively owned by the consumer.
#[derive(Clone)]
pub struct SessionAuthorizedToolExecutor<
    S: waltr_agent::SessionStore + Clone + Send + Sync + 'static,
> {
    sessions: waltr_agent::SessionCoordinator<S>,
    session_id: waltr_agent::SessionId,
    runtime: waltr_agent::AgentRuntime,
    dispatcher: CapabilityDispatcher,
    owner: waltr_agent::SessionExecutionOwner<S>,
}

impl<S: waltr_agent::SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug
    for SessionAuthorizedToolExecutor<S>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionAuthorizedToolExecutor")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl<S: waltr_agent::SessionStore + Clone + Send + Sync + 'static>
    SessionAuthorizedToolExecutor<S>
{
    /// Bind caller identity and a pinned runtime at construction, not from script arguments.
    pub fn new(
        sessions: waltr_agent::SessionCoordinator<S>,
        session_id: waltr_agent::SessionId,
        runtime: waltr_agent::AgentRuntime,
        dispatcher: CapabilityDispatcher,
    ) -> Self {
        let owner = waltr_agent::SessionExecutionOwner::new(sessions.clone(), session_id.clone());
        Self {
            sessions,
            session_id,
            runtime,
            dispatcher,
            owner,
        }
    }
}

impl<S: waltr_agent::SessionStore + Clone + Send + Sync + 'static> AuthorizedToolExecutor
    for SessionAuthorizedToolExecutor<S>
{
    fn execute(
        &self,
        request: AuthorizedToolRequest,
        cancellation: CancellationToken,
    ) -> AuthorizedToolFuture<ToolResult> {
        let this = self.clone();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AuthorizedToolError::Unavailable);
            }
            if request.call.name != request.target.permission_name() {
                return Err(AuthorizedToolError::Invalid);
            }
            let mut matching = this
                .dispatcher
                .capabilities()
                .filter(|capability| capability.concrete_target.as_ref() == Some(&request.target));
            let capability = matching.next().ok_or(AuthorizedToolError::Denied)?;
            if matching.next().is_some() {
                return Err(AuthorizedToolError::Invalid);
            }
            if capability
                .mcp_app_source
                .as_ref()
                .is_some_and(|source| !source.ui.visible_to(mcp::apps::UiVisibility::Model))
            {
                return Err(AuthorizedToolError::Denied);
            }
            let mut call = request.call;
            call.name = capability.definition().name.clone();
            let registered = this
                .runtime
                .tools()
                .iter()
                .find(|tool| {
                    tool.identity() == Some(request.target.permission_name().as_str())
                        && tool.definition() == capability.definition()
                })
                .ok_or(AuthorizedToolError::Unavailable)?;
            let binding = registered
                .prepare_host_dispatch(&call)
                .map_err(|_| AuthorizedToolError::Unavailable)?
                .ok_or(AuthorizedToolError::Unavailable)?;
            if binding.source_revision != this.dispatcher.revision() {
                return Err(AuthorizedToolError::Unavailable);
            }
            let call_id = call.id.clone();
            let outcome = this
                .owner
                .tool(this.runtime.clone(), call, cancellation)
                .await
                .map_err(map_owned_error)?;
            if !matches!(outcome.status, waltr_agent::SessionStatus::Finished) {
                return Err(AuthorizedToolError::Uncertain);
            }
            let record = this
                .sessions
                .store()
                .load(&this.session_id)
                .await
                .map_err(|_| AuthorizedToolError::Uncertain)?;
            let snapshot = record.snapshot();
            snapshot
                .turns
                .iter()
                .flat_map(|turn| &turn.items)
                .find_map(|item| match item {
                    waltr_agent::TurnItem::ToolResult(result) if result.call_id == call_id => {
                        Some(result.clone())
                    }
                    _ => None,
                })
                .ok_or(AuthorizedToolError::Uncertain)
        })
    }

    fn cleanup(&self) -> AuthorizedToolFuture<()> {
        let owner = self.owner.clone();
        Box::pin(async move { owner.close().await.map_err(map_owned_error) })
    }

    fn schemas(
        &self,
        targets: Vec<ConcreteToolTarget>,
        cancellation: CancellationToken,
    ) -> AuthorizedToolFuture<Vec<ToolDefinition>> {
        let dispatcher = self.dispatcher.clone();
        Box::pin(async move {
            if targets.len() > 32 || cancellation.is_cancelled() {
                return Err(AuthorizedToolError::Invalid);
            }
            let mut definitions = Vec::new();
            for target in targets {
                let policy = dispatcher.permission_policy();
                let mut matching = dispatcher.capabilities().filter(|capability| {
                    capability.concrete_target.as_ref() == Some(&target)
                        && capability.mcp_app_source.as_ref().is_none_or(|source| {
                            source.ui.visible_to(mcp::apps::UiVisibility::Model)
                        })
                });
                let capability = matching.next().ok_or(AuthorizedToolError::Denied)?;
                let tool = &capability.tool;
                if matching.next().is_some() {
                    return Err(AuthorizedToolError::Invalid);
                }
                if policy
                    .evaluate_target_tool_effect(&target.permission_name(), &tool.definition().name)
                    .level
                    != PermissionLevel::Allow
                {
                    return Err(AuthorizedToolError::Denied);
                }
                tool.check_execution(cancellation.clone())
                    .await
                    .map_err(|_| AuthorizedToolError::Denied)?;
                let mut definition = tool.definition().clone();
                definition.name = target.permission_name();
                definitions.push(definition);
            }
            if serde_json::to_vec(&definitions)
                .map_err(|_| AuthorizedToolError::Invalid)?
                .len()
                > 65_536
            {
                return Err(AuthorizedToolError::Invalid);
            }
            Ok(definitions)
        })
    }
}

fn map_owned_error(error: waltr_agent::OwnedExecutionError) -> AuthorizedToolError {
    match error {
        waltr_agent::OwnedExecutionError::Unavailable => AuthorizedToolError::Unavailable,
        waltr_agent::OwnedExecutionError::Uncertain => AuthorizedToolError::Uncertain,
    }
}
