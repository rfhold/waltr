//! Constructor-injected completion coordination for the production host.

use super::{CoordinatorContext, owned::ContextCalls};
use crate::routine::context::ContextPort;
use crate::routine::{
    ports::{PortError, PortFuture},
    service::InvocationHandle,
};
use futures_util::FutureExt;
use std::{collections::BTreeMap, sync::Arc};
use waltr_agent::{
    SessionCompletionAdmissionFinalizer, SessionCompletionCoordinationFuture,
    SessionCompletionCoordinationHandler, SessionCompletionCoordinationPreparation,
    SessionCompletionFinalizationFuture, SessionCoordinationReason, SessionCoordinator,
    SessionPauseLease, SessionRunCompletionSnapshot, SessionStore,
};

/// Host-supplied admission through the shared invocation service and typed adapters.
pub trait AutomaticCompactionAdmission<S: SessionStore + Clone + Send + Sync + 'static>:
    Send + Sync
{
    /// Settle earlier background owners before pinning the completed boundary.
    fn settle(
        &self,
        snapshot: SessionRunCompletionSnapshot,
    ) -> PortFuture<SessionRunCompletionSnapshot> {
        Box::pin(async move { Ok(snapshot) })
    }
    /// Admit the immutable compaction definition against the already-owned context.
    fn admit(
        &self,
        context: Arc<CoordinatorContext<S>>,
        snapshot: SessionRunCompletionSnapshot,
    ) -> PortFuture<InvocationHandle>;
}

/// Bounded, live-only completion bridge. The host must explicitly shut it down.
pub struct AutomaticCompactionHandler<S: SessionStore + Clone + Send + Sync + 'static> {
    sessions: SessionCoordinator<S>,
    authority: String,
    limits: BTreeMap<(String, String), u64>,
    admission: Arc<dyn AutomaticCompactionAdmission<S>>,
    owners: ContextCalls,
    roles: crate::agent_host::session_roles::SessionRoles,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> Clone for AutomaticCompactionHandler<S> {
    fn clone(&self) -> Self {
        Self {
            sessions: self.sessions.clone(),
            authority: self.authority.clone(),
            limits: self.limits.clone(),
            admission: self.admission.clone(),
            owners: self.owners.clone(),
            roles: self.roles.clone(),
        }
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> std::fmt::Debug
    for AutomaticCompactionHandler<S>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomaticCompactionHandler")
            .finish_non_exhaustive()
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> AutomaticCompactionHandler<S> {
    /// Freeze at most 64 host-owned model limits; unknown limits remain unknown.
    pub fn new(
        sessions: SessionCoordinator<S>,
        authority: String,
        limits: BTreeMap<(String, String), u64>,
        admission: Arc<dyn AutomaticCompactionAdmission<S>>,
    ) -> Result<Self, PortError> {
        if authority.is_empty()
            || authority.len() > 128
            || limits.len() > 64
            || limits.iter().any(|((provider, model), limit)| {
                provider.is_empty()
                    || provider.len() > 256
                    || model.is_empty()
                    || model.len() > 256
                    || *limit == 0
            })
        {
            return Err(PortError::Invalid);
        }
        Ok(Self {
            sessions,
            authority,
            limits,
            admission,
            owners: ContextCalls::new(),
            roles: crate::agent_host::session_roles::SessionRoles::default(),
        })
    }
    pub(crate) fn with_session_roles(
        mut self,
        roles: crate::agent_host::session_roles::SessionRoles,
    ) -> Self {
        self.roles = roles;
        self
    }
    /// Join all registered admissions, invocations, and pause cleanup, even if a waiter drops.
    pub fn shutdown(&self) -> PortFuture<()> {
        self.owners.cleanup(Box::pin(async { Ok(()) }))
    }
}
impl<S: SessionStore + Clone + Send + Sync + 'static> SessionCompletionCoordinationHandler
    for AutomaticCompactionHandler<S>
{
    fn prepare(
        &self,
        snapshot: SessionRunCompletionSnapshot,
    ) -> SessionCompletionCoordinationFuture {
        let handler = self.clone();
        Box::pin(async move {
            let record = handler
                .sessions
                .load(&snapshot.session_id)
                .await
                .map_err(|_| "completion source unavailable".to_owned())?
                .record;
            if snapshot.incarnation.is_none()
                || snapshot.incarnation != record.metadata.incarnation
                || snapshot.revision != record.revision
            {
                return Ok(SessionCompletionCoordinationPreparation::Continue);
            }
            if handler.roles.excluded(&record) {
                return Ok(SessionCompletionCoordinationPreparation::Continue);
            }
            let limit = handler
                .limits
                .get(&(snapshot.provider_key.clone(), snapshot.model_id.clone()))
                .copied();
            Ok(SessionCompletionCoordinationPreparation::Claim {
                reason: SessionCoordinationReason::new("routine.compaction", None, None)
                    .map_err(|_| "invalid reason".to_owned())?,
                finalizer: Box::new(Finalizer {
                    handler,
                    snapshot,
                    limit,
                }),
            })
        })
    }
}
struct Finalizer<S: SessionStore + Clone + Send + Sync + 'static> {
    handler: AutomaticCompactionHandler<S>,
    snapshot: SessionRunCompletionSnapshot,
    limit: Option<u64>,
}
impl<S: SessionStore + Clone + Send + Sync + 'static> SessionCompletionAdmissionFinalizer
    for Finalizer<S>
{
    fn finalize(self: Box<Self>, lease: SessionPauseLease) -> SessionCompletionFinalizationFuture {
        Box::pin(async move {
            let snapshot = self
                .handler
                .admission
                .settle(self.snapshot)
                .await
                .map_err(|_| "completion predecessors did not settle".to_owned())?;
            let context = Arc::new(
                CoordinatorContext::completed_compaction(
                    self.handler.sessions.clone(),
                    lease,
                    &self.handler.authority,
                    &snapshot,
                    self.limit,
                )
                .await
                .map_err(|_| "completion pin failed".to_owned())?,
            );
            let (start, gate) = tokio::sync::oneshot::channel();
            let admission = self.handler.admission.clone();
            let fallback = context.clone();
            let owned = self.handler.owners.run_required(Box::pin(async move {
                let result = std::panic::AssertUnwindSafe(async {
                    if gate.await.is_ok() {
                        match admission.admit(context.clone(), snapshot).await {
                            Ok(handle) => {
                                let outcome = handle.join().await;
                                if outcome.cleanup_complete {
                                    Ok(())
                                } else {
                                    Err(PortError::Uncertain)
                                }
                            }
                            Err(error) => Err(error),
                        }
                    } else {
                        Err(PortError::Denied)
                    }
                })
                .catch_unwind()
                .await
                .unwrap_or(Err(PortError::Uncertain));
                context.cleanup().await?;
                result?;
                Ok(serde_json::Value::Null)
            }));
            let owned = match owned {
                Ok(owned) => owned,
                Err(_) => {
                    fallback
                        .cleanup()
                        .await
                        .map_err(|_| "completion release uncertain".to_owned())?;
                    return Err("completion owner unavailable".to_owned());
                }
            };
            Ok(Box::pin(async move {
                let _ = start.send(());
                drop(owned.await);
            }) as waltr_agent::SessionCompletionWorkerFuture)
        })
    }
}
