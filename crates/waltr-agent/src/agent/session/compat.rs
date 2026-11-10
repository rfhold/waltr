//! Compatibility input for ephemeral session-backed runs.

use std::sync::Arc;

use crate::{LlmStream, ModelCost, ModelRef};
use tokio_util::sync::CancellationToken;

use super::{ResumeInput, SessionId, SessionRunContext, SessionStore};
use crate::middleware::{chain::MiddlewareChain, prompt::SystemPromptOverride};

use super::super::{AgentTool, ProviderRetryConfig};
use crate::SessionEventSink;

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
    pub retry_config: ProviderRetryConfig,
    pub max_provider_turns: Option<u64>,
    pub(crate) host_context: Option<Arc<dyn crate::AgentHostContext>>,
    pub session_id: SessionId,
    pub session_store: S,
    pub session_run_context: Option<SessionRunContext>,
    pub cancellation_token: CancellationToken,
    pub event_sink: Option<Arc<dyn SessionEventSink>>,
    pub completion_validation: Option<crate::agent::completion::CompletionValidation>,
}
