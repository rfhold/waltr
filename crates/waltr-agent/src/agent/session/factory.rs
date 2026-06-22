//! Durable agent session factory.

use std::collections::BTreeMap;

use crate::Result;
use serde_json::Value;

use super::watch::{SessionListBroadcaster, SessionListEvent};
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
        self.create_with_metadata_values(BTreeMap::new()).await
    }

    /// Create a new durable session with initial host-defined metadata values.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store rejects the new record.
    pub async fn create_with_metadata_values(
        &self,
        metadata_values: BTreeMap<String, Value>,
    ) -> Result<AgentSession> {
        let runtime = AgentSessionRuntime::new(&self.store);
        let record = runtime
            .create_session_with_metadata_values(SessionId::generate(), metadata_values)
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Delete a durable session if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the session store cannot complete the delete operation.
    pub async fn delete(&self, id: &SessionId) -> Result<()> {
        AgentSessionRuntime::new(&self.store)
            .delete_session(id)
            .await?;
        self.sessions.publish(SessionListEvent::Deleted(id.clone()));
        Ok(())
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

    /// Set a host-defined metadata value and publish an updated session summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn set_metadata_value(
        &self,
        id: &SessionId,
        key: impl Into<String>,
        value: Value,
    ) -> Result<AgentSession> {
        let record = AgentSessionRuntime::new(&self.store)
            .set_metadata_value(id, key.into(), value)
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
        Ok(AgentSession::new(record))
    }

    /// Remove a host-defined metadata value and publish an updated session summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not exist or the store fails.
    pub async fn remove_metadata_value(
        &self,
        id: &SessionId,
        key: impl Into<String>,
    ) -> Result<AgentSession> {
        let record = AgentSessionRuntime::new(&self.store)
            .remove_metadata_value(id, key.into())
            .await?;
        self.sessions
            .publish(SessionListEvent::Upsert(record.summary()));
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

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;
    use serde_json::json;

    use super::*;
    use crate::agent::session::{MemorySessionStore, SessionListEvent};

    #[tokio::test]
    async fn metadata_update_publishes_session_upsert() {
        let sessions = AgentSessions::new(MemorySessionStore::new());
        let session = sessions.create().await.unwrap();
        let mut watch = sessions.watch_sessions();

        let updated = sessions
            .set_metadata_value(&session.record.id, "host.key", json!("value"))
            .await
            .unwrap();
        let update = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
            .await
            .unwrap()
            .expect("metadata update should publish");

        assert!(update.sequence > 0);
        match update.event {
            SessionListEvent::Upsert(summary) => {
                assert_eq!(summary.id, session.record.id);
                assert_eq!(summary.revision, updated.record.revision);
                assert_eq!(
                    summary.metadata_values.get("host.key"),
                    Some(&json!("value"))
                );
            }
            SessionListEvent::Deleted(id) => panic!("expected upsert, got deleted {id:?}"),
        }
    }
}
