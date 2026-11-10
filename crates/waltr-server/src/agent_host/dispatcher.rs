//! One concrete capability registry and execution boundary for server entrypoints.

use super::{Capability, tools::sandbox::claims};
use crate::permission::{PermissionClaim, PermissionPolicy};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};
use waltr_agent::{
    AgentTool, HostDispatchBinding, LlmError, Result, SessionId, ToolExecutionContext, ToolResult,
};

pub type PolicyFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + 'a>>;

/// D4 implements interaction waits and approval scopes here, before Execute or a handler.
pub trait CapabilityPolicy: Send + Sync {
    fn interaction_binding(
        &self,
        _context: &ToolExecutionContext,
    ) -> Option<(
        crate::interaction::InteractionBroker,
        crate::interaction::OperationBinding,
    )> {
        None
    }
    fn authorize<'a>(
        &'a self,
        context: &'a ToolExecutionContext,
        identity: &'a str,
        policy: &'a PermissionPolicy,
        claims: &'a [PermissionClaim],
    ) -> PolicyFuture<'a>;
    fn interact<'a>(
        &'a self,
        _context: &'a ToolExecutionContext,
        _request: crate::interaction::Interaction,
    ) -> crate::interaction::InteractionFuture<'a> {
        Box::pin(async { Err(crate::interaction::InteractionError::Unavailable) })
    }
    fn finished(&self, _operation_id: &str) {}
    fn retire_session(&self, _session: &SessionId) {}
}

pub struct UnconfiguredPolicy;
impl CapabilityPolicy for UnconfiguredPolicy {
    fn authorize<'a>(
        &'a self,
        _: &'a ToolExecutionContext,
        _: &'a str,
        _: &'a PermissionPolicy,
        _: &'a [PermissionClaim],
    ) -> PolicyFuture<'a> {
        Box::pin(async { Ok(Some("server capability policy is not configured".into())) })
    }
}

#[derive(Clone)]
pub struct CapabilityDispatcher {
    entries: Arc<BTreeMap<String, Capability>>,
    revision: String,
    session: Option<SessionId>,
    policy: Arc<dyn CapabilityPolicy>,
    context: Option<(super::middleware::chain::MiddlewareChain, PermissionPolicy)>,
}

impl CapabilityDispatcher {
    pub fn new(
        capabilities: Vec<Capability>,
        session: Option<SessionId>,
        policy: Arc<dyn CapabilityPolicy>,
    ) -> Result<Self> {
        let mut entries = BTreeMap::new();
        let mut names = std::collections::BTreeSet::new();
        for capability in capabilities {
            let name = capability.definition().name.clone();
            let identity = capability.identity();
            if !names.insert(capability.definition().name.clone())
                || entries.insert(capability.identity(), capability).is_some()
            {
                return Err(invalid(&format!(
                    "duplicate capability identity or visible name: {identity} ({name})"
                )));
            }
        }
        let declarations = entries
            .iter()
            .map(|(identity, cap)| {
                (
                    identity,
                    cap.definition(),
                    &cap.mcp_source_revision,
                    cap.mcp_app_source
                        .as_ref()
                        .map(|source| &source.declaration),
                )
            })
            .collect::<Vec<_>>();
        let revision = super::sha256_hex(
            &serde_json::to_vec(&declarations)
                .map_err(|_| invalid("capability declaration cannot be serialized"))?,
        );
        Ok(Self {
            entries: Arc::new(entries),
            revision,
            session,
            policy,
            context: None,
        })
    }

    pub(crate) fn with_context(
        mut self,
        chain: super::middleware::chain::MiddlewareChain,
        policy: PermissionPolicy,
    ) -> Self {
        self.context = Some((chain, policy));
        self
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub(crate) fn with_revision_source(mut self, source: &str) -> Self {
        self.revision = super::sha256_hex(
            &serde_json::to_vec(&(&self.revision, source)).expect("dispatch source serialization"),
        );
        self
    }
    pub fn capabilities(&self) -> impl Iterator<Item = &Capability> {
        self.entries.values()
    }
    pub fn permission_policy(&self) -> PermissionPolicy {
        self.context
            .as_ref()
            .map(|(_, policy)| policy.clone())
            .unwrap_or_else(|| PermissionPolicy::new(crate::permission::PermissionLevel::Deny))
    }
    pub fn narrowed_permissions(mut self, restriction: PermissionPolicy) -> Self {
        if let Some((_, policy)) = &mut self.context {
            *policy = policy.clone().intersection(restriction);
        }
        self
    }

    pub(crate) fn operation_policy(&self) -> Arc<dyn CapabilityPolicy> {
        self.policy.clone()
    }

    /// Trusted host specialization keeps the registry's identity, policy, and middleware.
    pub(crate) fn scoped_effect(&self, capability: Capability) -> Result<AgentTool> {
        let identity = capability.identity();
        let original = self
            .entries
            .get(&identity)
            .ok_or_else(|| invalid("unknown scoped capability"))?;
        if original.definition() != capability.definition()
            || original.mcp_source_revision != capability.mcp_source_revision
            || original.mcp_app_source != capability.mcp_app_source
        {
            return Err(invalid("scoped capability declaration changed"));
        }
        let mut entries = self.entries.as_ref().clone();
        entries.insert(identity.clone(), capability);
        let mut dispatcher = self.clone();
        dispatcher.entries = Arc::new(entries);
        let bound = dispatcher
            .agent_tools()
            .into_iter()
            .find(|tool| tool.identity() == Some(identity.as_str()))
            .ok_or_else(|| invalid("scoped capability adapter unavailable"))?;
        let preparation = bound.clone();
        let guard = bound.clone();
        let context_chain = self.context.clone();
        Ok(
            AgentTool::new_async(bound.definition().clone(), move |context| {
                let bound = bound.clone();
                let chain = context_chain.clone();
                Box::pin(async move {
                    let mut result = bound.call(context.clone()).await?;
                    let original = result.clone();
                    if let Some((chain, policy)) = chain {
                        use super::middleware::{
                            HookOutcome,
                            context::{MiddlewareStateLog, ToolResultContext},
                        };
                        let mut state = MiddlewareStateLog::from_values(
                            context.result_metadata().context_snapshot(),
                        );
                        let outcome = chain
                            .after_tool_call(&mut ToolResultContext {
                                call: Some(context.call()),
                                result: &mut result,
                                middleware_state: &mut state,
                                middleware_chain: chain.clone(),
                                permission_policy: policy,
                                cancellation_token: context.cancellation_token().clone(),
                                session_id: context.session_id().map_or("", SessionId::as_str),
                                turn: context.turn().unwrap_or_default(),
                            })
                            .await;
                        state.flush_tool(context.result_metadata())?;
                        if !matches!(outcome?, HookOutcome::Continue) || result != original {
                            return Err(invalid(
                                "app result changed in middleware; raw publication denied",
                            ));
                        }
                    }
                    Ok(result)
                })
            })
            .with_identity(identity)
            .with_execution_guard(move |cancellation| guard.check_execution(cancellation))
            .with_host_dispatch(move |call| {
                preparation
                    .prepare_host_dispatch(call)?
                    .ok_or_else(|| invalid("scoped effect requires durable association"))
            }),
        )
    }

    /// Every model tool is only an adapter into this same dispatcher.
    pub fn agent_tools(&self) -> Vec<AgentTool> {
        self.entries
            .iter()
            .map(|(identity, capability)| {
                let dispatcher = self.clone();
                let identity = identity.clone();
                let evidence = identity.clone();
                let revision = self.revision.clone();
                let binding_revision = revision.clone();
                let bound_tool = capability.tool.clone();
                let has_custom_binding = bound_tool.requires_host_dispatch();
                AgentTool::new_async(capability.definition().clone(), move |context| {
                    let dispatcher = dispatcher.clone();
                    let identity = identity.clone();
                    let revision = revision.clone();
                    Box::pin(async move {
                        let Some(binding) = context.host_dispatch() else {
                            return Ok(context
                                .result("durable dispatch association is required")
                                .with_error());
                        };
                        if !has_custom_binding && binding.source_revision != revision {
                            return Ok(context
                                .result("capability declaration changed")
                                .with_error());
                        }
                        dispatcher.dispatch(&identity, &revision, context).await
                    })
                })
                .with_identity(evidence)
                .with_model_exposure(capability.tool.is_model_exposed())
                .with_host_dispatch(move |call| {
                    Ok(bound_tool.prepare_host_dispatch(call)?.unwrap_or_else(|| {
                        HostDispatchBinding {
                            invocation_id: uuid::Uuid::new_v4().to_string(),
                            source_revision: binding_revision.clone(),
                        }
                    }))
                })
            })
            .collect()
    }

    /// Callers own durable association/cancellation; names never substitute for exact identity.
    pub async fn dispatch(
        &self,
        identity: &str,
        expected_revision: &str,
        mut context: ToolExecutionContext,
    ) -> Result<ToolResult> {
        if context.host_dispatch().is_none() {
            return Ok(context
                .result("durable dispatch association is required")
                .with_error());
        }
        let _owner = PolicyOwner {
            policy: self.policy.clone(),
            operation: context
                .host_dispatch()
                .expect("checked association")
                .invocation_id
                .clone(),
        };
        if expected_revision != self.revision {
            return Ok(context
                .result("capability declaration changed")
                .with_error());
        }
        if self.session.as_ref() != context.session_id() {
            return Ok(context
                .result("capability execution context mismatch")
                .with_error());
        }
        if context.is_cancelled() {
            return Ok(context
                .result("capability operation cancelled before dispatch")
                .with_error());
        }
        let Some(capability) = self.entries.get(identity) else {
            return Ok(context
                .result("capability identity is unavailable")
                .with_error());
        };
        if context.call().name != capability.definition().name {
            return Ok(context
                .result("capability alias does not match identity")
                .with_error());
        }
        // Whole-capability admission precedes context middleware that may perform worker I/O.
        let policy = self
            .context
            .as_ref()
            .map(|(_, policy)| policy)
            .cloned()
            .unwrap_or_else(|| PermissionPolicy::new(crate::permission::PermissionLevel::Deny));
        match self
            .policy
            .authorize(&context, identity, &policy, &[])
            .await
        {
            Ok(None) => {}
            Ok(Some(message)) => return Ok(context.result(message).with_error()),
            Err(error) => return Ok(context.result(error.to_string()).with_error()),
        }
        if context.is_cancelled() {
            return Ok(context
                .result("capability operation cancelled")
                .with_error());
        }
        if let Some((chain, policy)) = &self.context {
            use super::middleware::{
                HookOutcome,
                context::{MiddlewareStateLog, ToolCallContext},
            };
            let mut state =
                MiddlewareStateLog::from_values(context.result_metadata().context_snapshot());
            let mut call = context.call().clone();
            let name = call.name.clone();
            let mut claims = Vec::new();
            let outcome = chain
                .before_tool_call(&mut ToolCallContext {
                    approved_tool_name: &name,
                    permission_policy: policy,
                    authorization: Some(super::middleware::context::LiveAuthorization {
                        context: context.clone(),
                        policy: self.policy.clone(),
                        identity: identity.into(),
                    }),
                    cancellation_token: context.cancellation_token().clone(),
                    session_id: context
                        .session_id()
                        .map_or("", waltr_agent::SessionId::as_str),
                    turn: context.turn().unwrap_or_default(),
                    call: &mut call,
                    permission_claims: &mut claims,
                    middleware_state: &mut state,
                })
                .await;
            state.flush_tool(context.result_metadata())?;
            if call.name != name {
                return Ok(context
                    .result("context changed the capability identity")
                    .with_error());
            }
            context.replace_arguments(call.arguments);
            {
                match self
                    .policy
                    .authorize(&context, identity, policy, &claims)
                    .await
                {
                    Ok(None) => {}
                    Ok(Some(message)) => return Ok(context.result(message).with_error()),
                    Err(error) => return Ok(context.result(error.to_string()).with_error()),
                }
            }
            match outcome {
                Ok(HookOutcome::Continue) => {}
                Ok(HookOutcome::ReplaceToolResult(result)) => return Ok(result),
                Err(error) => return Ok(context.result(error.to_string()).with_error()),
            }
        }
        let prepared = if let Some(sandbox) = &capability.sandbox {
            match sandbox
                .prepare_with_shell_defaults(
                    &context.call().name,
                    &context.call().arguments,
                    capability.shell_defaults(),
                )
                .await
            {
                Ok(prepared) => Some(prepared),
                Err(error) => return Ok(context.result(error.to_string()).with_error()),
            }
        } else {
            None
        };
        let claims = match prepared.as_ref().map(claims).transpose() {
            Ok(claims) => claims.unwrap_or_default(),
            Err(error) => {
                if let (Some(prepared), Some(sandbox)) = (prepared, &capability.sandbox) {
                    prepared.cancel(sandbox);
                }
                return Ok(context.result(error.to_string()).with_error());
            }
        };
        let decision = self
            .policy
            .authorize(&context, identity, &policy, &claims)
            .await;
        match decision {
            Ok(None) if !context.is_cancelled() => {}
            decision => {
                if let (Some(prepared), Some(sandbox)) = (prepared, &capability.sandbox) {
                    prepared.cancel(sandbox);
                }
                let message = match decision {
                    Ok(Some(message)) => message,
                    Ok(None) => "capability operation cancelled".into(),
                    Err(error) => error.to_string(),
                };
                return Ok(context.result(message).with_error());
            }
        }
        if let (Some(prepared), Some(sandbox)) = (prepared, &capability.sandbox) {
            if let Err(error) = context.check_execution().await {
                prepared.cancel(sandbox);
                return Ok(context.result(error.to_string()).with_error());
            }
            if let Err(error) = capability
                .tool
                .check_execution(context.cancellation_token().clone())
                .await
            {
                prepared.cancel(sandbox);
                return Ok(context.result(error.to_string()).with_error());
            }
            // The agent's neutral fence is durable before this future is polled.
            let output = sandbox
                .execute(
                    prepared,
                    context.cancellation_token().clone(),
                    context.progress().clone(),
                )
                .await?;
            let result = context.result(output.content);
            return Ok(if output.is_error {
                result.with_error()
            } else {
                result
            });
        }
        if let Err(error) = context.check_execution().await {
            return Ok(context.result(error.to_string()).with_error());
        }
        if let Err(error) = capability
            .tool
            .check_execution(context.cancellation_token().clone())
            .await
        {
            return Ok(context.result(error.to_string()).with_error());
        }
        capability.tool.call(context).await
    }
}

struct PolicyOwner {
    policy: Arc<dyn CapabilityPolicy>,
    operation: String,
}
impl Drop for PolicyOwner {
    fn drop(&mut self) {
        self.policy.finished(&self.operation);
    }
}

fn invalid(message: &str) -> LlmError {
    LlmError::Protocol {
        provider: "server-capability",
        message: message.into(),
    }
}

impl std::fmt::Debug for CapabilityDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityDispatcher")
            .field("revision", &self.revision)
            .field("capabilities", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use waltr_agent::{ToolCall, ToolDefinition};

    struct Allow;
    impl CapabilityPolicy for Allow {
        fn authorize<'a>(
            &'a self,
            _: &'a ToolExecutionContext,
            _: &'a str,
            _: &'a PermissionPolicy,
            _: &'a [PermissionClaim],
        ) -> PolicyFuture<'a> {
            Box::pin(async { Ok(None) })
        }
    }

    fn counted(effects: Arc<AtomicUsize>) -> Capability {
        Capability::new(
            ToolDefinition::new("effect", "effect", serde_json::json!({})),
            move |context| {
                effects.fetch_add(1, Ordering::SeqCst);
                Ok(context.result("done"))
            },
        )
    }

    #[test]
    fn duplicate_visible_names_are_rejected() {
        let effects = Arc::new(AtomicUsize::new(0));
        assert!(
            CapabilityDispatcher::new(
                vec![counted(effects.clone()), counted(effects)],
                None,
                Arc::new(Allow)
            )
            .is_err()
        );
    }

    #[test]
    fn declarations_and_opaque_identity_bind_registry_revision() {
        let one = CapabilityDispatcher::new(
            vec![counted(Arc::new(AtomicUsize::new(0)))],
            None,
            Arc::new(Allow),
        )
        .unwrap();
        let two = CapabilityDispatcher::new(
            vec![Capability::new(
                ToolDefinition::new("effect", "changed", serde_json::json!({})),
                |context| Ok(context.result("done")),
            )],
            None,
            Arc::new(Allow),
        )
        .unwrap();
        assert_ne!(one.revision(), two.revision());
        assert_eq!(one.agent_tools()[0].identity(), Some("builtin:effect"));
        assert!(one.agent_tools()[0].requires_host_dispatch());
    }

    #[tokio::test]
    async fn raw_handler_cannot_bypass_durable_association() {
        let effects = Arc::new(AtomicUsize::new(0));
        let dispatcher =
            CapabilityDispatcher::new(vec![counted(effects.clone())], None, Arc::new(Allow))
                .unwrap();
        let result = dispatcher
            .dispatch(
                "builtin:effect",
                dispatcher.revision(),
                ToolExecutionContext::new(ToolCall::new("call", "effect", serde_json::json!({}))),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn normal_model_adapter_does_not_execute_without_fence() {
        let effects = Arc::new(AtomicUsize::new(0));
        let dispatcher =
            CapabilityDispatcher::new(vec![counted(effects.clone())], None, Arc::new(Allow))
                .unwrap();
        let result = dispatcher.agent_tools()[0]
            .call(ToolExecutionContext::new(ToolCall::new(
                "call",
                "effect",
                serde_json::json!({}),
            )))
            .await;
        assert!(result.is_err());
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }
}
