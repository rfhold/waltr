//! Live semantic consumption and bounded exact-source invocation coalescence.

use super::ports::{PortError, PortFuture};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use waltr_agent::{
    ContextIdentity, SessionId, SessionSemanticEvent, SessionSemanticKind,
    SessionSemanticSubscription, TimelineCursor,
};

/// Explicit live-source loss; none of these outcomes claims durable coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerSourceError {
    /// The bounded live ring dropped this many facts.
    Lagged(u64),
    /// The source has permanently stopped.
    Closed,
    /// The source cannot currently be read.
    Unavailable,
}

/// Borrowed asynchronous live-source read.
pub type TriggerFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<SessionSemanticEvent, TriggerSourceError>>
            + Send
            + 'a,
    >,
>;

/// Consumer-owned live source. No replay/backfill and no provider-text parsing.
pub trait TriggerSource: Send {
    /// Read one committed fact or explicit loss, without implicit recovery.
    fn next(&mut self) -> TriggerFuture<'_>;
}
impl TriggerSource for SessionSemanticSubscription {
    fn next(&mut self) -> TriggerFuture<'_> {
        Box::pin(async move {
            self.recv().await.map_err(|error| match error {
                tokio::sync::broadcast::error::RecvError::Lagged(count) => {
                    TriggerSourceError::Lagged(count)
                }
                tokio::sync::broadcast::error::RecvError::Closed => TriggerSourceError::Closed,
            })
        })
    }
}

/// Routine selection requested from trusted durable semantic progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineTrigger {
    /// Source session to capture through the coordinator.
    pub session_id: SessionId,
    /// Task completion belongs to a child: its cursor must never stand in for the parent's.
    pub requested_boundary: Option<TimelineCursor>,
}
#[derive(Debug, Default)]
struct Counts {
    incarnation: String,
    turns: u8,
    tasks: u8,
    seen: VecDeque<String>,
    live: bool,
    last_idle: Option<TimelineCursor>,
    turn_revision: Option<waltr_agent::Revision>,
}

/// At most 32 watched sessions and 64 recent fact identities per session.
/// Hosts explicitly forget stopped sessions; admission fails instead of evicting active state.
#[derive(Debug, Default)]
pub struct SemanticTriggers {
    sessions: BTreeMap<SessionId, Counts>,
}
impl SemanticTriggers {
    pub(crate) fn contains(&self, id: &SessionId) -> bool {
        self.sessions.contains_key(id)
    }
    pub(crate) fn watches(&self, id: &SessionId, incarnation: &str) -> bool {
        self.sessions
            .get(id)
            .is_some_and(|counts| counts.incarnation == incarnation)
    }
    /// Explicitly reserve one bounded session counter.
    pub fn watch(&mut self, id: SessionId, incarnation: String) -> Result<(), PortError> {
        if id.as_str().is_empty()
            || id.as_str().len() > 256
            || incarnation.is_empty()
            || incarnation.len() > 128
        {
            return Err(PortError::Invalid);
        }
        if !self.sessions.contains_key(&id) && self.sessions.len() >= 32 {
            return Err(PortError::Unavailable);
        }
        let counts = self.sessions.entry(id).or_default();
        if counts.incarnation != incarnation {
            *counts = Counts {
                incarnation,
                ..Counts::default()
            };
        }
        Ok(())
    }
    /// Remove all local counters when the host stops watching a session.
    pub fn forget(&mut self, id: &SessionId) {
        self.sessions.remove(id);
    }
    /// Lag resets counters; it does not synthesize missing completions.
    pub fn lost(&mut self) {
        for count in self.sessions.values_mut() {
            count.turns = 0;
            count.tasks = 0;
            count.seen.clear();
            count.live = false;
        }
    }
    /// Count committed turns or completed tasks, never previews or generic tool results.
    pub fn consume(&mut self, event: SessionSemanticEvent) -> Option<RoutineTrigger> {
        if !matches!(event.kind, SessionSemanticKind::TurnCompleted { .. }) { return None; }
        let target = event.session_id.clone();
        let incarnation = event.incarnation.clone()?;
        self.progress(event, &target, &incarnation, false)
    }
    pub(crate) fn task_completed(&mut self, event: SessionSemanticEvent, origin: &crate::agent_host::session_roles::TaskOrigin) -> Option<RoutineTrigger> {
        if !matches!(event.kind, SessionSemanticKind::RunFinished) { return None; }
        self.progress(event, &origin.parent, &origin.incarnation, true)
    }
    fn progress(&mut self, event: SessionSemanticEvent, target: &SessionId, incarnation: &str, task: bool) -> Option<RoutineTrigger> {
        if event.id.len() > 512 {
            return None;
        }
        let counts = self.sessions.get_mut(target)?;
        if incarnation != counts.incarnation {
            return None;
        }
        if !task {
            if counts.turn_revision.is_some_and(|revision| event.revision <= revision) { return None; }
            counts.turn_revision = Some(event.revision);
        }
        if counts.seen.contains(&event.id) {
            return None;
        }
        if counts.seen.len() == 64 {
            counts.seen.pop_front();
        }
        counts.seen.push_back(event.id);
        counts.live = true;
        if task {
            counts.tasks = counts.tasks.saturating_add(1);
        } else {
            counts.turns = counts.turns.saturating_add(1);
        }
        if counts.tasks < 2 && counts.turns < 20 {
            return None;
        }
        counts.tasks = 0;
        counts.turns = 0;
        Some(RoutineTrigger {
            session_id: target.clone(),
            requested_boundary: if task { None } else { event.cursor },
        })
    }
    /// Idle is a live host fact. Never replay an initial list snapshot or repeat a boundary.
    pub fn idle(
        &mut self,
        id: &SessionId,
        boundary: TimelineCursor,
    ) -> Option<RoutineTrigger> {
        let counts = self.sessions.get_mut(id)?;
        if !counts.live || counts.last_idle == Some(boundary) { return None; }
        counts.last_idle = Some(boundary);
        Some(RoutineTrigger {
            session_id: id.clone(),
            requested_boundary: Some(boundary),
        })
    }
}

type Waiter = tokio::sync::oneshot::Sender<Result<String, PortError>>;
#[derive(Debug)]
struct Active {
    context: ContextIdentity,
    invocation: InvocationIdentity,
    deadline: tokio::time::Instant,
    waiters: Vec<Waiter>,
}
#[derive(Debug, Default)]
struct Coalescence {
    active: BTreeMap<SessionId, Active>,
    waiting: usize,
}

/// At most 32 leaders and 32 total waiters, all owned by callers rather than watcher tasks.
#[derive(Clone, Debug, Default)]
pub struct InvocationCoalescer {
    state: Arc<Mutex<Coalescence>>,
}
/// One actual invocation owner, or an exact-source bounded waiter.
pub enum InvocationAdmission {
    /// Caller owns admission, joined execution, and the generic commit receipt.
    Leader(Box<InvocationLeader>),
    /// This future never starts a second invocation or claims newer coverage.
    Waiter(PortFuture<String>),
}
impl std::fmt::Debug for InvocationAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvocationAdmission").finish_non_exhaustive()
    }
}
/// Dropping an unfinished leader fails its waiters without detached cleanup work.
#[derive(Debug)]
pub struct InvocationLeader {
    state: Arc<Mutex<Coalescence>>,
    id: SessionId,
    context: ContextIdentity,
    invocation: InvocationIdentity,
    deadline: tokio::time::Instant,
    completed: bool,
    execution_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationIdentity {
    pub routine: String,
    pub revision: String,
    pub entrypoint: String,
}
impl InvocationCoalescer {
    /// Reserve a leader or waiter with a deadline no longer than thirty seconds.
    pub fn admit(
        &self,
        id: SessionId,
        context: ContextIdentity,
        invocation: InvocationIdentity,
        deadline: tokio::time::Instant,
    ) -> Result<InvocationAdmission, PortError> {
        let now = tokio::time::Instant::now();
        if id.as_str().is_empty()
            || id.as_str().len() > 256
            || [&invocation.routine, &invocation.revision, &invocation.entrypoint].iter().any(|value| value.is_empty() || value.len() > 256)
            || deadline <= now
            || deadline.duration_since(now) > std::time::Duration::from_secs(30)
        {
            return Err(PortError::Invalid);
        }
        let mut state = self.state.lock().map_err(|_| PortError::Unavailable)?;
        if let Some(active) = state.active.get(&id) {
            // Different work is not implicit success or silent recapture.
            if active.context != context || active.invocation != invocation {
                return Err(PortError::Conflict);
            }
            let deadline = deadline.min(active.deadline);
            if state.waiting >= 32 {
                return Err(PortError::Unavailable);
            }
            let (send, receive) = tokio::sync::oneshot::channel();
            state
                .active
                .get_mut(&id)
                .ok_or(PortError::Unavailable)?
                .waiters
                .push(send);
            state.waiting += 1;
            return Ok(InvocationAdmission::Waiter(Box::pin(async move {
                tokio::time::timeout_at(deadline, receive)
                    .await
                    .map_err(|_| PortError::Denied)?
                    .map_err(|_| PortError::Unavailable)?
            })));
        }
        if state.active.len() >= 32 {
            return Err(PortError::Unavailable);
        }
        state.active.insert(
            id.clone(),
            Active {
                context: context.clone(),
                invocation: invocation.clone(),
                deadline,
                waiters: Vec::new(),
            },
        );
        Ok(InvocationAdmission::Leader(Box::new(InvocationLeader {
            state: self.state.clone(),
            id,
            context,
            invocation,
            deadline,
            completed: false,
            execution_id: None,
        })))
    }
}
impl InvocationLeader {
    /// Bind the actual admitted owner before observing its joined result.
    pub fn bind_execution(&mut self, id: &str) -> Result<(), PortError> {
        if id.is_empty() || id.len() > 256 || self.execution_id.is_some() { return Err(PortError::Invalid); }
        self.execution_id = Some(id.into());
        Ok(())
    }
    /// Release waiters after successful owned work and a fresh generic commit receipt.
    pub fn complete(mut self, proof: CommittedInvocationReceipt) -> Result<(), PortError> {
        if proof.context != self.context
            || proof.invocation != self.invocation
            || self.execution_id.as_deref() != Some(proof.execution_id.as_str())
            || proof.note.len() > 8192
            || tokio::time::Instant::now() >= self.deadline
        {
            return Err(PortError::Conflict);
        }
        self.finish(Ok(proof.note))?;
        self.completed = true;
        Ok(())
    }
    fn finish(&self, result: Result<String, PortError>) -> Result<(), PortError> {
        let mut state = self.state.lock().map_err(|_| PortError::Unavailable)?;
        if let Some(active) = state.active.remove(&self.id) {
            state.waiting = state.waiting.saturating_sub(active.waiters.len());
            for waiter in active.waiters {
                drop(waiter.send(result.clone()));
            }
        }
        Ok(())
    }
}

/// Successful joined work with its store-verified transaction still current.
pub struct CommittedInvocationReceipt {
    pub(crate) context: ContextIdentity,
    pub(crate) invocation: InvocationIdentity,
    pub(crate) execution_id: String,
    pub(crate) revision: waltr_agent::Revision,
    pub(crate) note: String,
}
impl std::fmt::Debug for CommittedInvocationReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommittedInvocationReceipt")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl Drop for InvocationLeader {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.finish(Err(PortError::Unavailable));
        }
    }
}
