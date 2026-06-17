//! Durable agent session factory.

use crate::Result;

use super::watch::SessionListBroadcaster;
use super::{
    AgentSessionRuntime, ResumeInput, SessionId, SessionListSubscription, SessionStore,
    SessionSummary,
};
use crate::agent::{AgentRunBuilder, AgentRuntime, AgentSession};

/// Durable session factory bound to a store.
#[derive(Debug, Clone)]
pub struct AgentSessions<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    store: S,
    sessions: SessionListBroadcaster,
}

impl<S> AgentSessions<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    /// Create a durable session factory.
    pub fn new(store: S) -> Self {
        Self {
            store,
            sessions: SessionListBroadcaster::new(),
        }
    }

    /// Create a new durable session with a generated id.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    pub async fn create(&self) -> Result<AgentSession> {
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime.create_session(SessionId::generate()).await?;
        self.sessions.publish(record.summary());
        Ok(AgentSession::new(record))
    }

    /// Watch newly created or updated sessions.
    #[must_use]
    pub fn watch_sessions(&self) -> SessionListSubscription {
        self.sessions.subscribe()
    }

    /// Load an existing durable session by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn load(&self, id: &SessionId) -> Result<AgentSession> {
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime.load_session(id).await?;
        Ok(AgentSession::new(record))
    }

    /// List durable sessions, newest activity first.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store cannot list sessions.
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        AgentSessionRuntime::new(&self.store).list_sessions().await
    }

    /// Persist an interruption for an existing durable session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn interrupt(&self, id: &SessionId) -> Result<AgentSession> {
        let record = AgentSessionRuntime::new(&self.store).interrupt(id).await?;
        Ok(AgentSession::new(record))
    }

    /// Start a durable session turn from a user prompt.
    pub fn prompt(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        prompt: impl Into<String>,
    ) -> AgentRunBuilder<S> {
        runtime
            .prompt(prompt)
            .with_session_store(self.store.clone(), session_id)
    }

    /// Resume an existing durable session with explicit input.
    pub fn resume(
        &self,
        runtime: &AgentRuntime,
        session_id: SessionId,
        input: ResumeInput,
    ) -> AgentRunBuilder<S> {
        AgentRunBuilder::resume(runtime, session_id, self.store.clone(), input)
    }

    /// Return the session store used by this factory.
    pub const fn store(&self) -> &S {
        &self.store
    }
}
