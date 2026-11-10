//! One read-only readiness snapshot shared by HTTP and MCP status.
use crate::{
    DefinitionWatcherHealth, mcp_oauth::McpOAuthServer, persistence::Persistence,
    routine::production::RuntimeHealth,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug)]
pub(crate) struct Readiness {
    pub persistence: Persistence,
    pub oauth: Option<McpOAuthServer>,
    pub definitions: tokio::sync::watch::Receiver<DefinitionWatcherHealth>,
    pub routines: tokio::sync::watch::Receiver<RuntimeHealth>,
    pub invocations: Option<crate::routine::service::InvocationService>,
    pub sandbox_alive: Arc<AtomicBool>,
}

impl Readiness {
    pub(crate) async fn is_ready(&self) -> bool {
        crate::app::definition_watcher_ready(&self.definitions)
            && self.sandbox_alive.load(Ordering::Acquire)
            && crate::app::routine_engine_ready(&self.routines)
            && self
                .invocations
                .as_ref()
                .is_none_or(crate::routine::service::InvocationService::is_ready)
            && self.persistence.ready().await.is_ok()
            && match &self.oauth {
                Some(oauth) => oauth.ready().await.is_ok(),
                None => true,
            }
    }
}
