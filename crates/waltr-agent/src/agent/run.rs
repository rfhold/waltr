//! Per-run agent configuration.

use std::fmt;

use futures_util::stream;
use tokio_util::sync::CancellationToken;

use crate::PermissionPolicy;

use super::session::{
    EphemeralSessionRun, MemorySessionStore, ResumeInput, SessionId, SessionListBroadcaster,
    SessionStore, SessionTimelineBroadcaster,
};
use super::{AgentRuntime, AgentTool, BoxAgentTimelineStream, r#loop::AgentLoopState};
use crate::middleware::{
    chain::MiddlewareChain, prompt::SystemPromptOverride,
    tools::PermissionFilteredToolRegistryMiddleware,
};

/// Builder for an agent loop request.
pub struct AgentRunBuilder<S = MemorySessionStore>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    runtime: AgentRuntime,
    prompt: String,
    input: ResumeInput,
    session_id: SessionId,
    session_store: S,
    session_broadcaster: Option<SessionListBroadcaster>,
    timeline_broadcaster: Option<SessionTimelineBroadcaster>,
    system_prompt: SystemPromptOverride,
    cancellation_token: Option<CancellationToken>,
}

impl AgentRunBuilder {
    pub(super) fn new(runtime: &AgentRuntime, prompt: String) -> Self {
        Self {
            runtime: runtime.clone(),
            prompt: prompt.clone(),
            input: ResumeInput::Prompt(prompt),
            session_id: SessionId::new("ephemeral"),
            session_store: MemorySessionStore::new(),
            session_broadcaster: None,
            timeline_broadcaster: None,
            system_prompt: SystemPromptOverride::default(),
            cancellation_token: None,
        }
    }

    pub(super) fn resume<S>(
        runtime: &AgentRuntime,
        session_id: SessionId,
        session_store: S,
        input: ResumeInput,
    ) -> AgentRunBuilder<S>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let prompt = match &input {
            ResumeInput::Prompt(prompt) => prompt.clone(),
            ResumeInput::ToolResults(_) | ResumeInput::ExternalResponse { .. } => String::new(),
        };
        AgentRunBuilder {
            runtime: runtime.clone(),
            prompt,
            input,
            session_id,
            session_store,
            session_broadcaster: None,
            timeline_broadcaster: None,
            system_prompt: SystemPromptOverride::default(),
            cancellation_token: None,
        }
    }
}

impl<S> AgentRunBuilder<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Add one tool to this run.
    #[must_use]
    pub fn with_tool(mut self, tool: AgentTool) -> Self {
        self.runtime.tools.push(tool);
        self
    }

    /// Replace the agent-configured system prompt for this run.
    #[must_use]
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt.replace(prompt);
        self
    }

    /// Append a system prompt fragment for this run.
    #[must_use]
    pub fn with_system_prompt_fragment(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt.push_fragment(prompt);
        self
    }

    /// Override the permission policy for this run.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.runtime.permission_policy = policy;
        self
    }

    /// Stop this run when the supplied cancellation token is canceled.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    pub(crate) fn with_timeline_broadcaster(
        mut self,
        timeline_broadcaster: SessionTimelineBroadcaster,
    ) -> Self {
        self.timeline_broadcaster = Some(timeline_broadcaster);
        self
    }

    pub(crate) fn with_session_broadcaster(
        mut self,
        session_broadcaster: SessionListBroadcaster,
    ) -> Self {
        self.session_broadcaster = Some(session_broadcaster);
        self
    }

    /// Persist this run in the supplied session store and session id.
    #[must_use]
    pub fn with_session_store<T>(self, store: T, id: SessionId) -> AgentRunBuilder<T>
    where
        T: SessionStore + Clone + Send + Sync + 'static,
    {
        let Self {
            runtime,
            prompt,
            input,
            session_id: _,
            session_store: _,
            session_broadcaster,
            timeline_broadcaster,
            system_prompt,
            cancellation_token,
        } = self;
        AgentRunBuilder {
            runtime,
            prompt,
            input,
            session_id: id,
            session_store: store,
            session_broadcaster,
            timeline_broadcaster,
            system_prompt,
            cancellation_token,
        }
    }

    /// Stream the full ordered agent timeline.
    #[must_use]
    pub fn stream(self) -> BoxAgentTimelineStream<'static> {
        let run = self.into_ephemeral_session_run();
        Box::pin(stream::unfold(
            AgentLoopState::from_ephemeral_session(run),
            AgentLoopState::next,
        ))
    }

    fn into_ephemeral_session_run(self) -> EphemeralSessionRun<S> {
        let AgentRuntime {
            provider,
            model,
            tools,
            middleware_chain: default_chain,
            permission_policy,
        } = self.runtime;
        let system_prompt = self.system_prompt;

        let tool_definitions = tools.iter().map(|tool| tool.definition().clone()).collect();
        let mut middleware_chain = MiddlewareChain::new();
        middleware_chain.push(PermissionFilteredToolRegistryMiddleware::new(
            tool_definitions,
            permission_policy.clone(),
            true,
        ));
        middleware_chain.extend(default_chain);

        EphemeralSessionRun {
            provider,
            model,
            input: self.input,
            system_prompt_override: (!system_prompt.is_empty()).then_some(system_prompt),
            tools,
            middleware_chain,
            permission_policy,
            session_id: self.session_id,
            session_store: self.session_store,
            session_broadcaster: self.session_broadcaster,
            timeline_broadcaster: self.timeline_broadcaster,
            cancellation_token: self.cancellation_token.unwrap_or_default(),
        }
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl<S> fmt::Debug for AgentRunBuilder<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRunBuilder")
            .field("runtime", &self.runtime)
            .field("prompt", &self.prompt)
            .field("system_prompt", &self.system_prompt)
            .field("session_broadcaster", &self.session_broadcaster.is_some())
            .field("timeline_broadcaster", &self.timeline_broadcaster.is_some())
            .field("cancellation_token", &self.cancellation_token.is_some())
            .finish()
    }
}
