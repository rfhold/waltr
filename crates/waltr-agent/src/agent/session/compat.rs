//! Compatibility input for ephemeral session-backed runs.

use std::sync::Arc;

use crate::{LlmStream, ModelCost, ModelRef, PermissionPolicy};
use tokio_util::sync::CancellationToken;

use super::watch::{SessionListBroadcaster, SessionTimelineBroadcaster};
use super::{ResumeInput, SessionId, SessionStore};
use crate::middleware::{chain::MiddlewareChain, prompt::SystemPromptOverride};

use super::super::{AgentTool, ProviderRetryConfig};

pub struct EphemeralSessionRun<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub provider: Arc<dyn LlmStream>,
    pub model: ModelRef,
    pub model_cost: Option<ModelCost>,
    pub input: ResumeInput,
    pub system_prompt_override: Option<SystemPromptOverride>,
    pub tools: Vec<AgentTool>,
    pub middleware_chain: MiddlewareChain,
    pub permission_policy: PermissionPolicy,
    pub retry_config: ProviderRetryConfig,
    pub session_id: SessionId,
    pub session_store: S,
    pub session_broadcaster: Option<SessionListBroadcaster>,
    pub timeline_broadcaster: Option<SessionTimelineBroadcaster>,
    pub cancellation_token: CancellationToken,
}
