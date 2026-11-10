//! Session-owned connections with serialized preparation and joined retirement.

use super::super::runtime_factory::{
    McpRuntimeFingerprint, PreparedMcpRuntime, ServerAgentRuntimeFactory,
};
use super::{McpClientRuntime, runtime::McpRuntimeStartError};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use waltr_agent::{
    AgentSessionRuntimeError, SessionCoordinator, SessionId, SessionRecord, SessionStore,
};

#[derive(Clone)]
struct Cached {
    runtime: Arc<McpClientRuntime>,
    incarnation: String,
    fingerprint: McpRuntimeFingerprint,
    credentialed: BTreeSet<String>,
}

#[derive(Default)]
pub struct ServerMcpRegistry {
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    preparations: Mutex<Vec<Arc<Retirement>>>,
    entries: Mutex<BTreeMap<SessionId, Cached>>,
    preparation: Mutex<BTreeMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
    retired: Mutex<BTreeMap<SessionId, Vec<Arc<Retirement>>>>,
}

struct Retirement {
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<waltr_agent::Result<()>>>>,
    outcome: Mutex<Option<Result<(), String>>>,
}

impl Retirement {
    async fn join(&self) -> waltr_agent::Result<()> {
        let mut task = self.task.lock().await;
        if let Some(task) = task.as_mut() {
            let result = task
                .await
                .unwrap_or_else(|_| Err(retirement_error("MCP retirement task failed")));
            *self
                .outcome
                .lock()
                .expect("MCP retirement outcome lock poisoned") =
                Some(result.map_err(|error| error.to_string()));
        }
        task.take();
        self.outcome
            .lock()
            .expect("MCP retirement outcome lock poisoned")
            .as_ref()
            .expect("joined MCP retirement outcome")
            .clone()
            .map_err(retirement_error)
    }
}

fn retirement_error(message: impl Into<String>) -> waltr_agent::LlmError {
    waltr_agent::LlmError::Protocol {
        provider: "mcp",
        message: message.into(),
    }
}

impl ServerMcpRegistry {
    pub(crate) async fn runtime<S>(
        self: &Arc<Self>,
        factory: &ServerAgentRuntimeFactory,
        record: &SessionRecord,
        sessions: &SessionCoordinator<S>,
    ) -> Result<Option<Arc<McpClientRuntime>>, AgentSessionRuntimeError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let lifecycle = Arc::new(self.lifecycle.clone().read_owned().await);
        self.reap_preparations().await?;
        let owner = self.clone();
        let factory = factory.clone();
        let record = record.clone();
        let sessions = sessions.clone();
        let (reply, receive) = tokio::sync::oneshot::channel();
        let task_lifecycle = lifecycle.clone();
        let task = tokio::spawn(async move {
            let _lifecycle = task_lifecycle;
            let result = owner.prepare(&factory, &record, &sessions).await;
            drop(reply.send(result));
            Ok(())
        });
        self.preparations
            .lock()
            .expect("MCP preparation tasks lock poisoned")
            .push(Arc::new(Retirement {
                task: tokio::sync::Mutex::new(Some(task)),
                outcome: Mutex::new(None),
            }));
        drop(lifecycle);
        receive.await.map_err(|_| {
            AgentSessionRuntimeError::Internal("MCP preparation completion unavailable".into())
        })?
    }

    async fn prepare<S>(
        &self,
        factory: &ServerAgentRuntimeFactory,
        record: &SessionRecord,
        sessions: &SessionCoordinator<S>,
    ) -> Result<Option<Arc<McpClientRuntime>>, AgentSessionRuntimeError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let incarnation = record
            .metadata
            .incarnation
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or_else(stale)?;
        let guard = self
            .preparation
            .lock()
            .expect("MCP preparation lock poisoned")
            .entry(record.id.clone())
            .or_default()
            .clone();
        let _preparation = guard.lock().await;
        let prepared = match factory.mcp_feature_for_session(record)? {
            Some(feature) => factory.prepare_mcp_runtime(&feature).await?,
            None => None,
        };
        validate_record(sessions, record).await?;
        let Some(prepared) = prepared else {
            self.evict(&record.id);
            return Ok(None);
        };
        let cached = self
            .entries
            .lock()
            .expect("MCP registry lock poisoned")
            .get(&record.id)
            .cloned();
        if let Some(cached) = cached.filter(|cached| {
            cached.incarnation == incarnation
                && cached.fingerprint == prepared.fingerprint
                && cached.credentialed == prepared.credentialed_servers
                && cached.runtime.app_sources() == prepared.config.app_sources()
        }) {
            factory
                .preflight_cached_mcp_authentication(&prepared.connections, &cached.credentialed)
                .await?;
            validate_record(sessions, record).await?;
            return Ok(Some(cached.runtime));
        }
        let root = factory.mcp_root_uri()?;
        let runtime =
            McpClientRuntime::start(prepared.config.clone(), root, CancellationToken::new())
                .await
                .map_err(|error| start_error(error, &prepared))?;
        if let Err(error) = validate_record(sessions, record).await {
            drop(runtime.shutdown().await);
            return Err(error);
        }
        let replaced = self
            .entries
            .lock()
            .expect("MCP registry lock poisoned")
            .insert(
                record.id.clone(),
                Cached {
                    runtime: runtime.clone(),
                    incarnation,
                    fingerprint: prepared.fingerprint,
                    credentialed: prepared.credentialed_servers,
                },
            );
        if let Some(replaced) = replaced {
            self.enqueue(record.id.clone(), replaced.runtime);
        }
        Ok(Some(runtime))
    }

    fn enqueue(&self, session: SessionId, runtime: Arc<McpClientRuntime>) {
        let task = tokio::spawn(async move { runtime.shutdown().await });
        let retirement = Arc::new(Retirement {
            task: tokio::sync::Mutex::new(Some(task)),
            outcome: Mutex::new(None),
        });
        self.retired
            .lock()
            .expect("MCP retirement lock poisoned")
            .entry(session)
            .or_default()
            .push(retirement);
    }

    fn evict(&self, session: &SessionId) {
        let cached = self
            .entries
            .lock()
            .expect("MCP registry lock poisoned")
            .remove(session);
        if let Some(cached) = cached {
            self.enqueue(session.clone(), cached.runtime);
        }
    }

    pub async fn retire(&self, session: &SessionId) -> waltr_agent::Result<()> {
        let _lifecycle = self.lifecycle.write().await;
        let preparation = self.join_preparations().await;
        let retirement = self.retire_inner(session).await;
        preparation.and(retirement)
    }

    async fn retire_inner(&self, session: &SessionId) -> waltr_agent::Result<()> {
        self.evict(session);
        self.preparation
            .lock()
            .expect("MCP preparation lock poisoned")
            .remove(session);
        let tasks = self
            .retired
            .lock()
            .expect("MCP retirement lock poisoned")
            .get(session)
            .cloned()
            .unwrap_or_default();
        let mut failure = None;
        for task in tasks {
            let result = task.join().await;
            if let Err(error) = result {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        self.retired
            .lock()
            .expect("MCP retirement lock poisoned")
            .remove(session);
        failure.map_or(Ok(()), Err)
    }

    pub async fn retire_all(&self) -> waltr_agent::Result<()> {
        let _lifecycle = self.lifecycle.write().await;
        let preparation = self.join_preparations().await;
        let mut sessions: BTreeSet<_> = self
            .entries
            .lock()
            .expect("MCP registry lock poisoned")
            .keys()
            .cloned()
            .collect();
        sessions.extend(
            self.retired
                .lock()
                .expect("MCP retirement lock poisoned")
                .keys()
                .cloned(),
        );
        let mut failure = preparation.err();
        for session in sessions {
            if let Err(error) = self.retire_inner(&session).await {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    async fn join_preparations(&self) -> waltr_agent::Result<()> {
        let tasks = self
            .preparations
            .lock()
            .expect("MCP preparation tasks lock poisoned")
            .clone();
        let mut failure = None;
        for task in &tasks {
            if let Err(error) = task.join().await {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        self.preparations
            .lock()
            .expect("MCP preparation tasks lock poisoned")
            .retain(|current| !tasks.iter().any(|joined| Arc::ptr_eq(current, joined)));
        failure.map_or(Ok(()), Err)
    }

    async fn reap_preparations(&self) -> Result<(), AgentSessionRuntimeError> {
        let finished = self
            .preparations
            .lock()
            .expect("MCP preparation tasks lock poisoned")
            .iter()
            .filter(|retirement| {
                retirement.task.try_lock().is_ok_and(|task| {
                    task.as_ref()
                        .is_none_or(tokio::task::JoinHandle::is_finished)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        for task in &finished {
            task.join().await.map_err(AgentSessionRuntimeError::from)?;
        }
        self.preparations
            .lock()
            .expect("MCP preparation tasks lock poisoned")
            .retain(|current| !finished.iter().any(|joined| Arc::ptr_eq(current, joined)));
        Ok(())
    }
}

async fn validate_record<S>(
    sessions: &SessionCoordinator<S>,
    expected: &SessionRecord,
) -> Result<(), AgentSessionRuntimeError>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    let current = sessions
        .store()
        .load(&expected.id)
        .await
        .map_err(|_| stale())?;
    if current.metadata.incarnation != expected.metadata.incarnation
        || current.agent_definition != expected.agent_definition
    {
        return Err(stale());
    }
    Ok(())
}

fn stale() -> AgentSessionRuntimeError {
    AgentSessionRuntimeError::FailedPrecondition(
        "session changed during MCP runtime preparation".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retirement_join_keeps_the_owned_task_when_its_waiter_is_dropped() {
        use futures_util::FutureExt as _;
        let (release, wait) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = wait.await;
            Ok(())
        });
        let retirement = Retirement {
            task: tokio::sync::Mutex::new(Some(task)),
            outcome: Mutex::new(None),
        };
        assert!(retirement.join().now_or_never().is_none());
        release.send(()).unwrap();
        retirement.join().await.unwrap();
        retirement.join().await.unwrap();
    }

    #[tokio::test]
    async fn retirement_joins_without_waiting_for_lifecycle_owner_or_handler_arcs() {
        struct Empty;
        impl mcp::server::ServerHandler for Empty {
            fn capabilities(&self) -> mcp::server::ServerCapabilities {
                mcp::server::ServerCapabilities::default()
            }
            fn server_info(&self) -> mcp::server::ServerInfo {
                mcp::server::ServerInfo::new("empty", "1")
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let router = mcp::server::streamable_http_router(Arc::new(Empty));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let runtime = McpClientRuntime::start(
            super::super::McpConfig::new().with_server(
                super::super::McpServerConfig::streamable_http("upstream", endpoint),
            ),
            "file:///test".into(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let retained = runtime.clone();
        let token = retained.lifetime();
        let registry = ServerMcpRegistry::default();
        let session = SessionId::generate();
        registry.enqueue(session.clone(), runtime);
        tokio::time::timeout(std::time::Duration::from_secs(5), registry.retire(&session))
            .await
            .unwrap()
            .unwrap();
        assert!(token.is_cancelled());
        assert!(Arc::strong_count(&retained) >= 1);
        server.abort();
    }
    #[tokio::test]
    async fn deleted_recreated_or_reconfigured_sessions_cannot_receive_prepared_connections() {
        let sessions = SessionCoordinator::new(waltr_agent::MemorySessionStore::new());
        let session = SessionId::generate();
        sessions
            .create(waltr_agent::SessionCreateRequest::new(session.clone()))
            .await
            .unwrap();
        let original = sessions.store().load(&session).await.unwrap();
        validate_record(&sessions, &original).await.unwrap();
        let mut different = original.clone();
        different.agent_definition = Some(serde_json::json!({"other":true}));
        assert!(validate_record(&sessions, &different).await.is_err());
        sessions.delete(&session).await.unwrap();
        assert!(validate_record(&sessions, &original).await.is_err());
        sessions
            .create(waltr_agent::SessionCreateRequest::new(session))
            .await
            .unwrap();
        assert!(validate_record(&sessions, &original).await.is_err());
    }
}

fn start_error(
    error: McpRuntimeStartError,
    prepared: &PreparedMcpRuntime,
) -> AgentSessionRuntimeError {
    use waltr_agent::{AuthenticationFailureReason, LlmError};
    match error.error {
        LlmError::AuthenticationFailure {
            mut failure,
            message,
        } => {
            failure.target = crate::auth::AuthTarget::mcp_server(&error.server_key).into();
            if let Some(context) = failure
                .host_context
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                let endpoint = prepared
                    .connections
                    .iter()
                    .find(|connection| connection.key == error.server_key)
                    .map_or_else(String::new, |connection| connection.url.clone());
                context.insert("endpoint".into(), endpoint.into());
            }
            AgentSessionRuntimeError::Authentication {
                target: failure.target,
                message,
                reason: failure.reason,
                host_context: failure.host_context,
            }
        }
        LlmError::Authorization { message, .. } => AgentSessionRuntimeError::Authentication {
            target: crate::auth::AuthTarget::mcp_server(error.server_key).into(),
            message,
            reason: AuthenticationFailureReason::CredentialRequired,
            host_context: None,
        },
        error => AgentSessionRuntimeError::from(error),
    }
}
