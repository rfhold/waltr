//! Process-local mappings for opaque blob authorities; durable descriptors never grant reads.
use super::{
    api::ResponseStream,
    attachments::{AttachmentRegistry, CancelOnDrop},
    cursor,
    transport::{self, PendingTransport},
    wire,
};
use crate::{
    access_auth::AuthenticatedPrincipal,
    routine::{
        blob::{self, BlobStore},
        history::InvocationBinding,
        observation::{ObservationScope, ObservationStore, Publication},
        ports::{PortError, PortFuture},
    },
};
use futures_util::stream;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tonic::Status;
use waltr::grpc::waltr::routine::v2 as p;

struct Upload {
    pending: Arc<PendingTransport>,
    port: String,
    state: tokio::sync::Mutex<Option<UploadState>>,
}
struct UploadState {
    upload: blob::Upload,
    next: u64,
    offset: u64,
}
struct Grant {
    principal: String,
    invocation: String,
    port: String,
    id: String,
    secret: [u8; 32],
    lease: blob::OutputLease,
    expires: Instant,
    reader: Mutex<Option<std::sync::Weak<ReadOwner>>>,
    cancel: CancellationToken,
    _slot: OwnedSemaphorePermit,
    _principal_slot: OwnedSemaphorePermit,
}
struct ReadOwner {
    principal: String,
    invocation: String,
    id: String,
    secret: [u8; 32],
    reader: blob::BlobReader,
    access: blob::ReadAccess,
    expires: Instant,
    attached: AtomicBool,
    claimed: AtomicBool,
    changed: tokio::sync::Notify,
    cancel: CancellationToken,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    joined: Mutex<Option<Result<(), PortError>>>,
    _slot: OwnedSemaphorePermit,
    _principal_slot: OwnedSemaphorePermit,
}
#[derive(Default)]
struct ReaderAdmission {
    closed: bool,
    opens: BTreeMap<String, Arc<ReaderOpen>>,
}
struct ReaderOpen {
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    joined: Mutex<Option<Result<(), PortError>>>,
}
#[cfg(test)]
#[derive(Default)]
struct OpenBarrier {
    enabled: AtomicBool,
    entered: tokio::sync::Notify,
    release: CancellationToken,
}
impl ReaderOpen {
    async fn join(&self) -> Result<(), PortError> {
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await.map_err(|_| PortError::Uncertain);
            *self.joined.lock().map_err(|_| PortError::Uncertain)? = Some(result);
        }
        task.take();
        self.joined
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .unwrap_or(Err(PortError::Uncertain))
    }
}
async fn reap_reader_opens(admission: &Mutex<ReaderAdmission>) -> Result<(), PortError> {
    let opens: Vec<_> = admission
        .lock()
        .map_err(|_| PortError::Uncertain)?
        .opens
        .iter()
        .map(|(id, open)| (id.clone(), open.clone()))
        .collect();
    for (id, open) in opens {
        let finished = match open.task.try_lock() {
            Ok(task) => task.as_ref().is_none_or(JoinHandle::is_finished),
            Err(_) => false,
        };
        if finished {
            open.join().await?;
            admission
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .opens
                .remove(&id);
        }
    }
    Ok(())
}
impl ReadOwner {
    fn revoke(&self) {
        self.cancel.cancel();
        self.reader.close();
    }
    async fn join(&self) -> Result<(), PortError> {
        self.revoke();
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await.map_err(|_| PortError::Uncertain);
            *self.joined.lock().map_err(|_| PortError::Uncertain)? = Some(result);
        }
        task.take();
        self.joined
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .unwrap_or(Err(PortError::Uncertain))
    }
}
/// Shared, bounded mapping registry. Inject the same monotonic clock as the blob store.
pub struct BlobRegistry {
    generation: String,
    store: Arc<dyn BlobStore>,
    clock: Arc<dyn blob::Clock>,
    history: Arc<dyn ObservationStore>,
    attachments: Arc<AttachmentRegistry>,
    uploads: Mutex<BTreeMap<String, Arc<Upload>>>,
    grants: Mutex<BTreeMap<String, Arc<Grant>>>,
    readers: Mutex<BTreeMap<String, Arc<ReadOwner>>>,
    reader_admission: Mutex<ReaderAdmission>,
    #[cfg(test)]
    open_barrier: OpenBarrier,
    #[cfg(test)]
    driver_barrier: Arc<OpenBarrier>,
    grant_slots: Arc<Semaphore>,
    reader_slots: Arc<Semaphore>,
    read_bytes: Arc<Semaphore>,
    grant_principals: Mutex<BTreeMap<String, Arc<Semaphore>>>,
    reader_principals: Mutex<BTreeMap<String, Arc<Semaphore>>>,
}
impl std::fmt::Debug for BlobRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRegistry").finish_non_exhaustive()
    }
}
impl BlobRegistry {
    /// At most 128 output grants and 256 readers, with a shared 16 MiB read-buffer budget.
    pub fn new(
        store: Arc<dyn BlobStore>,
        clock: Arc<dyn blob::Clock>,
        history: Arc<dyn ObservationStore>,
        attachments: Arc<AttachmentRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            generation: uuid::Uuid::new_v4().to_string(),
            store,
            clock,
            history,
            attachments,
            uploads: Mutex::new(BTreeMap::new()),
            grants: Mutex::new(BTreeMap::new()),
            readers: Mutex::new(BTreeMap::new()),
            reader_admission: Mutex::new(ReaderAdmission::default()),
            #[cfg(test)]
            open_barrier: OpenBarrier::default(),
            #[cfg(test)]
            driver_barrier: Arc::new(OpenBarrier::default()),
            grant_slots: Arc::new(Semaphore::new(128)),
            reader_slots: Arc::new(Semaphore::new(256)),
            read_bytes: Arc::new(Semaphore::new(16 * 1024 * 1024)),
            grant_principals: Mutex::new(BTreeMap::new()),
            reader_principals: Mutex::new(BTreeMap::new()),
        })
    }
    pub(super) fn publish(
        self: &Arc<Self>,
        binding: InvocationBinding,
        port: String,
        lease: blob::OutputLease,
    ) -> PortFuture<p::BlobOutput> {
        let registry = self.clone();
        Box::pin(async move {
            registry.reap().await?;
            let slot = registry
                .grant_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| PortError::Unavailable)?;
            let principal_slot =
                principal_slot(&registry.grant_principals, binding.principal(), 32, 128)?;
            let ttl = lease
                .expires_at_ms()
                .saturating_sub(registry.clock.now_ms())
                .min(blob::OUTPUT_LEASE_MS);
            if ttl == 0 {
                return Err(PortError::Denied);
            }
            let grant = Arc::new(Grant {
                principal: binding.principal().into(),
                invocation: binding.id().into(),
                port: port.clone(),
                id: uuid::Uuid::new_v4().to_string(),
                secret: rand::random(),
                lease,
                expires: Instant::now() + Duration::from_millis(ttl),
                reader: Mutex::new(None),
                cancel: CancellationToken::new(),
                _slot: slot,
                _principal_slot: principal_slot,
            });
            let metadata = grant.lease.metadata();
            registry
                .history
                .publish(
                    ObservationScope {
                        principal: grant.principal.clone(),
                        source_session: None,
                    },
                    grant.invocation.clone(),
                    Publication::Blob {
                        blob_id: grant.id.clone(),
                        port: grant.port.clone(),
                        display_name: metadata.display_name.clone(),
                        content_type: metadata.content_type.clone(),
                        bytes: metadata.bytes,
                        sha256: metadata.sha256,
                        image: None,
                        remaining_validity_ms: ttl,
                    },
                )
                .await?;
            let descriptor = descriptor(&grant.id, metadata, Some(ttl));
            let output = p::BlobOutput {
                port,
                blob: Some(descriptor),
                output_grant: Some(p::BinaryOutputCredentials {
                    invocation_id: grant.invocation.clone(),
                    port: grant.port.clone(),
                    blob_id: grant.id.clone(),
                    generation: registry.generation.clone(),
                    secret: grant.secret.to_vec(),
                }),
            };
            registry
                .grants
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .insert(grant.id.clone(), grant);
            Ok(output)
        })
    }
    pub(super) async fn begin(
        &self,
        principal: &str,
        request: p::BeginBlobUploadRequest,
    ) -> Result<p::BeginBlobUploadResponse, Status> {
        self.reap().await.map_err(wire::error)?;
        let pending = self
            .attachments
            .by_invocation(principal, &request.invocation_id)?;
        let metadata = transport::binary_metadata(p::BinaryInputDeclaration {
            display_name: request.display_name,
            content_type: request.content_type,
            declared_bytes: request.declared_bytes,
            expected_sha256: request.expected_sha256,
            purpose: request.purpose.map(|purpose| match purpose {
                p::begin_blob_upload_request::Purpose::File(file) => {
                    p::binary_input_declaration::Purpose::File(file)
                }
                p::begin_blob_upload_request::Purpose::Image(image) => {
                    p::binary_input_declaration::Purpose::Image(image)
                }
            }),
        })?;
        let pinned = pending
            .declarations
            .get(&request.port)
            .ok_or_else(|| Status::invalid_argument("undeclared binary input"))?;
        if !same_metadata(pinned, &metadata) {
            return Err(Status::invalid_argument("binary declaration mismatch"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let upload = Arc::new(Upload {
            pending: pending.clone(),
            port: request.port,
            state: tokio::sync::Mutex::new(None),
        });
        {
            let mut uploads = self
                .uploads
                .lock()
                .map_err(|_| Status::unavailable("upload registry"))?;
            if uploads
                .values()
                .any(|entry| Arc::ptr_eq(&entry.pending, &pending) && entry.port == upload.port)
                || uploads.len() >= 2048
            {
                return Err(Status::failed_precondition("binary port already claimed"));
            }
            uploads.insert(id.clone(), upload.clone());
        }
        let guard = CancelOnDrop(Some(pending.control.clone()));
        let owner = pending
            .owner
            .lock()
            .map_err(|_| Status::unavailable("blob owner unavailable"))?
            .clone()
            .ok_or_else(|| Status::failed_precondition("invocation not admitted"))?;
        let handle = self
            .store
            .begin(
                pending.scope().map_err(wire::error)?,
                owner,
                metadata,
                pending.wait(),
            )
            .await
            .map_err(blob_error)?;
        *upload.state.lock().await = Some(UploadState {
            upload: handle,
            next: 0,
            offset: 0,
        });
        guard.disarm();
        Ok(p::BeginBlobUploadResponse {
            upload_id: id,
            chunk_bytes: 65536,
        })
    }
    fn upload(&self, principal: &str, invocation: &str, id: &str) -> Result<Arc<Upload>, Status> {
        wire::id(id, 256)?;
        let upload = self
            .uploads
            .lock()
            .map_err(|_| Status::unavailable("upload registry"))?
            .get(id)
            .cloned()
            .ok_or_else(|| Status::permission_denied("upload unavailable"))?;
        if upload.pending.principal != principal
            || upload.pending.binding().map_err(wire::error)?.id() != invocation
            || upload.pending.control.is_cancelled()
        {
            return Err(Status::permission_denied("upload unavailable"));
        }
        Ok(upload)
    }
    pub(super) async fn append(
        &self,
        principal: &str,
        request: p::AppendBlobUploadRequest,
    ) -> Result<p::AppendBlobUploadResponse, Status> {
        let upload = self.upload(principal, &request.invocation_id, &request.upload_id)?;
        let guard = CancelOnDrop(Some(upload.pending.control.clone()));
        let _credit = upload.pending.try_credits(request.data.len())?;
        let mut state = upload.state.lock().await;
        let state = state
            .as_mut()
            .ok_or_else(|| Status::failed_precondition("upload closed"))?;
        if request.sequence != state.next
            || request.offset != state.offset
            || request.data.is_empty()
            || request.data.len() > 65536
        {
            return Err(Status::invalid_argument("upload sequence"));
        }
        let bytes = request.data.len() as u64;
        self.store
            .append(
                upload.pending.scope().map_err(wire::error)?,
                state.upload.clone(),
                request.sequence,
                request.data,
                upload.pending.wait(),
            )
            .await
            .map_err(blob_error)?;
        state.next += 1;
        state.offset += bytes;
        let response = p::AppendBlobUploadResponse {
            accepted_sequence: request.sequence,
            next_offset: state.offset,
        };
        guard.disarm();
        Ok(response)
    }
    pub(super) async fn commit(
        &self,
        principal: &str,
        request: p::CommitBlobUploadRequest,
    ) -> Result<p::CommitBlobUploadResponse, Status> {
        let upload = self.upload(principal, &request.invocation_id, &request.upload_id)?;
        let guard = CancelOnDrop(Some(upload.pending.control.clone()));
        let mut state = upload.state.lock().await;
        let current = state
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("upload closed"))?;
        if current.next != request.next_sequence || current.offset != request.bytes {
            return Err(Status::invalid_argument("upload final sequence"));
        }
        let committed = self
            .store
            .commit(
                upload.pending.scope().map_err(wire::error)?,
                current.upload.clone(),
                upload.pending.wait(),
            )
            .await
            .map_err(blob_error)?;
        let id = uuid::Uuid::new_v4().to_string();
        let descriptor = descriptor(&id, committed.metadata(), None);
        upload
            .pending
            .committed
            .lock()
            .map_err(|_| Status::unavailable("committed registry"))?
            .insert(id, (upload.port.clone(), committed));
        state.take();
        guard.disarm();
        Ok(p::CommitBlobUploadResponse {
            blob: Some(descriptor),
        })
    }
    pub(super) async fn cancel_upload(
        &self,
        principal: &str,
        request: p::CancelBlobUploadRequest,
    ) -> Result<p::CancelBlobUploadResponse, Status> {
        let upload = self.upload(principal, &request.invocation_id, &request.upload_id)?;
        upload.pending.control.cancel();
        upload.state.lock().await.take();
        Ok(p::CancelBlobUploadResponse {})
    }
    pub(super) async fn open(
        self: &Arc<Self>,
        principal: &str,
        auth: AuthenticatedPrincipal,
        request: p::OpenBlobReadRequest,
    ) -> Result<p::OpenBlobReadResponse, Status> {
        reap_reader_opens(&self.reader_admission)
            .await
            .map_err(wire::error)?;
        let id = uuid::Uuid::new_v4().to_string();
        let open = Arc::new(ReaderOpen {
            task: tokio::sync::Mutex::new(None),
            joined: Mutex::new(None),
        });
        let (send, receive) = tokio::sync::oneshot::channel();
        {
            let mut admission = self
                .reader_admission
                .lock()
                .map_err(|_| Status::unavailable("reader admission"))?;
            if admission.closed || admission.opens.len() >= 256 {
                return Err(Status::unavailable("reader admission closed or full"));
            }
            let registry = self.clone();
            let principal = principal.to_owned();
            let mut task = open
                .task
                .try_lock()
                .map_err(|_| Status::unavailable("reader admission"))?;
            admission.opens.insert(id.clone(), open.clone());
            *task = Some(tokio::spawn(async move {
                #[cfg(test)]
                if registry.driver_barrier.enabled.load(Ordering::Acquire) {
                    registry.driver_barrier.release.cancelled().await;
                }
                let result = registry.open_inner(&principal, auth, request).await;
                drop(send.send(result));
            }));
        }
        let result = receive
            .await
            .map_err(|_| Status::unavailable("reader open interrupted"));
        open.join().await.map_err(wire::error)?;
        self.reader_admission
            .lock()
            .map_err(|_| Status::unavailable("reader admission"))?
            .opens
            .remove(&id);
        result?
    }
    async fn open_inner(
        self: &Arc<Self>,
        principal: &str,
        auth: AuthenticatedPrincipal,
        request: p::OpenBlobReadRequest,
    ) -> Result<p::OpenBlobReadResponse, Status> {
        self.reap().await.map_err(wire::error)?;
        let slot = self
            .reader_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("reader capacity"))?;
        let principal_slot =
            principal_slot(&self.reader_principals, principal, 16, 256).map_err(wire::error)?;
        let wait = blob::Wait {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(30),
        };
        let (reader, access, expires, descriptor, grant) = match request
            .access
            .ok_or_else(|| Status::invalid_argument("missing blob access"))?
        {
            p::open_blob_read_request::Access::InputBlobId(id) => {
                let pending = self
                    .attachments
                    .by_invocation(principal, &request.invocation_id)?;
                let blob = pending
                    .committed
                    .lock()
                    .map_err(|_| Status::unavailable("input registry"))?
                    .get(&id)
                    .map(|(_, blob)| blob.clone())
                    .ok_or_else(|| Status::permission_denied("blob unavailable"))?;
                let scope = pending.scope().map_err(wire::error)?;
                let reader = self
                    .store
                    .reader(scope.clone(), blob.clone(), pending.wait())
                    .await
                    .map_err(blob_error)?;
                (
                    reader,
                    blob::ReadAccess::Invocation(scope),
                    pending.deadline(),
                    descriptor(&id, blob.metadata(), None),
                    None,
                )
            }
            p::open_blob_read_request::Access::OutputGrant(credentials) => {
                let grant = self
                    .grants
                    .lock()
                    .map_err(|_| Status::unavailable("output registry"))?
                    .get(&credentials.blob_id)
                    .cloned()
                    .ok_or_else(|| Status::permission_denied("blob unavailable"))?;
                if grant.principal != principal
                    || grant.invocation != request.invocation_id
                    || credentials.invocation_id != grant.invocation
                    || credentials.port != grant.port
                    || credentials.generation != self.generation
                    || !cursor::equal(&credentials.secret, &grant.secret)
                    || Instant::now() >= grant.expires
                    || self.clock.now_ms() >= grant.lease.expires_at_ms()
                    || grant.cancel.is_cancelled()
                {
                    return Err(Status::permission_denied("blob unavailable"));
                }
                {
                    let active = grant
                        .reader
                        .lock()
                        .map_err(|_| Status::unavailable("output reader registry"))?;
                    if active.is_some() {
                        return Err(Status::failed_precondition("output grant already claimed"));
                    }
                }
                #[cfg(test)]
                if self.open_barrier.enabled.load(Ordering::Acquire) {
                    self.open_barrier.entered.notify_one();
                    self.open_barrier.release.cancelled().await;
                }
                let reader = self
                    .store
                    .output_reader(principal.into(), grant.lease.clone(), wait.clone())
                    .await
                    .map_err(blob_error)?;
                let descriptor = descriptor(
                    &grant.id,
                    grant.lease.metadata(),
                    Some(
                        grant
                            .lease
                            .expires_at_ms()
                            .saturating_sub(self.clock.now_ms()),
                    ),
                );
                (
                    reader,
                    blob::ReadAccess::Output {
                        principal: principal.into(),
                    },
                    grant.expires,
                    descriptor,
                    Some(grant),
                )
            }
        };
        let owner = Arc::new(ReadOwner {
            principal: principal.into(),
            invocation: request.invocation_id,
            id: uuid::Uuid::new_v4().to_string(),
            secret: rand::random(),
            reader,
            access,
            expires,
            attached: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
            cancel: CancellationToken::new(),
            task: tokio::sync::Mutex::new(None),
            joined: Mutex::new(None),
            _slot: slot,
            _principal_slot: principal_slot,
        });
        if let Some(grant) = grant {
            let mut current = grant
                .reader
                .lock()
                .map_err(|_| Status::unavailable("output reader registry"))?;
            if current.is_some() {
                owner.revoke();
                return Err(Status::failed_precondition("output grant already claimed"));
            }
            *current = Some(Arc::downgrade(&owner));
        }
        let mut monitor = owner.task.lock().await;
        self.readers
            .lock()
            .map_err(|_| Status::unavailable("reader registry"))?
            .insert(owner.id.clone(), owner.clone());
        let active = owner.clone();
        let mut access = super::access_ended(&auth);
        let attach_by = Instant::now() + Duration::from_secs(5);
        *monitor = Some(tokio::spawn(async move {
            loop {
                let changed = active.changed.notified();
                let deadline = if active.attached.load(Ordering::Acquire) {
                    active.expires
                } else {
                    active.expires.min(attach_by)
                };
                tokio::select! { ()=active.cancel.cancelled()=>break,()=&mut access=>break,()=tokio::time::sleep_until(deadline)=>break,()=changed=>{} }
            }
            active.revoke();
        }));
        drop(monitor);
        let credentials = p::BlobReaderCredentials {
            invocation_id: owner.invocation.clone(),
            reader_id: owner.id.clone(),
            generation: self.generation.clone(),
            secret: owner.secret.to_vec(),
        };
        Ok(p::OpenBlobReadResponse {
            credentials: Some(credentials),
            blob: Some(descriptor),
        })
    }
    fn reader(
        &self,
        principal: &str,
        credentials: Option<p::BlobReaderCredentials>,
    ) -> Result<Arc<ReadOwner>, Status> {
        let credentials =
            credentials.ok_or_else(|| Status::invalid_argument("missing reader credentials"))?;
        let reader = self
            .readers
            .lock()
            .map_err(|_| Status::unavailable("reader registry"))?
            .get(&credentials.reader_id)
            .cloned()
            .ok_or_else(|| Status::permission_denied("reader unavailable"))?;
        if reader.principal != principal
            || reader.invocation != credentials.invocation_id
            || credentials.generation != self.generation
            || !cursor::equal(&credentials.secret, &reader.secret)
            || reader.cancel.is_cancelled()
            || Instant::now() >= reader.expires
        {
            return Err(Status::permission_denied("reader unavailable"));
        }
        Ok(reader)
    }
    pub(super) fn read(
        self: &Arc<Self>,
        principal: &str,
        request: p::ReadBlobRequest,
    ) -> Result<ResponseStream<p::ReadBlobResponse>, Status> {
        if request.max_bytes == 0 || request.max_bytes > 65536 {
            return Err(Status::invalid_argument("blob read bound"));
        }
        let reader = self.reader(principal, request.credentials)?;
        reader
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Status::failed_precondition("reader already consumed"))?;
        reader.attached.store(true, Ordering::Release);
        reader.changed.notify_waiters();
        let state = ReadState {
            registry: self.clone(),
            owner: reader,
            maximum: request.max_bytes as usize,
            sequence: 0,
            buffer: Vec::new(),
            offset: 0,
            eof: false,
            done: false,
            credit: None,
        };
        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            if state.done {
                return None;
            }
            if state.owner.cancel.is_cancelled() || Instant::now() >= state.owner.expires {
                state.done = true;
                return Some((Err(Status::failed_precondition("reader expired")), state));
            }
            if state.offset == state.buffer.len() && !state.eof {
                state.buffer.clear();
                state.offset = 0;
                state.credit = match state
                    .registry
                    .read_bytes
                    .clone()
                    .try_acquire_many_owned(131072)
                {
                    Ok(credit) => Some(credit),
                    Err(_) => {
                        state.done = true;
                        return Some((Err(Status::resource_exhausted("read byte budget")), state));
                    }
                };
                let wait = blob::Wait {
                    cancellation: state.owner.cancel.clone(),
                    deadline: state
                        .owner
                        .expires
                        .min(Instant::now() + Duration::from_secs(30)),
                };
                match state
                    .registry
                    .store
                    .read(state.owner.access.clone(), state.owner.reader.clone(), wait)
                    .await
                {
                    Ok(chunk) => {
                        state.buffer = chunk.bytes;
                        state.eof = chunk.eof;
                    }
                    Err(error) => {
                        state.done = true;
                        return Some((Err(blob_error(error)), state));
                    }
                }
            }
            let end = (state.offset + state.maximum).min(state.buffer.len());
            let data = state.buffer[state.offset..end].to_vec();
            state.offset = end;
            let eof = state.eof && end == state.buffer.len();
            let sequence = state.sequence;
            state.sequence += 1;
            state.done = eof;
            Some((
                Ok(p::ReadBlobResponse {
                    data,
                    sequence,
                    eof,
                }),
                state,
            ))
        })))
    }
    pub(super) async fn close(
        &self,
        principal: &str,
        request: p::CloseBlobReadRequest,
    ) -> Result<p::CloseBlobReadResponse, Status> {
        let reader = self.reader(principal, request.credentials)?;
        reader.join().await.map_err(wire::error)?;
        Ok(p::CloseBlobReadResponse {})
    }
    async fn reap(&self) -> Result<(), PortError> {
        reap_reader_opens(&self.reader_admission).await?;
        self.uploads
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .retain(|_, upload| {
                upload
                    .pending
                    .owner
                    .lock()
                    .is_ok_and(|owner| owner.is_some())
            });
        let readers: Vec<_> = self
            .readers
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .iter()
            .map(|(id, reader)| (id.clone(), reader.clone()))
            .collect();
        for (id, reader) in readers {
            if reader.cancel.is_cancelled() || Instant::now() >= reader.expires {
                reader.join().await?;
                self.readers
                    .lock()
                    .map_err(|_| PortError::Uncertain)?
                    .remove(&id);
            }
        }
        self.grants
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .retain(|_, grant| {
                let live = Instant::now() < grant.expires
                    && self.clock.now_ms() < grant.lease.expires_at_ms()
                    && !grant.cancel.is_cancelled();
                if !live {
                    grant.cancel.cancel();
                    if let Ok(reader) = grant.reader.lock()
                        && let Some(reader) = reader.as_ref().and_then(std::sync::Weak::upgrade)
                    {
                        reader.revoke();
                    }
                }
                live
            });
        Ok(())
    }
    /// Revoke and join all reader monitors; underlying store owns verified object deletion.
    pub async fn shutdown(&self) -> Result<(), PortError> {
        let opens: Vec<_> = {
            let mut admission = self
                .reader_admission
                .lock()
                .map_err(|_| PortError::Uncertain)?;
            admission.closed = true;
            admission.opens.values().cloned().collect()
        };
        for open in opens {
            open.join().await?;
        }
        let readers: Vec<_> = self
            .readers
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .values()
            .cloned()
            .collect();
        for reader in &readers {
            reader.revoke();
        }
        for reader in readers {
            reader.join().await?;
        }
        self.readers
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clear();
        self.grants
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clear();
        self.uploads
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clear();
        self.reader_admission
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .opens
            .clear();
        Ok(())
    }
    #[cfg(test)]
    pub(super) async fn test_reclaim_abandoned_opens(
        self: &Arc<Self>,
        principal: String,
        auth: AuthenticatedPrincipal,
        request: p::OpenBlobReadRequest,
    ) {
        self.driver_barrier.enabled.store(true, Ordering::Release);
        let mut callers = Vec::new();
        for i in 0..256 {
            let registry = self.clone();
            let principal = principal.clone();
            let auth = auth.clone();
            let request = if i == 0 {
                request.clone()
            } else {
                p::OpenBlobReadRequest {
                    invocation_id: request.invocation_id.clone(),
                    access: None,
                }
            };
            callers.push(tokio::spawn(async move {
                registry.open(&principal, auth, request).await
            }));
        }
        while self.reader_admission.lock().unwrap().opens.len() < 256 {
            tokio::task::yield_now().await;
        }
        for caller in callers {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        reap_reader_opens(&self.reader_admission).await.unwrap();
        assert_eq!(self.reader_admission.lock().unwrap().opens.len(), 256);
        self.driver_barrier.release.cancel();
        loop {
            let opens: Vec<_> = self
                .reader_admission
                .lock()
                .unwrap()
                .opens
                .values()
                .cloned()
                .collect();
            let mut done = true;
            for open in opens {
                done &= open
                    .task
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished);
            }
            if done {
                break;
            }
            tokio::task::yield_now().await;
        }
        for owner in self.readers.lock().unwrap().values() {
            owner.revoke();
        }
        self.reap().await.unwrap();
        assert!(self.reader_admission.lock().unwrap().opens.is_empty());
        assert!(self.readers.lock().unwrap().is_empty());
        assert_eq!(self.reader_slots.available_permits(), 256);
        assert!(!self.reader_admission.lock().unwrap().closed);
    }
    #[cfg(test)]
    pub(super) async fn test_shutdown_during_open(
        self: &Arc<Self>,
        principal: String,
        auth: AuthenticatedPrincipal,
        request: p::OpenBlobReadRequest,
    ) {
        self.open_barrier.enabled.store(true, Ordering::Release);
        let registry = self.clone();
        let owner = principal.clone();
        let credentials = auth.clone();
        let input = request.clone();
        let opening = tokio::spawn(async move { registry.open(&owner, credentials, input).await });
        self.open_barrier.entered.notified().await;
        let registry = self.clone();
        let shutdown = tokio::spawn(async move { registry.shutdown().await });
        while !self.reader_admission.lock().unwrap().closed {
            tokio::task::yield_now().await;
        }
        assert!(!shutdown.is_finished());
        assert!(self.open(&principal, auth, request).await.is_err());
        // Both abandoned callers leave the actual open task and its admission owned.
        opening.abort();
        assert!(opening.await.unwrap_err().is_cancelled());
        shutdown.abort();
        assert!(shutdown.await.unwrap_err().is_cancelled());
        let registry = self.clone();
        let retry = tokio::spawn(async move { registry.shutdown().await });
        tokio::task::yield_now().await;
        assert!(!retry.is_finished());
        self.open_barrier.release.cancel();
        retry.await.unwrap().unwrap();
        self.shutdown().await.unwrap();
        assert!(self.reader_admission.lock().unwrap().opens.is_empty());
        assert!(self.readers.lock().unwrap().is_empty());
        assert_eq!(self.reader_slots.available_permits(), 256);
        assert!(
            self.reader_principals
                .lock()
                .unwrap()
                .values()
                .all(|pool| pool.available_permits() == 16)
        );
    }
}
impl Drop for BlobRegistry {
    fn drop(&mut self) {
        if let Ok(readers) = self.readers.lock() {
            for reader in readers.values() {
                reader.revoke();
            }
        }
    }
}
fn principal_slot(
    pools: &Mutex<BTreeMap<String, Arc<Semaphore>>>,
    principal: &str,
    limit: usize,
    maximum: usize,
) -> Result<OwnedSemaphorePermit, PortError> {
    let mut pools = pools.lock().map_err(|_| PortError::Uncertain)?;
    pools.retain(|_, pool| pool.available_permits() != limit);
    if !pools.contains_key(principal) && pools.len() >= maximum {
        return Err(PortError::Unavailable);
    }
    pools
        .entry(principal.into())
        .or_insert_with(|| Arc::new(Semaphore::new(limit)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| PortError::Unavailable)
}
#[cfg(test)]
mod quota_tests {
    use super::*;
    #[tokio::test]
    async fn routine_open_reaper_retains_uncertain_driver() {
        let open = Arc::new(ReaderOpen {
            task: tokio::sync::Mutex::new(Some(tokio::spawn(async {
                panic!("open driver panic")
            }))),
            joined: Mutex::new(None),
        });
        while !open.task.lock().await.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        let admission = Mutex::new(ReaderAdmission {
            closed: false,
            opens: BTreeMap::from([("failed".into(), open.clone())]),
        });
        assert_eq!(
            reap_reader_opens(&admission).await,
            Err(PortError::Uncertain)
        );
        assert_eq!(
            reap_reader_opens(&admission).await,
            Err(PortError::Uncertain)
        );
        assert_eq!(admission.lock().unwrap().opens.len(), 1);
        assert_eq!(open.join().await, Err(PortError::Uncertain));
    }
    #[test]
    fn routine_principal_quota_reserves_before_async_open_and_releases_verified_owners() {
        let pools = Mutex::new(BTreeMap::new());
        let held: Vec<_> = (0..16)
            .map(|_| principal_slot(&pools, "owner", 16, 256).unwrap())
            .collect();
        assert!(principal_slot(&pools, "owner", 16, 256).is_err());
        let foreign = principal_slot(&pools, "other", 16, 256).unwrap();
        assert_eq!(pools.lock().unwrap().len(), 2);
        drop(held);
        let next = principal_slot(&pools, "owner", 16, 256).unwrap();
        drop(next);
        drop(foreign);
        let last = principal_slot(&pools, "new", 16, 256).unwrap();
        assert_eq!(pools.lock().unwrap().len(), 1);
        drop(last);
    }
}
struct ReadState {
    registry: Arc<BlobRegistry>,
    owner: Arc<ReadOwner>,
    maximum: usize,
    sequence: u64,
    buffer: Vec<u8>,
    offset: usize,
    eof: bool,
    done: bool,
    credit: Option<OwnedSemaphorePermit>,
}
impl Drop for ReadState {
    fn drop(&mut self) {
        self.owner.revoke();
    }
}
fn same_metadata(a: &blob::UploadMetadata, b: &blob::UploadMetadata) -> bool {
    a.display_name == b.display_name
        && a.content_type == b.content_type
        && a.kind == b.kind
        && a.declared_bytes == b.declared_bytes
        && a.expected_sha256 == b.expected_sha256
}
fn descriptor(
    id: &str,
    metadata: &blob::BlobMetadata,
    remaining_validity_ms: Option<u64>,
) -> p::BlobDescriptor {
    p::BlobDescriptor {
        blob_id: id.into(),
        metadata: Some(p::BlobMetadata {
            display_name: metadata.display_name.clone(),
            content_type: metadata.content_type.clone(),
            bytes: metadata.bytes,
            sha256: metadata.sha256.to_vec(),
            image: None,
        }),
        remaining_validity_ms,
    }
}
fn blob_error(error: blob::BlobError) -> Status {
    match error {
        blob::BlobError::Invalid | blob::BlobError::Sequence => {
            Status::invalid_argument("invalid blob operation")
        }
        blob::BlobError::Denied => Status::permission_denied("blob unavailable"),
        blob::BlobError::Stale => Status::failed_precondition("blob unavailable"),
        blob::BlobError::Quota => Status::resource_exhausted("blob budget"),
        blob::BlobError::Cancelled => Status::cancelled("blob cancelled"),
        blob::BlobError::Deadline => Status::deadline_exceeded("blob deadline"),
        _ => Status::unavailable("blob unavailable"),
    }
}
