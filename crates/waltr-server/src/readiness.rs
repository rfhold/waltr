//! Core readiness and independent feature diagnostics.
use crate::{
    DefinitionWatcherHealth, mcp_oauth::McpOAuthServer, persistence::Persistence,
    routine::production::RuntimeHealth,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use waltr::grpc::waltr::status::v1::{FeatureHealth, GetFeatureStatusResponse};

const DATABASE_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub(crate) struct Readiness {
    pub shutdown: CancellationToken,
    pub persistence: Persistence,
    pub oauth: Option<McpOAuthServer>,
    pub definitions: tokio::sync::watch::Receiver<DefinitionWatcherHealth>,
    pub routines: tokio::sync::watch::Receiver<RuntimeHealth>,
    pub invocations: Option<crate::routine::service::InvocationService>,
    pub sandbox_alive: Arc<AtomicBool>,
}

impl Readiness {
    pub(crate) async fn is_ready(&self) -> bool {
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => false,
            result = tokio::time::timeout(DATABASE_PROBE_TIMEOUT, self.persistence.ready()) =>
                matches!(result, Ok(Ok(()))),
        }
    }

    pub(crate) async fn features(&self) -> GetFeatureStatusResponse {
        let hosted_mcp_oauth = match &self.oauth {
            Some(oauth) => health(matches!(
                tokio::time::timeout(DATABASE_PROBE_TIMEOUT, oauth.ready()).await,
                Ok(Ok(()))
            )),
            None => FeatureHealth::NotConfigured,
        };
        let definition_watcher = health(crate::app::definition_watcher_ready(&self.definitions));
        let default_worker = health(self.sandbox_alive.load(Ordering::Acquire));
        let routine_catalog = health(crate::app::routine_engine_ready(&self.routines));
        let routine_invocations = self
            .invocations
            .as_ref()
            .map_or(FeatureHealth::NotConfigured, |service| {
                health(service.feature_healthy())
            });
        tracing::debug!(
            ?definition_watcher,
            ?default_worker,
            ?routine_catalog,
            ?routine_invocations,
            ?hosted_mcp_oauth,
            "server feature status"
        );
        GetFeatureStatusResponse {
            definition_watcher: definition_watcher.into(),
            default_worker: default_worker.into(),
            routine_catalog: routine_catalog.into(),
            routine_invocations: routine_invocations.into(),
            hosted_mcp_oauth: hosted_mcp_oauth.into(),
        }
    }
}

fn health(healthy: bool) -> FeatureHealth {
    if healthy {
        FeatureHealth::Healthy
    } else {
        FeatureHealth::Degraded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn core_readiness_ignores_feature_failures_but_requires_its_database() {
        let persistence =
            Persistence::initialize(crate::persistence::DatabaseConfig::for_tests().unwrap())
                .await
                .unwrap();
        let (definition_tx, definitions) =
            tokio::sync::watch::channel(DefinitionWatcherHealth::Running);
        let (routine_tx, routines) = tokio::sync::watch::channel(RuntimeHealth::Running);
        let readiness = Readiness {
            shutdown: CancellationToken::new(),
            persistence,
            oauth: None,
            definitions,
            routines,
            invocations: None,
            sandbox_alive: Arc::new(AtomicBool::new(false)),
        };
        drop(definition_tx);
        drop(routine_tx);
        readiness
            .persistence
            .mcp_sqlite_pool()
            .unwrap()
            .close()
            .await;
        assert!(readiness.is_ready().await);
        let features = readiness.features().await;
        assert_eq!(features.definition_watcher(), FeatureHealth::Degraded);
        assert_eq!(features.default_worker(), FeatureHealth::Degraded);
        assert_eq!(features.routine_catalog(), FeatureHealth::Degraded);
        assert_eq!(features.hosted_mcp_oauth(), FeatureHealth::NotConfigured);
        readiness.persistence.sqlite_pool().unwrap().close().await;
        assert!(!readiness.is_ready().await);
    }
}
