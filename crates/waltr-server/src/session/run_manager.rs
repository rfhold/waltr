use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::StreamExt as _;
use waltr::grpc::waltr::config::v1::Configuration;
use waltr_agent::{
    AgentSessions, ExternalRequestId, ExternalResponse, ResumeInput, SessionId, SessionStore,
};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use super::{error::agent_error_to_status, runtime::ServerRuntimeFactory};

pub(super) struct SessionRunRequest<S>
where
    S: SessionStore + Clone + Send + Sync + 'static,
{
    pub(super) sessions: AgentSessions<S>,
    pub(super) runtime_factory: ServerRuntimeFactory,
    pub(super) config: Configuration,
    pub(super) session_id: SessionId,
}

#[derive(Debug, Clone)]
pub(super) struct SessionRunManager {
    active: Arc<Mutex<BTreeMap<String, ActiveRun>>>,
    next_run_id: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
struct ActiveRun {
    id: u64,
    token: CancellationToken,
}

impl SessionRunManager {
    pub(super) fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(BTreeMap::new())),
            next_run_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) async fn submit_user_message<S>(
        &self,
        request: SessionRunRequest<S>,
        text: String,
    ) -> Result<(), Status>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let session = request
            .sessions
            .load(&request.session_id)
            .await
            .map_err(agent_error_to_status)?;
        let runtime = request
            .runtime_factory
            .build(&session.record.summary(), &request.config)
            .await?;
        let active = self.replace_active(&request.session_id).await;
        let sessions = request.sessions;
        let session_id = request.session_id;
        let manager = self.clone();
        tokio::spawn(async move {
            let mut stream = sessions
                .prompt(&runtime, session_id.clone(), text)
                .with_cancellation_token(active.token.clone())
                .stream();
            while let Some(event) = stream.next().await {
                if let Err(error) = event {
                    tracing::warn!(session_id = %session_id.as_str(), %error, "session run failed");
                    break;
                }
            }
            manager.remove_active(&session_id, active.id).await;
        });
        Ok(())
    }

    pub(super) async fn respond_external_request<S>(
        &self,
        request: SessionRunRequest<S>,
        request_id: ExternalRequestId,
        response: Value,
    ) -> Result<(), Status>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let session = request
            .sessions
            .load(&request.session_id)
            .await
            .map_err(agent_error_to_status)?;
        let runtime = request
            .runtime_factory
            .build(&session.record.summary(), &request.config)
            .await?;
        let active = self.insert_active(&request.session_id).await?;
        let sessions = request.sessions;
        let session_id = request.session_id;
        let manager = self.clone();
        tokio::spawn(async move {
            let input = ResumeInput::ExternalResponse {
                request_id,
                response: ExternalResponse::new(response),
            };
            let mut stream = sessions
                .resume(&runtime, session_id.clone(), input)
                .with_cancellation_token(active.token.clone())
                .stream();
            while let Some(event) = stream.next().await {
                if let Err(error) = event {
                    tracing::warn!(session_id = %session_id.as_str(), %error, "session resume failed");
                    break;
                }
            }
            manager.remove_active(&session_id, active.id).await;
        });
        Ok(())
    }

    pub(super) async fn interrupt<S>(
        &self,
        sessions: &AgentSessions<S>,
        session_id: &SessionId,
    ) -> Result<(), Status>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        let active = self.active.lock().await.remove(session_id.as_str());
        if let Some(active) = active {
            active.token.cancel();
        }
        sessions
            .interrupt(session_id)
            .await
            .map_err(agent_error_to_status)?;
        Ok(())
    }

    async fn insert_active(&self, session_id: &SessionId) -> Result<ActiveRun, Status> {
        let mut active = self.active.lock().await;
        if active.contains_key(session_id.as_str()) {
            return Err(Status::failed_precondition(
                "session already has an active run",
            ));
        }
        let run = self.new_active_run();
        active.insert(session_id.as_str().to_owned(), run.clone());
        drop(active);
        Ok(run)
    }

    async fn replace_active(&self, session_id: &SessionId) -> ActiveRun {
        let run = self.new_active_run();
        let mut active = self.active.lock().await;
        if let Some(previous) = active.insert(session_id.as_str().to_owned(), run.clone()) {
            previous.token.cancel();
        }
        drop(active);
        run
    }

    fn new_active_run(&self) -> ActiveRun {
        ActiveRun {
            id: self.next_run_id.fetch_add(1, Ordering::Relaxed),
            token: CancellationToken::new(),
        }
    }

    async fn remove_active(&self, session_id: &SessionId, run_id: u64) {
        let mut active = self.active.lock().await;
        if active
            .get(session_id.as_str())
            .is_some_and(|run| run.id == run_id)
        {
            active.remove(session_id.as_str());
        }
    }
}
