use super::*;
use crate::routine::{
    triggers::{InvocationAdmission, InvocationIdentity, SemanticTriggers},
};
use futures_util::StreamExt as _;
use waltr_agent::{
    SessionCoordinationPhase, SessionListEvent, SessionSemanticEvent, SessionSemanticHub,
};

fn original_ceiling(capabilities: &BTreeSet<String>, policy: &PermissionPolicy) -> bool {
    tools::original_source_allowed(capabilities, policy, false)
}

pub(super) struct Semantic {
    hub: SessionSemanticHub,
    counts: Arc<Mutex<SemanticTriggers>>,
    stop: CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<(), PortError>>>>,
}
impl Semantic {
    pub fn new<S: SessionStore + Clone + Send + Sync + 'static>(
        owner: std::sync::Weak<Composition<S>>,
        principal: AuthenticatedPrincipal,
        sessions: SessionCoordinator<S>,
        record: &waltr_agent::SessionRecord,
    ) -> Result<Self, PortError> {
        let hub = SessionSemanticHub::new(64).map_err(|_| PortError::Invalid)?;
        let mut source = hub.subscribe().map_err(|_| PortError::Busy)?;
        let counts = Arc::new(Mutex::new(SemanticTriggers::default()));
        counts.lock().map_err(|_| PortError::Uncertain)?.watch(
            record.id.clone(),
            record
                .metadata
                .incarnation
                .clone()
                .ok_or(PortError::Denied)?,
        )?;
        let state = counts.clone();
        let stop = CancellationToken::new();
        let cancel = stop.clone();
        let mut updates = sessions.watch_sessions();
        let coalescer = owner
            .upgrade()
            .ok_or(PortError::Unavailable)?
            .invocation_coalescer
            .clone();
        let task = tokio::spawn(async move {
            let result = async {
            loop {
                let target = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(()),
                    () = principal.access_ended() => return Ok(()),
                    event = source.recv() => match event {
                        Ok(event) => {
                             let Some(owner) = owner.upgrade() else { return Ok(()) };
                             let role = owner.factory.session_roles.get(&event.session_id, event.incarnation.as_deref());
                             let mut counts = state.lock().map_err(|_| PortError::Uncertain)?;
                             match role {
                                 Some(crate::agent_host::session_roles::SessionRole::Task(origin)) => counts.task_completed(event, &origin)
                                     .map(|trigger| (trigger.session_id, trigger.requested_boundary, "session.delegated-task-batch")),
                                 Some(_) => None,
                                 None if matches!(event.kind, waltr_agent::SessionSemanticKind::RunFinished) => counts.contains(&event.session_id)
                                     .then_some((event.session_id, None, "session.idle-flush")),
                                 None => counts.consume(event).map(|trigger| (trigger.session_id, trigger.requested_boundary, "session.turn-batch")),
                             }
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { state.lock().map_err(|_| PortError::Uncertain)?.lost(); None },
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                    },
                    update = updates.next() => match update.map(|update| update.event) {
                        Some(SessionListEvent::Upsert(summary)) if summary.coordination.phase == SessionCoordinationPhase::Ready => {
                            if state.lock().map_err(|_| PortError::Uncertain)?.contains(&summary.id) { Some((summary.id, None, "session.idle-flush")) } else { None }
                        },
                        Some(SessionListEvent::Deleted(id)) => { state.lock().map_err(|_| PortError::Uncertain)?.forget(&id); None },
                        Some(SessionListEvent::Cleared) => { *state.lock().map_err(|_| PortError::Uncertain)? = SemanticTriggers::default(); None },
                        Some(_) => None,
                        None => return Ok(()),
                    },
                };
                let Some((id, boundary, selector)) = target else { continue };
                let Some(owner) = owner.upgrade() else { return Ok(()) };
                let record = match sessions.load(&id).await { Ok(session) => session.record, Err(_) => continue };
                if !state.lock().map_err(|_| PortError::Uncertain)?.watches(&id, record.metadata.incarnation.as_deref().unwrap_or_default()) { continue; }
                if owner.factory.session_roles.excluded(&record) { continue; }
                let catalog = owner.state.snapshot();
                let selected = catalog.scripts().iter().flat_map(|(name, definition)| definition.envelope().spec.entrypoints.iter()
                    .filter(|(_, entry)| matches!(entry.trigger, crate::routine::definition::Trigger::Event) && entry.selector.as_deref() == Some(selector))
                    .map(move |(entrypoint, _)| (name.clone(), entrypoint.clone(), definition.clone()))).collect::<Vec<_>>();
                if selected.is_empty() { continue; }
                let policy = match owner.factory.runtime_for_record(&record, sessions.clone()).await {
                    Ok((_, dispatcher)) => dispatcher.permission_policy(), Err(_) => continue,
                };
                for (name, entrypoint, definition) in selected {
                let original_allowed = original_ceiling(&definition.envelope().spec.capabilities, &policy);
                let context = match CoordinatorContext::pin(sessions.clone(), &id, &principal_id(&principal), original_allowed).await { Ok(context) => Arc::new(context), Err(_) => continue };
                let identity = context.snapshot_identity().ok_or(PortError::Denied)?;
                if !identity.matches(&record) { continue; }
                if boundary.is_some_and(|boundary| boundary != identity.boundary()) { continue; }
                if selector == "session.idle-flush" {
                    if sessions.coordination_state(&id).await.phase != SessionCoordinationPhase::Ready { continue; }
                    if state.lock().map_err(|_| PortError::Uncertain)?.idle(&id, identity.boundary()).is_none() { continue; }
                }
                let invocation = InvocationIdentity { routine: name.clone(), revision: definition.revision().into(), entrypoint: entrypoint.clone() };
                let mut leader = match coalescer.admit(id.clone(), identity.clone(), invocation.clone(), tokio::time::Instant::now() + std::time::Duration::from_secs(30)) {
                    Ok(InvocationAdmission::Leader(leader)) => leader,
                    Ok(InvocationAdmission::Waiter(waiter)) => { tokio::select! { biased; () = cancel.cancelled() => return Ok(()), () = principal.access_ended() => return Ok(()), _result = waiter => {} } continue; },
                    Err(_) => continue,
                };
                let expected_revision = invocation.revision.clone();
                let admission = owner.semantic(principal.clone(), id.clone(), &name, Some(&expected_revision), &entrypoint, crate::routine::history::Origin::Event, Value::Null, Some(context.clone()));
                tokio::pin!(admission);
                let mut ended = false;
                let admitted = tokio::select! {
                    biased;
                    () = cancel.cancelled() => { ended = true; admission.await },
                    () = principal.access_ended() => { ended = true; admission.await },
                    result = &mut admission => result,
                };
                let Ok(handle) = admitted else { continue };
                if ended { handle.cancel(); let _joined = handle.join().await; return Ok(()); }
                if leader.bind_execution(&handle.id).is_err() { handle.cancel(); let _joined = handle.join().await; continue; }
                let joined = handle.join();
                tokio::pin!(joined);
                let result = tokio::select! {
                    biased;
                    () = cancel.cancelled() => { handle.cancel(); joined.await },
                    () = principal.access_ended() => { handle.cancel(); joined.await },
                    result = &mut joined => result,
                };
                if result.status == Some(crate::routine::history::Status::Succeeded) && result.cleanup_complete {
                    if let Ok(receipt) = context.committed_receipt(invocation, &result).await { let _completed = leader.complete(receipt); }
                }
                }
            }
            }.await;
            if result.is_err() {
                if let Some(owner) = owner.upgrade() {
                    let _maintenance = owner.service.set_maintenance(true);
                }
            }
            result
        });
        Ok(Self {
            hub,
            counts,
            stop,
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }
    pub fn watch(&self, record: &waltr_agent::SessionRecord) -> Result<(), PortError> {
        let incarnation = record
            .metadata
            .incarnation
            .as_deref()
            .ok_or(PortError::Denied)?;
        self.counts
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .watch(record.id.clone(), incarnation.into())
    }
    pub fn publish(&self, event: SessionSemanticEvent) {
        let _delivered = self.hub.publish(event);
    }
    pub fn stop(&self) {
        self.stop.cancel();
    }
    pub async fn shutdown(&self) -> Result<(), PortError> {
        self.stop();
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await.map_err(|_| PortError::Uncertain);
            task.take();
            result??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_original_requires_declaration_and_independent_allow() {
        let capabilities = BTreeSet::from(["context.original".into()]);
        for level in [PermissionLevel::Deny, PermissionLevel::Ask] {
            assert!(!original_ceiling(&capabilities, &PermissionPolicy::new(level)));
        }
        let allow = PermissionPolicy::new(PermissionLevel::Allow);
        assert!(original_ceiling(&capabilities, &allow));
        assert!(!original_ceiling(&BTreeSet::new(), &allow));
    }
}
