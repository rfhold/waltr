//! Unary browser input/control and server-streamed output, without background tasks.
#![allow(missing_docs)]
use super::*;
use futures_util::future::LocalBoxFuture;
use std::rc::Rc;

pub(crate) enum MediaRequest {
    Open(wire::OpenAttachmentRequest),
    Batch(wire::SendInputBatchRequest),
    Renew(wire::RenewAttachmentRequest),
    Close(wire::CloseAttachmentRequest),
    Begin(wire::BeginBlobUploadRequest),
    Append(wire::AppendBlobUploadRequest),
    Commit(wire::CommitBlobUploadRequest),
    CancelUpload(wire::CancelBlobUploadRequest),
    OpenRead(wire::OpenBlobReadRequest),
    CloseRead(wire::CloseBlobReadRequest),
    Text(wire::ReadTextResultRequest),
}
pub(crate) enum MediaResponse {
    Open(Box<wire::OpenAttachmentResponse>),
    Batch(wire::SendInputBatchResponse),
    Renew(wire::RenewAttachmentResponse),
    Close(wire::CloseAttachmentResponse),
    Begin(wire::BeginBlobUploadResponse),
    Append(wire::AppendBlobUploadResponse),
    Commit(wire::CommitBlobUploadResponse),
    Unit,
    OpenRead(wire::OpenBlobReadResponse),
    Text(wire::ReadTextResultResponse),
}
pub(crate) enum MediaStreamRequest {
    Output(wire::WatchLiveOutputRequest),
    Blob(wire::ReadBlobRequest),
}
pub(crate) enum MediaStreamResponse {
    Output(Box<wire::OutputEvent>),
    Blob(wire::ReadBlobResponse),
}
pub(crate) type MediaStream = ProtocolStream<MediaStreamResponse>;
pub(crate) type MediaRpc =
    Arc<dyn Fn(MediaRequest) -> LocalBoxFuture<'static, Result<MediaResponse, ClientError>>>;
pub(crate) type MediaStreamRpc =
    Arc<dyn Fn(MediaStreamRequest) -> LocalBoxFuture<'static, Result<MediaStream, ClientError>>>;

#[cfg(test)]
mod renewal_tests {
    use super::*;
    use futures_util::FutureExt;
    fn owner() -> (
        AttachmentControl,
        futures_channel::oneshot::Sender<Result<MediaResponse, ClientError>>,
        Arc<InteractionQueue>,
    ) {
        let (tx, rx) = futures_channel::oneshot::channel();
        let receiver = Arc::new(Mutex::new(Some(rx)));
        let rpc: MediaRpc = Arc::new(move |_| {
            let rx = receiver.lock().unwrap().take().unwrap();
            async move { rx.await.unwrap() }.boxed_local()
        });
        let queue = InteractionQueue::new();
        let control = AttachmentControl(Rc::new(Owner {
            credentials: wire::AttachmentCredentials::default(),
            rpc,
            queue: queue.clone(),
            lease: Mutex::new((100, 100000)),
            attach_by_ms: 50,
        }));
        (control, tx, queue)
    }
    #[tokio::test]
    async fn renewal_error_after_terminal_does_not_cancel_the_eof_drain() {
        let (control, tx, queue) = owner();
        let mut renew = Box::pin(control.renew());
        assert!(renew.as_mut().now_or_never().is_none());
        queue.end();
        assert!(tx.send(Err(invalid())).is_ok());
        assert!(renew.await.is_err());
        assert!(!control.is_cancelled());
    }
    #[tokio::test]
    async fn pending_renewal_drop_after_terminal_preserves_the_eof_drain() {
        let (control, _tx, queue) = owner();
        let mut renew = Box::pin(control.renew());
        assert!(renew.as_mut().now_or_never().is_none());
        queue.end();
        drop(renew);
        assert!(!control.is_cancelled());
    }
    #[tokio::test]
    async fn renewal_error_before_terminal_still_cancels_owner() {
        let (control, tx, _queue) = owner();
        let mut renew = Box::pin(control.renew());
        assert!(renew.as_mut().now_or_never().is_none());
        assert!(tx.send(Err(invalid())).is_ok());
        assert!(renew.await.is_err());
        assert!(control.is_cancelled());
    }
}

struct Owner {
    attach_by_ms: i64,
    credentials: wire::AttachmentCredentials,
    rpc: MediaRpc,
    lease: Mutex<(i64, i64)>,
    queue: Arc<InteractionQueue>,
}
impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachmentOwner([redacted])")
    }
}
#[derive(Clone, Debug)]
pub struct AttachmentControl(Rc<Owner>);
impl AttachmentControl {
    /// Public timestamps for caller-owned deadline timers; no secret authority.
    pub fn deadlines(&self) -> (i64, i64, i64) {
        let lease = self.0.lease.lock().unwrap();
        (self.0.attach_by_ms, lease.0, lease.1)
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.queue.state.lock().unwrap().cancelled
    }
    pub async fn renew(&self) -> Result<(), ClientError> {
        if self.0.queue.state.lock().unwrap().cancelled || self.0.queue.state.lock().unwrap().ended
        {
            return Err(invalid());
        }
        let mut pending = PendingMediaWait::new(self.0.queue.clone());
        let response = (self.0.rpc)(MediaRequest::Renew(wire::RenewAttachmentRequest {
            credentials: Some(self.0.credentials.clone()),
        }))
        .await;
        pending.complete();
        let result = match response {
            Ok(MediaResponse::Renew(r)) => {
                let mut lease = self.0.lease.lock().unwrap();
                if r.absolute_expires_ms != lease.1
                    || r.lease_expires_ms <= lease.0
                    || r.lease_expires_ms > lease.1
                {
                    Err(invalid())
                } else {
                    lease.0 = r.lease_expires_ms;
                    Ok(())
                }
            }
            Err(e) => Err(e),
            _ => Err(invalid()),
        };
        if result.is_err() && !self.0.queue.state.lock().unwrap().ended {
            self.0.queue.cancel();
        }
        result
    }
    pub async fn cancel_and_join(&self) -> Result<bool, ClientError> {
        self.close(wire::AttachmentCloseMode::Cancel).await
    }
    pub async fn release_and_join(&self) -> Result<bool, ClientError> {
        self.close(wire::AttachmentCloseMode::ReleaseOwner).await
    }
    async fn close(&self, mode: wire::AttachmentCloseMode) -> Result<bool, ClientError> {
        self.0.queue.cancel();
        match (self.0.rpc)(MediaRequest::Close(wire::CloseAttachmentRequest {
            credentials: Some(self.0.credentials.clone()),
            mode: mode as i32,
        }))
        .await?
        {
            MediaResponse::Close(r) => {
                let s = crate::transport::routine::invocation_status(r.status)?;
                Ok(s.is_terminal() && r.cleanup_complete)
            }
            _ => Err(invalid()),
        }
    }
}
pub struct AttachmentInput {
    owner: Rc<Owner>,
    protocol: InputProtocol,
    limits: InteractionLimits,
    sequence: u64,
    poisoned: bool,
}
impl std::fmt::Debug for AttachmentInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachmentInput([redacted])")
    }
}
impl AttachmentInput {
    pub fn control(&self) -> AttachmentControl {
        AttachmentControl(self.owner.clone())
    }
    pub async fn send(&mut self, events: Vec<wire::InputEvent>) -> Result<(), ClientError> {
        if self.poisoned
            || events.is_empty()
            || events.len() > self.limits.server.input_batch_events as usize
        {
            return Err(invalid());
        }
        {
            let s = self.owner.queue.state.lock().unwrap();
            if !s.accepted || s.cancelled || s.ended {
                return Err(invalid());
            }
        }
        let mut protocol = self.protocol.clone();
        for e in &events {
            protocol.apply(e, &self.limits)?;
        }
        let request = wire::SendInputBatchRequest {
            credentials: Some(self.owner.credentials.clone()),
            sequence: self.sequence,
            events,
        };
        if request.encoded_len() > self.limits.server.input_batch_bytes as usize {
            return Err(invalid());
        }
        // Once dispatch starts, dropping this future leaves the input lane poisoned.
        // Neither a lost acknowledgement nor a cancelled waiter grants retry authority.
        self.poisoned = true;
        let mut pending = PendingMediaWait::new(self.owner.queue.clone());
        let future = (self.owner.rpc)(MediaRequest::Batch(request));
        futures_util::pin_mut!(future);
        let response = futures_util::future::poll_fn(|cx| {
            self.owner.queue.writer.register(cx.waker());
            if self.owner.queue.state.lock().unwrap().cancelled {
                return Poll::Ready(Err(invalid()));
            }
            future.as_mut().poll(cx)
        })
        .await;
        match response {
            Ok(MediaResponse::Batch(r)) if r.accepted_sequence == self.sequence => {
                self.sequence = self.sequence.checked_add(1).ok_or_else(invalid)?;
                self.protocol = protocol;
                if self.protocol.finished {
                    self.owner.queue.state.lock().unwrap().input_finished = true;
                    self.owner.queue.wake_uploads();
                }
                self.poisoned = false;
                pending.complete();
                Ok(())
            }
            Err(e) => {
                self.owner.queue.cancel();
                Err(e)
            }
            _ => {
                self.owner.queue.cancel();
                Err(invalid())
            }
        }
    }
    pub async fn finish_input(&mut self) -> Result<(), ClientError> {
        self.send(vec![wire::InputEvent {
            event: Some(input_event::Event::Finish(wire::InputFinish {})),
        }])
        .await
    }
    pub fn invocation_id(&self) -> &str {
        &self.owner.credentials.invocation_id
    }
    pub fn begin_upload(&self, port: &str) -> Result<super::transfer::BlobUpload, ClientError> {
        if !self.owner.queue.state.lock().unwrap().accepted
            || self.owner.queue.state.lock().unwrap().cancelled
            || self.owner.queue.state.lock().unwrap().ended
            || self.protocol.finished
            || self.protocol.blobs.contains(port)
        {
            return Err(invalid());
        }
        let declaration = self
            .protocol
            .declarations
            .iter()
            .find(|d| d.port == port)
            .ok_or_else(invalid)?;
        let Some(input_declaration::Format::Binary(b)) = &declaration.format else {
            return Err(invalid());
        };
        super::transfer::BlobUpload::new(
            self.owner.rpc.clone(),
            self.owner.credentials.invocation_id.clone(),
            port.into(),
            b.clone(),
            self.owner.queue.clone(),
        )
    }
}
impl Drop for AttachmentInput {
    fn drop(&mut self) {
        if !self.owner.queue.state.lock().unwrap().ended {
            self.owner.queue.cancel();
        }
    }
}
#[derive(Debug)]
pub struct AttachmentSession {
    pub input: AttachmentInput,
    pub output: InteractionOutput,
    pub control: AttachmentControl,
}
impl AttachmentSession {
    pub(crate) async fn open(
        open: wire::InteractionOpen,
        limits: InteractionLimits,
        rpc: MediaRpc,
        stream: MediaStreamRpc,
    ) -> Result<Self, ClientError> {
        Self::open_observed(open, limits, rpc, stream, None).await
    }
    pub(crate) async fn open_observed(
        open: wire::InteractionOpen,
        limits: InteractionLimits,
        rpc: MediaRpc,
        stream: MediaStreamRpc,
        observed: Option<Rc<dyn Fn(AttachmentControl)>>,
    ) -> Result<Self, ClientError> {
        limits.validate_open(&open)?;
        let r = (rpc)(MediaRequest::Open(wire::OpenAttachmentRequest {
            open: Some(open.clone()),
        }))
        .await?;
        let admission = match r {
            MediaResponse::Open(r) => match r.admission {
                Some(wire::open_attachment_response::Admission::Accepted(a)) => a,
                Some(wire::open_attachment_response::Admission::Rejected(r))
                    if matches!(r.code, 1..=6) =>
                {
                    return Err(ClientError::new("interaction rejected before admission"));
                }
                _ => return Err(invalid()),
            },
            _ => return Err(invalid()),
        };
        let accepted = admission.invocation.ok_or_else(invalid)?;
        let credentials = admission.credentials.ok_or_else(invalid)?;
        let server = accepted.limits.ok_or_else(invalid)?;
        validate_limits(&server)?;
        if !crate::RoutineLimits::from(server).is_within(&limits.server.into()) {
            return Err(invalid());
        }
        identity(&accepted.invocation_id, 256)?;
        identity(&credentials.attachment_id, 256)?;
        identity(&credentials.generation, 256)?;
        if accepted.revision != limits.definition.routine_revision
            || accepted.invocation_id != credentials.invocation_id
            || !(32..=64).contains(&credentials.secret.len())
            || admission.attach_by_ms <= 0
            || admission.attach_by_ms > admission.lease_expires_ms
            || admission.lease_expires_ms > admission.absolute_expires_ms
        {
            return Err(invalid());
        }
        let queue = InteractionQueue::new();
        queue.state.lock().unwrap().split = true;
        let owner = Rc::new(Owner {
            attach_by_ms: admission.attach_by_ms,
            credentials: credentials.clone(),
            rpc,
            lease: Mutex::new((admission.lease_expires_ms, admission.absolute_expires_ms)),
            queue: queue.clone(),
        });
        let control = AttachmentControl(owner.clone());
        if let Some(observed) = &observed {
            observed(control.clone());
        }
        let source = match stream(MediaStreamRequest::Output(wire::WatchLiveOutputRequest {
            credentials: Some(credentials),
        }))
        .await
        {
            Ok(s) => s,
            Err(e) => {
                if observed.is_none() {
                    drop(control.cancel_and_join().await);
                }
                return Err(e);
            }
        };
        let expected = accepted.clone();
        let first = Arc::new(Mutex::new(true));
        let mapped = source.map(move |r| match r? {
            MediaStreamResponse::Output(o) => {
                let mut first = first.lock().unwrap();
                if *first {
                    *first = false;
                    if !matches!(&o.event,Some(output_event::Event::Accepted(a)) if a==&expected) {
                        return Err(invalid());
                    }
                }
                Ok(*o)
            }
            _ => Err(invalid()),
        });
        let limits = InteractionLimits::for_definition(limits.definition, server)?;
        Ok(Self {
            input: AttachmentInput {
                owner,
                protocol: InputProtocol::new(&open),
                limits: limits.clone(),
                sequence: 0,
                poisoned: false,
            },
            output: InteractionOutput::new(Box::pin(mapped), queue, limits),
            control,
        })
    }
}
