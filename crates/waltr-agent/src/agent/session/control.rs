//! Session lifecycle, mutation serialization, and run publication context.

#![allow(clippy::redundant_pub_crate)]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex},
};

use tokio::sync::{Mutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::{
    SessionId, SessionIncarnation, SessionListBroadcaster, SessionListSubscription,
    SessionPublicationClaim, SessionSummary, SessionTimelineBroadcaster,
};

type SessionMutationGates = Arc<StdMutex<BTreeMap<SessionId, Arc<Mutex<()>>>>>;

#[derive(Debug, Clone)]
pub(crate) struct SessionControl {
    lifecycle: Arc<RwLock<()>>,
    mutation_gates: SessionMutationGates,
    list: SessionListBroadcaster,
}

impl SessionControl {
    pub(crate) fn new() -> Self {
        Self {
            lifecycle: Arc::new(RwLock::new(())),
            mutation_gates: Arc::new(StdMutex::new(BTreeMap::new())),
            list: SessionListBroadcaster::new(),
        }
    }

    pub(crate) fn session(&self, id: SessionId) -> SessionHandle {
        SessionHandle {
            control: self.clone(),
            id,
        }
    }

    pub(crate) fn subscribe(&self) -> SessionListSubscription {
        self.list.subscribe()
    }

    pub(crate) fn publish_coordination(
        &self,
        id: &SessionId,
        coordination: super::SessionCoordinationState,
    ) {
        self.list.publish_coordination(id, coordination);
    }

    pub(crate) fn register_listed_summary(&self, summary: SessionSummary) -> SessionIncarnation {
        self.list.register_listed_summary(summary)
    }

    pub(crate) fn reconcile_listed_summary(
        &self,
        incarnation: SessionIncarnation,
        summary: SessionSummary,
    ) -> Option<SessionSummary> {
        self.list.reconcile_listed_summary(incarnation, summary)
    }

    pub(crate) async fn exclusive(&self) -> SessionLifecycleGuard {
        SessionLifecycleGuard {
            control: self.clone(),
            _lifecycle: Arc::clone(&self.lifecycle).write_owned().await,
        }
    }

    fn register_mutation_gate(&self, id: &SessionId) -> MutationGateLease {
        let gate = self
            .mutation_gates
            .lock()
            .expect("session mutation gate map lock poisoned")
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        MutationGateLease {
            registry: Arc::clone(&self.mutation_gates),
            id: id.clone(),
            gate: Some(gate),
        }
    }

    fn remove_unused_gates(&self) {
        self.mutation_gates
            .lock()
            .expect("session mutation gate map lock poisoned")
            .retain(|_, gate| Arc::strong_count(gate) > 1);
    }

    #[cfg(test)]
    pub(crate) fn mutation_gate_count(&self) -> usize {
        self.mutation_gates
            .lock()
            .expect("session mutation gate map lock poisoned")
            .len()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_claim(&self, id: &SessionId) -> SessionPublicationClaim {
        self.list.claim(id)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_publish_run_summary(
        &self,
        incarnation: SessionIncarnation,
        summary: SessionSummary,
    ) {
        self.list.publish_run_summary(incarnation, summary);
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn lifecycle_count(&self) -> usize {
        self.list.lifecycle_count()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    control: SessionControl,
    id: SessionId,
}

impl SessionHandle {
    pub(crate) fn try_operation(&self) -> Option<SessionOperation> {
        let lifecycle = Arc::clone(&self.control.lifecycle).try_read_owned().ok()?;
        let lease = self.control.register_mutation_gate(&self.id);
        let mutation = Arc::clone(lease.gate()).try_lock_owned().ok()?;
        let claim = self.control.list.claim(&self.id);
        Some(SessionOperation {
            session: self.clone(),
            lifecycle: Some(lifecycle),
            mutation: Some(mutation),
            claim: Some(claim),
            lease: Some(lease),
        })
    }

    pub(crate) async fn operation(&self) -> SessionOperation {
        let lifecycle = Arc::clone(&self.control.lifecycle).read_owned().await;
        let lease = self.control.register_mutation_gate(&self.id);
        let mutation = Arc::clone(lease.gate()).lock_owned().await;
        let claim = self.control.list.claim(&self.id);
        SessionOperation {
            session: self.clone(),
            lifecycle: Some(lifecycle),
            mutation: Some(mutation),
            claim: Some(claim),
            lease: Some(lease),
        }
    }

    pub(crate) async fn deletion(&self) -> SessionOperation {
        let lifecycle = Arc::clone(&self.control.lifecycle).read_owned().await;
        let lease = self.control.register_mutation_gate(&self.id);
        let mutation = Arc::clone(lease.gate()).lock_owned().await;
        SessionOperation {
            session: self.clone(),
            lifecycle: Some(lifecycle),
            mutation: Some(mutation),
            claim: None,
            lease: Some(lease),
        }
    }

    pub(crate) async fn operation_for_incarnation(
        &self,
        expected: SessionIncarnation,
    ) -> Option<SessionOperation> {
        let lifecycle = Arc::clone(&self.control.lifecycle).read_owned().await;
        let lease = self.control.register_mutation_gate(&self.id);
        let mutation = Arc::clone(lease.gate()).lock_owned().await;
        let claim = self.control.list.claim_incarnation(&self.id, expected)?;
        Some(SessionOperation {
            session: self.clone(),
            lifecycle: Some(lifecycle),
            mutation: Some(mutation),
            claim: Some(claim),
            lease: Some(lease),
        })
    }

    pub(crate) const fn id(&self) -> &SessionId {
        &self.id
    }
}

pub(crate) struct SessionOperation {
    session: SessionHandle,
    lifecycle: Option<OwnedRwLockReadGuard<()>>,
    mutation: Option<OwnedMutexGuard<()>>,
    claim: Option<SessionPublicationClaim>,
    lease: Option<MutationGateLease>,
}

impl SessionOperation {
    pub(crate) const fn incarnation(&self) -> SessionIncarnation {
        self.claim
            .as_ref()
            .expect("publication claim exists")
            .incarnation()
    }

    pub(crate) fn register_loaded_summary(&self, summary: SessionSummary) {
        self.session
            .control
            .list
            .register_loaded_summary(self.incarnation(), summary);
    }

    pub(crate) fn publish_created(&self, summary: SessionSummary) {
        self.session
            .control
            .list
            .publish_created(self.incarnation(), summary);
    }

    pub(crate) fn publish_host_summary(&self, summary: SessionSummary) {
        self.session
            .control
            .list
            .publish_host_summary(self.incarnation(), summary);
    }

    pub(crate) fn publish_run_summary(&self, summary: SessionSummary) {
        self.session
            .control
            .list
            .publish_run_summary(self.incarnation(), summary);
    }

    pub(crate) fn publish_deleted(&self) {
        self.session
            .control
            .list
            .publish_deleted(self.session.id.clone());
    }
}

impl Drop for SessionOperation {
    fn drop(&mut self) {
        self.claim.take();
        self.mutation.take();
        self.lifecycle.take();
        self.lease.take();
    }
}

#[derive(Debug)]
struct MutationGateLease {
    registry: SessionMutationGates,
    id: SessionId,
    gate: Option<Arc<Mutex<()>>>,
}

impl MutationGateLease {
    const fn gate(&self) -> &Arc<Mutex<()>> {
        self.gate.as_ref().expect("mutation gate lease is active")
    }
}

impl Drop for MutationGateLease {
    fn drop(&mut self) {
        let gate = self.gate.take().expect("mutation gate lease is active");
        let mut gates = self
            .registry
            .lock()
            .expect("session mutation gate map lock poisoned");
        if gates
            .get(&self.id)
            .is_some_and(|current| Arc::ptr_eq(current, &gate) && Arc::strong_count(current) == 2)
        {
            gates.remove(&self.id);
        }
    }
}

#[derive(Debug)]
pub(crate) struct SessionLifecycleGuard {
    control: SessionControl,
    _lifecycle: OwnedRwLockWriteGuard<()>,
}

impl SessionLifecycleGuard {
    pub(crate) fn publish_cleared(&self) {
        self.control.list.publish_cleared();
        self.control.remove_unused_gates();
    }
}

#[derive(Clone)]
pub(crate) struct SessionRunContext {
    session: SessionHandle,
    timeline: SessionTimelineBroadcaster,
    event_sink: Option<Arc<dyn crate::SessionEventSink>>,
}

impl std::fmt::Debug for SessionRunContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRunContext")
            .field("session_id", self.session.id())
            .field("event_sink", &self.event_sink.is_some())
            .finish_non_exhaustive()
    }
}

impl SessionRunContext {
    pub(crate) fn new(
        session: SessionHandle,
        timeline: SessionTimelineBroadcaster,
        event_sink: Option<Arc<dyn crate::SessionEventSink>>,
    ) -> Self {
        Self {
            session,
            timeline,
            event_sink,
        }
    }

    pub(crate) async fn operation(&self) -> SessionOperation {
        self.session.operation().await
    }

    pub(crate) fn register_loaded_summary(
        &self,
        incarnation: SessionIncarnation,
        summary: SessionSummary,
    ) {
        self.session
            .control
            .list
            .register_loaded_summary(incarnation, summary);
    }

    pub(crate) fn publish_run_summary(
        &self,
        incarnation: SessionIncarnation,
        summary: SessionSummary,
    ) {
        self.session
            .control
            .list
            .publish_run_summary(incarnation, summary);
    }

    pub(crate) const fn timeline(&self) -> &SessionTimelineBroadcaster {
        &self.timeline
    }

    pub(crate) fn event_sink(&self) -> Option<&Arc<dyn crate::SessionEventSink>> {
        self.event_sink.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::sync::{Notify, oneshot};

    use super::SessionControl;
    use crate::SessionId;

    #[tokio::test]
    async fn normal_operation_drop_removes_its_gate_registration() {
        let control = SessionControl::new();
        let operation = control.session(SessionId::new("normal")).operation().await;
        assert_eq!(control.mutation_gate_count(), 1);
        drop(operation);
        assert_eq!(control.mutation_gate_count(), 0);
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn cancelled_waiter_keeps_holder_gate_until_last_lease_drops() {
        let control = SessionControl::new();
        let session = control.session(SessionId::new("cancelled-waiter"));
        let holder = session.operation().await;
        let waiting_session = session.clone();
        let waiter = tokio::spawn(async move { waiting_session.operation().await });
        tokio::task::yield_now().await;
        assert_eq!(control.mutation_gate_count(), 1);
        assert!(!waiter.is_finished());

        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        assert_eq!(control.mutation_gate_count(), 1);
        drop(holder);
        assert_eq!(control.mutation_gate_count(), 0);
    }

    #[tokio::test]
    async fn overlapping_holder_and_waiter_share_registration_until_both_drop() {
        let control = SessionControl::new();
        let session = control.session(SessionId::new("overlap"));
        let holder = session.operation().await;
        let waiting_session = session.clone();
        let acquired = Arc::new(Notify::new());
        let waiter_acquired = Arc::clone(&acquired);
        let (release_tx, release_rx) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let operation = waiting_session.operation().await;
            waiter_acquired.notify_one();
            drop(release_rx.await);
            operation
        });
        tokio::task::yield_now().await;
        assert_eq!(control.mutation_gate_count(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), acquired.notified())
                .await
                .is_err()
        );

        drop(holder);
        tokio::time::timeout(Duration::from_secs(1), acquired.notified())
            .await
            .expect("waiter should acquire the shared gate");
        assert_eq!(control.mutation_gate_count(), 1);
        let _ = release_tx.send(());
        drop(waiter.await.unwrap());
        assert_eq!(control.mutation_gate_count(), 0);
    }
}
