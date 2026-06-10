//! Durable agent session factory.

use crate::{LlmStream, Result};

use super::{AgentSessionRuntime, ResumeInput, SessionId, SessionStore};
use crate::agent::{Agent, AgentRunBuilder, AgentSession};

/// Durable session factory bound to an agent configuration and store.
#[derive(Debug, Clone)]
pub struct AgentSessions<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    agent: Agent,
    store: S,
}

impl<S> AgentSessions<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a durable session factory.
    pub(crate) const fn new(agent: Agent, store: S) -> Self {
        Self { agent, store }
    }

    /// Create a new durable session with a generated id.
    pub async fn create(&self) -> Result<AgentSession> {
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime.create_session(SessionId::generate()).await?;
        Ok(AgentSession::new(record))
    }

    /// Load an existing durable session by id.
    pub async fn load(&self, id: &SessionId) -> Result<AgentSession> {
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime.load_session(id).await?;
        Ok(AgentSession::new(record))
    }

    /// Start a durable session turn from a user prompt.
    pub fn prompt<'a, P>(
        &'a self,
        provider: &'a P,
        session_id: SessionId,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<'a, P, S>
    where
        P: LlmStream + ?Sized,
    {
        self.agent
            .prompt(provider, prompt)
            .with_session_store(self.store.clone(), session_id)
    }

    /// Resume an existing durable session with explicit input.
    pub fn resume<'a, P>(
        &'a self,
        provider: &'a P,
        session_id: SessionId,
        input: ResumeInput,
    ) -> AgentRunBuilder<'a, P, S>
    where
        P: LlmStream + ?Sized,
    {
        AgentRunBuilder::resume(&self.agent, provider, session_id, self.store.clone(), input)
    }

    /// Return the agent configuration used by this factory.
    pub const fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Return the session store used by this factory.
    pub const fn store(&self) -> &S {
        &self.store
    }
}
