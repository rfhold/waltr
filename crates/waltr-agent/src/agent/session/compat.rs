//! Compatibility input for ephemeral session-backed runs.

use crate::{LlmStream, ModelRef, PermissionPolicy};

use super::{ResumeInput, SessionId, SessionStore};
use crate::middleware::chain::MiddlewareChain;

use super::super::{AgentTool, AgentToolContext};

pub(crate) struct EphemeralSessionRun<'a, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(crate) provider: &'a P,
    pub(crate) model: ModelRef,
    pub(crate) input: ResumeInput,
    pub(crate) system_prompt: Option<String>,
    pub(crate) tools: Vec<AgentTool>,
    pub(crate) middleware_chain: MiddlewareChain,
    pub(crate) tool_context: AgentToolContext,
    pub(crate) permission_policy: PermissionPolicy,
    pub(crate) session_id: SessionId,
    pub(crate) session_store: S,
}

impl<'a, P, S> EphemeralSessionRun<'a, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(crate) fn new(
        provider: &'a P,
        model: ModelRef,
        input: ResumeInput,
        system_prompt: Option<String>,
        tools: Vec<AgentTool>,
        middleware_chain: MiddlewareChain,
        tool_context: AgentToolContext,
        permission_policy: PermissionPolicy,
        session_id: SessionId,
        session_store: S,
    ) -> Self {
        Self {
            provider,
            model,
            input,
            system_prompt,
            tools,
            middleware_chain,
            tool_context,
            permission_policy,
            session_id,
            session_store,
        }
    }
}
