use crate::{
    agent_host::HostRuntimeBuilder, interaction::InteractionBroker,
    permission::ServerPermissionPolicy,
};
use futures_util::StreamExt;
use waltr_agent::{
    AgentRuntime, AgentTimelineEvent, MemorySessionStore, SessionCoordinator, SessionCreateRequest,
    SessionId,
};

pub struct HostSession {
    pub sessions: SessionCoordinator<MemorySessionStore>,
    pub id: SessionId,
    pub broker: InteractionBroker,
    pub runtime: AgentRuntime,
    pub cancellation: tokio_util::sync::CancellationToken,
}

impl HostSession {
    pub async fn new(builder: HostRuntimeBuilder) -> Self {
        let sessions = SessionCoordinator::new(MemorySessionStore::new());
        let id = sessions
            .create(SessionCreateRequest::new(SessionId::generate()))
            .await
            .unwrap()
            .id()
            .clone();
        let broker = InteractionBroker::new();
        let runtime = builder
            .with_dispatch_context(
                Some(id.clone()),
                ServerPermissionPolicy::sessions(broker.clone()),
            )
            .build();
        Self {
            sessions,
            id,
            broker,
            runtime,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn start(
        &self,
        prompt: impl Into<String>,
    ) -> tokio::task::JoinHandle<Vec<waltr_agent::Result<AgentTimelineEvent>>> {
        let sessions = self.sessions.clone();
        let id = self.id.clone();
        let runtime = self.runtime.clone();
        let prompt = prompt.into();
        let cancellation = self.cancellation.child_token();
        tokio::spawn(async move {
            sessions
                .prompt(&runtime, id, prompt)
                .with_cancellation_token(cancellation)
                .stream()
                .collect()
                .await
        })
    }

    pub fn prompt(
        &self,
        prompt: impl Into<String>,
    ) -> waltr_agent::AgentRunBuilder<MemorySessionStore> {
        self.sessions.prompt(&self.runtime, self.id.clone(), prompt)
    }

    pub async fn close(self) {
        self.sessions.shutdown().await;
    }
}

impl std::ops::Deref for HostSession {
    type Target = AgentRuntime;
    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}
