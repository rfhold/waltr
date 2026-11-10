//! One live owner for native and browser interactions; control never waits for data credit.
use super::{
    api::{self, ResponseStream},
    cursor,
    transport::{OutputFrame, PendingTransport},
    wire,
};
use crate::{
    access_auth::AuthenticatedPrincipal,
    routine::{
        blob::BlobStore,
        definition::Definition,
        media::stream::Pool,
        observation::ObservationStore,
        ports::PortError,
        service::{ChildAdmission, InvocationHandle, Outcome},
    },
};
use futures_util::stream;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};
use tonic::Status;
use waltr::grpc::waltr::routine::v2 as p;

struct PrincipalPool {
    gate: Arc<Semaphore>,
    media: Pool,
    transport: Arc<Semaphore>,
}
struct Lease {
    renewed: Instant,
    attached: bool,
    watch_claimed: bool,
}
#[cfg(test)]
mod lease_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn routine_renew_rechecks_expiry_and_cancel_after_credential_lookup() {
        let born = Instant::now();
        let absolute = born + Duration::from_secs(90);
        for mode in 0..3 {
            let mut lease = Lease {
                renewed: born,
                attached: true,
                watch_claimed: false,
            };
            // Credential lookup succeeded before the critical section resumed.
            assert!(born < lease.expires(born, absolute));
            let now = match mode {
                0 => born + Duration::from_secs(30),
                1 => absolute,
                _ => born + Duration::from_secs(1),
            };
            assert!(lease.renew(now, born, absolute, mode == 2).is_err());
            assert_eq!(lease.renewed, born);
            assert_eq!(
                lease.expires(born, absolute),
                born + Duration::from_secs(30)
            );
        }
        let mut lease = Lease {
            renewed: born,
            attached: true,
            watch_claimed: false,
        };
        lease
            .renew(born + Duration::from_secs(29), born, absolute, false)
            .unwrap();
        assert_eq!(lease.renewed, born + Duration::from_secs(29));
        assert_eq!(
            lease.expires(born, absolute),
            born + Duration::from_secs(59)
        );
    }
    #[tokio::test(start_paused = true)]
    async fn routine_finite_clock_attach_renewal_never_extends_absolute() {
        let born = Instant::now();
        let absolute = born + Duration::from_secs(5400);
        let mut lease = Lease {
            renewed: born,
            attached: false,
            watch_claimed: false,
        };
        assert_eq!(lease.expires(born, absolute), born + Duration::from_secs(5));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(Instant::now() < lease.expires(born, absolute));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(Instant::now() >= lease.expires(born, absolute));
        lease.attached = true;
        lease.renewed = Instant::now();
        assert_eq!(
            lease.expires(born, absolute),
            born + Duration::from_secs(35)
        );
        tokio::time::advance(Duration::from_secs(29)).await;
        lease.renewed = Instant::now();
        assert_eq!(
            lease.expires(born, absolute),
            born + Duration::from_secs(64)
        );
        tokio::time::advance(Duration::from_secs(5365)).await;
        lease.renewed = Instant::now();
        assert_eq!(lease.expires(born, absolute), absolute);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(Instant::now() >= lease.expires(born, absolute));
    }
}
impl Lease {
    fn renew(
        &mut self,
        now: Instant,
        born: Instant,
        absolute: Instant,
        cancelled: bool,
    ) -> Result<(), Status> {
        if cancelled || now >= self.expires(born, absolute) {
            return Err(Status::failed_precondition("attachment expired"));
        }
        self.renewed = now;
        Ok(())
    }
    fn expires(&self, born: Instant, absolute: Instant) -> Instant {
        let deadline = (self.renewed + Duration::from_secs(30)).min(absolute);
        if self.attached {
            deadline
        } else {
            deadline.min(born + Duration::from_secs(5))
        }
    }
}
pub(super) struct Attachment {
    pub pending: Arc<PendingTransport>,
    pub owner: Arc<InvocationHandle>,
    id: String,
    secret: [u8; 32],
    generation: String,
    born: Instant,
    absolute: Instant,
    lease: Mutex<Lease>,
    renewed: tokio::sync::Notify,
    terminal: watch::Sender<Option<Outcome>>,
    task: tokio::sync::Mutex<Option<JoinHandle<Result<Outcome, PortError>>>>,
    joined: Mutex<Option<Result<Outcome, PortError>>>,
    _slots: Vec<OwnedSemaphorePermit>,
}
impl Attachment {
    fn credentials(&self) -> p::AttachmentCredentials {
        p::AttachmentCredentials {
            attachment_id: self.id.clone(),
            secret: self.secret.to_vec(),
            invocation_id: self.owner.id.clone(),
            generation: self.generation.clone(),
        }
    }
    fn check(&self, principal: &str, credentials: &p::AttachmentCredentials) -> Result<(), Status> {
        if principal != self.pending.principal
            || credentials.invocation_id != self.owner.id
            || credentials.attachment_id != self.id
            || credentials.generation != self.generation
            || !cursor::equal(&credentials.secret, &self.secret)
        {
            return Err(Status::permission_denied("attachment unavailable"));
        }
        let now = Instant::now();
        let lease = self
            .lease
            .lock()
            .map_err(|_| Status::unavailable("attachment unavailable"))?;
        if now >= lease.expires(self.born, self.absolute) || self.pending.control.is_cancelled() {
            return Err(Status::failed_precondition("attachment expired"));
        }
        Ok(())
    }
    fn presentation(&self) -> Result<(i64, i64, i64), Status> {
        let lease = self
            .lease
            .lock()
            .map_err(|_| Status::unavailable("attachment unavailable"))?;
        Ok((
            wall(
                lease
                    .renewed
                    .checked_add(Duration::from_secs(30))
                    .unwrap_or(self.absolute)
                    .min(self.absolute),
            ),
            wall(self.absolute),
            wall((self.born + Duration::from_secs(5)).min(self.absolute)),
        ))
    }
    async fn join(&self) -> Result<Outcome, PortError> {
        let mut task = self.task.lock().await;
        if let Some(task) = task.as_mut() {
            let outcome = task.await.unwrap_or(Err(PortError::Uncertain));
            *self.joined.lock().map_err(|_| PortError::Uncertain)? = Some(outcome.clone());
        }
        task.take();
        self.joined
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clone()
            .ok_or(PortError::Uncertain)?
    }
}
/// Explicit process-shared registry. Construct once and inject it into the Routine RPC service.
pub struct AttachmentRegistry {
    generation: String,
    gate: Arc<Semaphore>,
    principals: Mutex<BTreeMap<String, Arc<PrincipalPool>>>,
    media: Pool,
    transport: Arc<Semaphore>,
    entries: Mutex<BTreeMap<String, Arc<Attachment>>>,
    history: Arc<dyn ObservationStore>,
    blobs: Arc<dyn BlobStore>,
}
impl std::fmt::Debug for AttachmentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentRegistry").finish_non_exhaustive()
    }
}
impl AttachmentRegistry {
    /// Maximum 64 live/quarantined attachments, four per principal, with shared payload pools.
    pub fn new(history: Arc<dyn ObservationStore>, blobs: Arc<dyn BlobStore>) -> Arc<Self> {
        Arc::new(Self {
            generation: uuid::Uuid::new_v4().to_string(),
            gate: Arc::new(Semaphore::new(64)),
            principals: Mutex::new(BTreeMap::new()),
            media: Pool::process(),
            transport: Arc::new(Semaphore::new(16 * 1024 * 1024)),
            entries: Mutex::new(BTreeMap::new()),
            history,
            blobs,
        })
    }
    pub(super) async fn prepare(
        &self,
        principal: String,
        definition: Arc<Definition>,
        inputs: Vec<p::InputDeclaration>,
    ) -> Result<(Arc<PendingTransport>, Vec<OwnedSemaphorePermit>), Status> {
        self.reap().await;
        let process = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("attachment capacity"))?;
        let (pool, slot) = {
            let mut pools = self
                .principals
                .lock()
                .map_err(|_| Status::unavailable("attachment registry"))?;
            pools.retain(|_, pool| {
                pool.gate.available_permits() != 4
                    || pool.transport.available_permits() != 4 * 1024 * 1024
                    || pool.media.retained() != 0
            });
            if !pools.contains_key(&principal) && pools.len() >= 64 {
                return Err(Status::resource_exhausted("principal capacity"));
            }
            let pool = pools
                .entry(principal.clone())
                .or_insert_with(|| {
                    Arc::new(PrincipalPool {
                        gate: Arc::new(Semaphore::new(4)),
                        media: Pool::principal(),
                        transport: Arc::new(Semaphore::new(4 * 1024 * 1024)),
                    })
                })
                .clone();
            let slot = pool
                .gate
                .clone()
                .try_acquire_owned()
                .map_err(|_| Status::resource_exhausted("principal attachment capacity"))?;
            (pool, slot)
        };
        let pending = PendingTransport::new(
            principal,
            definition,
            inputs,
            pool.media.clone(),
            self.media.clone(),
            pool.transport.clone(),
            self.transport.clone(),
            self.history.clone(),
            self.blobs.clone(),
        )?;
        Ok((pending, vec![process, slot]))
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "original admission owner, access lease and resource reservation remain distinct"
    )]
    pub(super) async fn admit(
        self: &Arc<Self>,
        pending: Arc<PendingTransport>,
        slots: Vec<OwnedSemaphorePermit>,
        admission: ChildAdmission,
        principal: AuthenticatedPrincipal,
        deadline: Duration,
    ) -> Result<(Arc<Attachment>, p::AttachmentAdmission), Status> {
        let born = Instant::now();
        let absolute = (born + deadline.min(Duration::from_secs(5400))).min(pending.deadline());
        let owner = Arc::new(admission.owner);
        let (terminal, _) = watch::channel(None);
        let attachment = Arc::new(Attachment {
            pending,
            owner,
            id: uuid::Uuid::new_v4().to_string(),
            secret: rand::random(),
            generation: self.generation.clone(),
            born,
            absolute,
            lease: Mutex::new(Lease {
                renewed: born,
                attached: false,
                watch_claimed: false,
            }),
            renewed: tokio::sync::Notify::new(),
            terminal,
            task: tokio::sync::Mutex::new(None),
            joined: Mutex::new(None),
            _slots: slots,
        });
        self.entries
            .lock()
            .map_err(|_| Status::unavailable("attachment registry"))?
            .insert(attachment.id.clone(), attachment.clone());
        let (ack, admitted) = oneshot::channel();
        let active = attachment.clone();
        let mut access = super::access_ended(&principal);
        *attachment.task.lock().await = Some(tokio::spawn(async move {
            let admitted_handle = tokio::select! {
                ()=&mut access=>Err(PortError::Denied),()=active.pending.control.cancelled()=>Err(PortError::Unavailable),
                ()=tokio::time::sleep_until((active.born+Duration::from_secs(5)).min(active.absolute))=>Err(PortError::Unavailable),
                result=admission.admitted=>result,
            };
            let admitted_handle = match admitted_handle {
                Ok(handle) => {
                    let _ = ack.send(Ok(()));
                    Some(handle)
                }
                Err(error) => {
                    active.owner.cancel();
                    let _ = ack.send(Err(error));
                    None
                }
            };
            let outcome = loop {
                let notified = active.renewed.notified();
                let expires = {
                    let lease = active.lease.lock().map_err(|_| PortError::Uncertain)?;
                    lease.expires(active.born, active.absolute)
                };
                tokio::select! {
                    outcome=active.owner.join()=>break outcome,
                    ()=&mut access=> { active.owner.cancel();break active.owner.join().await; },
                    ()=active.pending.control.cancelled()=> { active.owner.cancel();break active.owner.join().await; },
                    ()=tokio::time::sleep_until(expires)=> { active.owner.cancel();break active.owner.join().await; },
                    ()=notified=>{},
                    ()=tokio::time::sleep(Duration::from_secs(10)),if active.pending.is_native()=> {
                        let mut lease=active.lease.lock().map_err(|_|PortError::Uncertain)?;
                        if lease.attached { lease.renewed=Instant::now(); }
                    },
                }
            };
            drop(admitted_handle);
            active.terminal.send_replace(Some(outcome.clone()));
            if !active
                .lease
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .attached
            {
                active
                    .pending
                    .output_rx
                    .lock()
                    .map_err(|_| PortError::Uncertain)?
                    .take();
            }
            Ok(outcome)
        }));
        let guard = CancelOnDrop(Some(attachment.pending.control.clone()));
        admitted
            .await
            .map_err(|_| Status::unavailable("admission acknowledgment unavailable"))?
            .map_err(wire::error)?;
        let (lease_expires_ms, absolute_expires_ms, attach_by_ms) = attachment.presentation()?;
        let response = p::AttachmentAdmission {
            invocation: Some(p::InteractionAccepted {
                invocation_id: attachment.owner.id.clone(),
                revision: attachment.pending.definition_revision().into(),
                limits: Some(api::limits()),
            }),
            credentials: Some(attachment.credentials()),
            lease_expires_ms,
            absolute_expires_ms,
            attach_by_ms,
        };
        guard.disarm();
        Ok((attachment, response))
    }
    pub(super) fn get(
        &self,
        principal: &str,
        credentials: Option<p::AttachmentCredentials>,
    ) -> Result<Arc<Attachment>, Status> {
        let credentials = credentials
            .ok_or_else(|| Status::invalid_argument("missing attachment credentials"))?;
        wire::id(&credentials.attachment_id, 256)?;
        wire::id(&credentials.invocation_id, 256)?;
        wire::id(&credentials.generation, 256)?;
        if !(32..=64).contains(&credentials.secret.len()) {
            return Err(Status::permission_denied("attachment unavailable"));
        }
        let entry = self
            .entries
            .lock()
            .map_err(|_| Status::unavailable("attachment registry"))?
            .get(&credentials.attachment_id)
            .cloned()
            .ok_or_else(|| Status::permission_denied("attachment unavailable"))?;
        entry.check(principal, &credentials)?;
        Ok(entry)
    }
    pub(super) fn by_invocation(
        &self,
        principal: &str,
        id: &str,
    ) -> Result<Arc<PendingTransport>, Status> {
        self.entries
            .lock()
            .map_err(|_| Status::unavailable("attachment registry"))?
            .values()
            .find(|entry| entry.pending.principal == principal && entry.owner.id == id)
            .map(|entry| entry.pending.clone())
            .ok_or_else(|| Status::permission_denied("invocation transport unavailable"))
    }
    /// Join all original owners. Uncertain cleanup retains registry and service quarantine.
    pub async fn shutdown(&self) -> Result<(), PortError> {
        let entries: Vec<_> = self
            .entries
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .values()
            .cloned()
            .collect();
        for entry in &entries {
            entry.pending.control.cancel();
        }
        let mut clean = true;
        for entry in entries {
            clean &= entry
                .join()
                .await
                .is_ok_and(|outcome| outcome.cleanup_complete);
        }
        if clean {
            self.entries
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .clear();
            Ok(())
        } else {
            Err(PortError::Uncertain)
        }
    }
    async fn reap(&self) {
        let entries: Vec<_> = match self.entries.lock() {
            Ok(entries) => entries
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect(),
            Err(_) => return,
        };
        for (id, entry) in entries {
            let finished = entry
                .task
                .lock()
                .await
                .as_ref()
                .is_some_and(JoinHandle::is_finished);
            if finished
                && entry.terminal.borrow().is_some()
                && entry.pending.output_rx.lock().is_ok_and(|rx| rx.is_none())
                && entry
                    .join()
                    .await
                    .is_ok_and(|outcome| outcome.cleanup_complete)
                && let Ok(mut entries) = self.entries.lock()
            {
                entries.remove(&id);
            }
        }
    }
    pub(super) async fn send(
        &self,
        principal: &str,
        request: p::SendInputBatchRequest,
    ) -> Result<p::SendInputBatchResponse, Status> {
        let entry = self.get(principal, request.credentials)?;
        if request
            .events
            .iter()
            .any(|event| matches!(event.event, Some(p::input_event::Event::Cancel(_))))
        {
            if request.events.len() != 1 {
                return Err(Status::invalid_argument("cancel must be independent"));
            }
            entry.pending.control.cancel();
            return Err(Status::cancelled("interaction cancelled"));
        }
        let guard = CancelOnDrop(Some(entry.pending.control.clone()));
        let result = entry.pending.send(request.sequence, request.events).await;
        if result.is_ok() {
            guard.disarm();
        }
        result?;
        Ok(p::SendInputBatchResponse {
            accepted_sequence: request.sequence,
        })
    }
    pub(super) fn renew(
        &self,
        principal: &str,
        request: p::RenewAttachmentRequest,
    ) -> Result<p::RenewAttachmentResponse, Status> {
        let entry = self.get(principal, request.credentials)?;
        {
            let mut lease = entry
                .lease
                .lock()
                .map_err(|_| Status::unavailable("attachment registry"))?;
            lease.renew(
                Instant::now(),
                entry.born,
                entry.absolute,
                entry.pending.control.is_cancelled(),
            )?;
        }
        entry.renewed.notify_waiters();
        let (lease_expires_ms, absolute_expires_ms, _) = entry.presentation()?;
        Ok(p::RenewAttachmentResponse {
            lease_expires_ms,
            absolute_expires_ms,
        })
    }
    pub(super) async fn close(
        &self,
        principal: &str,
        request: p::CloseAttachmentRequest,
    ) -> Result<p::CloseAttachmentResponse, Status> {
        let entry = self.get(principal, request.credentials)?;
        match request.mode {
            1 => {
                entry.pending.finish_input().await?;
                Ok(p::CloseAttachmentResponse {
                    status: 2,
                    cleanup_complete: false,
                })
            }
            2 | 3 => {
                entry.pending.control.cancel();
                let outcome = entry.join().await.map_err(wire::error)?;
                Ok(p::CloseAttachmentResponse {
                    status: status(&outcome),
                    cleanup_complete: outcome.cleanup_complete,
                })
            }
            _ => Err(Status::invalid_argument("unknown close mode")),
        }
    }
    pub(super) fn watch(
        &self,
        principal: &str,
        request: p::WatchLiveOutputRequest,
    ) -> Result<ResponseStream<p::WatchLiveOutputResponse>, Status> {
        let entry = self.get(principal, request.credentials)?;
        let receiver = {
            let mut lease = entry
                .lease
                .lock()
                .map_err(|_| Status::unavailable("attachment registry"))?;
            if lease.watch_claimed {
                return Err(Status::failed_precondition("output already claimed"));
            }
            let receiver = entry
                .pending
                .output_rx
                .lock()
                .map_err(|_| Status::unavailable("output registry"))?
                .take()
                .ok_or_else(|| Status::failed_precondition("output unavailable"))?;
            lease.watch_claimed = true;
            lease.attached = true;
            receiver
        };
        entry.renewed.notify_waiters();
        let accepted = p::OutputEvent {
            event: Some(p::output_event::Event::Accepted(p::InteractionAccepted {
                invocation_id: entry.owner.id.clone(),
                revision: entry.pending.definition_revision().into(),
                limits: Some(api::limits()),
            })),
        };
        let guard = CancelOnDrop(Some(entry.pending.control.clone()));
        let state = OutputState {
            terminal: entry.terminal.subscribe(),
            entry,
            receiver,
            previous: None,
            accepted: Some(accepted),
            done: false,
            guard,
        };
        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            if state.done {
                state.guard.disarm_in_place();
                return None;
            }
            if let Some(mut previous) = state.previous.take()
                && let Some(ack) = previous.consumed.take()
            {
                let _ = ack.send(());
            }
            if let Some(output) = state.accepted.take() {
                return Some((
                    Ok(p::WatchLiveOutputResponse {
                        output: Some(output),
                    }),
                    state,
                ));
            }
            loop {
                if let Ok(frame) = state.receiver.try_recv() {
                    let mut frame = frame;
                    let output = std::mem::take(&mut frame.event);
                    state.previous = Some(frame);
                    return Some((
                        Ok(p::WatchLiveOutputResponse {
                            output: Some(output),
                        }),
                        state,
                    ));
                }
                let outcome = state.terminal.borrow().clone();
                if let Some(outcome) = outcome {
                    let text_results = match state.entry.pending.selected_text().await {
                        Ok(descriptors) => descriptors,
                        Err(error) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                    };
                    state.done = true;
                    state.guard.disarm_in_place();
                    let output = p::OutputEvent {
                        event: Some(p::output_event::Event::Terminal(p::InteractionTerminal {
                            status: status(&outcome),
                            result: outcome
                                .result
                                .as_ref()
                                .and_then(|value| serde_json::to_vec(value).ok())
                                .map(|json| p::StructuredValue { json }),
                            cleanup_complete: outcome.cleanup_complete,
                            text_results,
                        })),
                    };
                    return Some((
                        Ok(p::WatchLiveOutputResponse {
                            output: Some(output),
                        }),
                        state,
                    ));
                }
                tokio::select! { frame=state.receiver.recv()=> {
                    if let Some(mut frame)=frame { let output=std::mem::take(&mut frame.event);state.previous=Some(frame);return Some((Ok(p::WatchLiveOutputResponse { output:Some(output) }),state)); }
                    state.done=true;return Some((Err(Status::unavailable("output ended without terminal")),state));
                },changed=state.terminal.changed()=> { if changed.is_err() { state.done=true;return Some((Err(Status::unavailable("owner unavailable")),state)); } } }
            }
        })))
    }
    pub(super) async fn native(
        &self,
        entry: Arc<Attachment>,
        mut input: tonic::Streaming<p::InteractRequest>,
    ) -> Result<(), Status> {
        use prost::Message as _;
        let active = entry.clone();
        entry.pending.own_task(async move {
            let guard=CancelOnDrop(Some(active.pending.control.clone()));
            loop {
                let frame=tokio::select! { ()=active.pending.control.cancelled()=>return Ok(()),()=active.pending.cancelled()=> { guard.disarm();return Ok(()); },frame=input.message()=>frame.map_err(|_|PortError::Invalid)? };
                let Some(frame)=frame else { active.pending.native_closed().await.map_err(|_|PortError::Invalid)?;guard.disarm();return Ok(()); };
                if frame.encoded_len()>131072 { return Err(PortError::Invalid); }
                let Some(p::interact_request::Frame::Input(event))=frame.frame else { return Err(PortError::Invalid); };
                if matches!(event.event,Some(p::input_event::Event::Cancel(_))) { active.pending.control.cancel();return Ok(()); }
                let sequence=active.pending.next_sequence().await;active.pending.send(sequence,vec![event]).await.map_err(|_|PortError::Invalid)?;
            }
        }).await
    }
}
impl Drop for AttachmentRegistry {
    fn drop(&mut self) {
        if let Ok(entries) = self.entries.lock() {
            for entry in entries.values() {
                entry.pending.control.cancel();
            }
        }
    }
}
struct OutputState {
    entry: Arc<Attachment>,
    receiver: mpsc::Receiver<OutputFrame>,
    terminal: watch::Receiver<Option<Outcome>>,
    previous: Option<OutputFrame>,
    accepted: Option<p::OutputEvent>,
    done: bool,
    guard: CancelOnDrop,
}
pub(super) struct CancelOnDrop(pub Option<tokio_util::sync::CancellationToken>);
impl CancelOnDrop {
    pub fn disarm(mut self) {
        self.0 = None;
    }
    fn disarm_in_place(&mut self) {
        self.0 = None;
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}
fn wall(deadline: Instant) -> i64 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_add(remaining)
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}
fn status(outcome: &Outcome) -> i32 {
    use crate::routine::history::Status;
    match outcome.status {
        Some(Status::Admitted) => 1,
        Some(Status::Running) => 2,
        Some(Status::Succeeded) => 3,
        Some(Status::Failed) => 4,
        Some(Status::Interrupted) => 5,
        Some(Status::Uncertain) | None => 6,
    }
}
