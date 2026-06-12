//! Compatibility input for ephemeral session-backed runs.

use crate::{LlmStream, ModelRef, PermissionPolicy};

use super::{ResumeInput, SessionId, SessionStore};
use crate::middleware::{chain::MiddlewareChain, prompt::SystemPromptOverride};

use super::super::AgentTool;

pub struct EphemeralSessionRun<'a, P, S>
where
    P: LlmStream + ?Sized,
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub provider: &'a P,
    pub model: ModelRef,
    pub input: ResumeInput,
    pub system_prompt_override: Option<SystemPromptOverride>,
    pub tools: Vec<AgentTool>,
    pub middleware_chain: MiddlewareChain,
    pub permission_policy: PermissionPolicy,
    pub session_id: SessionId,
    pub session_store: S,
}
