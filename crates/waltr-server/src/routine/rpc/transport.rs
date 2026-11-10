//! Cold request-local media association and owned, bounded interaction transport.
#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

use super::wire;
use crate::routine::{
    adapters::media::{BlobInput, MediaBindingFactory, MediaBindings, MediaDriver, UploadSource},
    blob::{self, BlobStore},
    definition::{Definition, Direction, Media},
    history::{Caller, InvocationBinding},
    media::stream::{self, Chunk, Format, Reader, Streams, Text, Writer},
    observation::{ObservationScope, ObservationStore, Publication},
    ports::{PortError, PortFuture, SelectedEntrypoint},
};
use prost::Message as _;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tonic::Status;
use waltr::grpc::waltr::routine::v2 as p;

#[derive(Clone)]
enum Declaration {
    Stream(Format),
    Binary(blob::UploadMetadata),
}
#[derive(Clone, Default)]
struct Logical {
    batch: u64,
    finished: bool,
    ports: BTreeMap<String, (Declaration, bool)>,
    streams: BTreeMap<String, InputStream>,
    credit: Arc<Vec<OwnedSemaphorePermit>>,
}
#[derive(Clone)]
struct InputStream {
    port: String,
    format: Format,
    next: u64,
    closed: bool,
    segments: BTreeMap<String, (usize, bool)>,
    bytes: u64,
}
enum Event {
    Open,
    Chunk(String, Chunk),
    Close(String),
    Blob(String, blob::CommittedBlob),
    Finish,
}
struct Batch {
    events: Vec<Event>,
    _credit: Vec<OwnedSemaphorePermit>,
}
pub(super) struct OutputFrame {
    pub event: p::OutputEvent,
    pub consumed: Option<tokio::sync::oneshot::Sender<()>>,
    pub _packet: Option<stream::Packet>,
    pub _credit: Vec<OwnedSemaphorePermit>,
}
struct Cold {
    readers: BTreeMap<String, Reader>,
    outputs: BTreeMap<String, Writer>,
    uploads: BTreeMap<String, BlobInput>,
    writers: BTreeMap<String, Writer>,
    consumers: BTreeMap<String, Reader>,
}
struct BlobSourceState {
    blob: Option<blob::CommittedBlob>,
    reader: Option<blob::BlobReader>,
    done: bool,
}
struct Source {
    pending: Arc<SourcePending>,
}
struct SourcePending {
    state: tokio::sync::Mutex<BlobSourceState>,
    ready: tokio::sync::Notify,
    cancel: CancellationToken,
    store: Arc<dyn BlobStore>,
    scope: Arc<Mutex<Option<blob::BlobScope>>>,
}
impl UploadSource for Source {
    fn next(&mut self, cancellation: CancellationToken) -> PortFuture<Option<Vec<u8>>> {
        let pending = self.pending.clone();
        Box::pin(async move {
            loop {
                let ready = pending.ready.notified();
                let mut state = pending.state.lock().await;
                if state.done {
                    return Ok(None);
                }
                if let Some(blob) = state.blob.clone() {
                    let scope = pending
                        .scope
                        .lock()
                        .map_err(|_| PortError::Uncertain)?
                        .clone()
                        .ok_or(PortError::Denied)?;
                    let wait = blob::Wait {
                        cancellation: cancellation.clone(),
                        deadline: tokio::time::Instant::now() + Duration::from_secs(30),
                    };
                    if state.reader.is_none() {
                        state.reader = Some(
                            pending
                                .store
                                .reader(scope.clone(), blob, wait.clone())
                                .await
                                .map_err(|_| PortError::Uncertain)?,
                        );
                    }
                    let reader = state.reader.as_ref().ok_or(PortError::Uncertain)?.clone();
                    let chunk = pending
                        .store
                        .read(blob::ReadAccess::Invocation(scope), reader, wait)
                        .await
                        .map_err(|_| PortError::Uncertain)?;
                    if chunk.eof {
                        state.done = true;
                    }
                    return if chunk.bytes.is_empty() && chunk.eof {
                        Ok(None)
                    } else {
                        Ok(Some(chunk.bytes))
                    };
                }
                drop(state);
                tokio::select! { ()=cancellation.cancelled()=>return Err(PortError::Unavailable),()=pending.cancel.cancelled()=>return Err(PortError::Unavailable),()=ready=>{} }
            }
        })
    }
    fn close(&mut self) -> PortFuture<()> {
        let pending = self.pending.clone();
        pending.cancel.cancel();
        Box::pin(async move {
            let mut state = pending.state.lock().await;
            if let Some(reader) = state.reader.take() {
                reader.close();
            }
            state.blob = None;
            Ok(())
        })
    }
}
/// Pending authenticated request authority, created before invoke/preflight, never from a guest ID.
pub struct PendingTransport {
    pub(super) principal: String,
    definition: Arc<Definition>,
    streams: Streams,
    pub(super) control: CancellationToken,
    cold: Mutex<Option<Cold>>,
    logical: tokio::sync::Mutex<Logical>,
    ingress: mpsc::Sender<Batch>,
    ingress_rx: tokio::sync::Mutex<Option<mpsc::Receiver<Batch>>>,
    output: mpsc::Sender<OutputFrame>,
    pub(super) output_rx: Mutex<Option<mpsc::Receiver<OutputFrame>>>,
    binding: Mutex<Option<InvocationBinding>>,
    scope: Arc<Mutex<Option<blob::BlobScope>>>,
    pub(super) owner: Mutex<Option<blob::InvocationOwner>>,
    pub(super) declarations: BTreeMap<String, blob::UploadMetadata>,
    pub(super) committed: Mutex<BTreeMap<String, (String, blob::CommittedBlob)>>,
    sources: BTreeMap<String, Arc<SourcePending>>,
    jobs: tokio::sync::Mutex<Vec<JoinHandle<Result<(), PortError>>>>,
    started: Mutex<bool>,
    joined: Mutex<Option<bool>>,
    cleanup_failed: Mutex<bool>,
    input_done: Mutex<bool>,
    principal_credit: Arc<Semaphore>,
    process_credit: Arc<Semaphore>,
    store: Arc<dyn ObservationStore>,
    pub(super) blob_registry: Mutex<Option<std::sync::Weak<super::blobs::BlobRegistry>>>,
    input_finish: CancellationToken,
    native_required: std::sync::atomic::AtomicBool,
    native_eof: CancellationToken,
}
impl std::fmt::Debug for PendingTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingTransport").finish_non_exhaustive()
    }
}
impl PendingTransport {
    pub(super) async fn own_task(
        self: &Arc<Self>,
        work: impl std::future::Future<Output = Result<(), PortError>> + Send + 'static,
    ) -> Result<(), Status> {
        let mut jobs = self.jobs.lock().await;
        if self.streams.cancellation().is_cancelled()
            || self
                .joined
                .lock()
                .map_err(|_| Status::unavailable("transport owner"))?
                .is_some()
        {
            return Err(Status::failed_precondition("transport owner closed"));
        }
        let pending = self.clone();
        jobs.push(tokio::spawn(async move {
            let result = work.await;
            if result.is_err() {
                pending.control.cancel();
            }
            result
        }));
        Ok(())
    }
    pub(super) fn definition_revision(&self) -> &str {
        self.definition.revision()
    }
    pub(super) async fn next_sequence(&self) -> u64 {
        self.logical.lock().await.batch
    }
    pub(super) fn require_native_eof(&self) {
        self.native_required
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(super) fn is_native(&self) -> bool {
        self.native_required
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub(super) async fn native_closed(&self) -> Result<(), Status> {
        if !self.logical.lock().await.finished {
            return Err(Status::invalid_argument("native input EOF before finish"));
        }
        self.native_eof.cancel();
        Ok(())
    }
    pub(super) async fn finish_input(&self) -> Result<(), Status> {
        let mut state = self.logical.lock().await;
        if !state.finished {
            if state.ports.values().any(|(_, closed)| !closed) {
                return Err(Status::failed_precondition("input ports not closed"));
            }
            state.finished = true;
            self.input_finish.cancel();
        }
        Ok(())
    }
    pub(super) async fn selected_text(&self) -> Result<Vec<p::TextResultDescriptor>, Status> {
        let binding = self.binding().map_err(wire::error)?;
        self.store
            .get(
                ObservationScope {
                    principal: self.principal.clone(),
                    source_session: None,
                },
                binding.id().into(),
            )
            .await
            .map_err(wire::error)?
            .ok_or_else(|| Status::unavailable("terminal view unavailable"))
            .map(|view| view.text_results.into_iter().map(wire::text).collect())
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "all process and principal resource pools are explicitly injected"
    )]
    pub(super) fn new(
        principal: String,
        definition: Arc<Definition>,
        declarations: Vec<p::InputDeclaration>,
        principal_pool: stream::Pool,
        process_pool: stream::Pool,
        principal_credit: Arc<Semaphore>,
        process_credit: Arc<Semaphore>,
        store: Arc<dyn ObservationStore>,
        blobs: Arc<dyn BlobStore>,
    ) -> Result<Arc<Self>, Status> {
        if declarations.len() > 32 {
            return Err(Status::invalid_argument("too many input declarations"));
        }
        let mut logical = Logical::default();
        let mut binaries = BTreeMap::new();
        logical.credit = Arc::new(vec![
            process_credit
                .clone()
                .try_acquire_many_owned(65536)
                .map_err(|_| Status::resource_exhausted("transport credit"))?,
            principal_credit
                .clone()
                .try_acquire_many_owned(65536)
                .map_err(|_| Status::resource_exhausted("transport credit"))?,
        ]);
        for input in declarations {
            wire::id(&input.port, 128)?;
            let port = definition
                .envelope()
                .spec
                .ports
                .get(&input.port)
                .ok_or_else(|| Status::invalid_argument("unknown input port"))?;
            if !matches!(port.direction, Direction::Input)
                || logical.ports.contains_key(&input.port)
            {
                return Err(Status::invalid_argument("invalid input port"));
            }
            let declaration = match input.format {
                Some(p::input_declaration::Format::Pcm16le(format))
                    if matches!(port.media, Media::Audio) =>
                {
                    let format = Format::Pcm16Le {
                        rate: format.sample_rate,
                        channels: u16::try_from(format.channels)
                            .map_err(|_| Status::invalid_argument("PCM format"))?,
                    };
                    format
                        .validate()
                        .map_err(|_| Status::invalid_argument("PCM format"))?;
                    let Format::Pcm16Le { rate, channels } = format else {
                        unreachable!()
                    };
                    if !port.formats.iter().any(|identity| {
                        identity
                            == &format!(
                                "pcm16le-{rate}-{}",
                                if channels == 1 {
                                    "mono".into()
                                } else {
                                    format!("{channels}ch")
                                }
                            )
                    }) {
                        return Err(Status::invalid_argument("undeclared PCM format"));
                    }
                    Declaration::Stream(format)
                }
                Some(p::input_declaration::Format::Text(_))
                    if matches!(port.media, Media::Text)
                        && port.formats.iter().any(|format| format == "utf8") =>
                {
                    Declaration::Stream(Format::Text)
                }
                Some(p::input_declaration::Format::Binary(metadata))
                    if matches!(port.media, Media::File | Media::Image) =>
                {
                    let metadata = binary_metadata(metadata)?;
                    if metadata
                        .declared_bytes
                        .is_none_or(|bytes| bytes > u64::from(port.max_bytes))
                        || (matches!(port.media, Media::Image)
                            && !matches!(metadata.kind, blob::BlobKind::Image(_)))
                        || !port.formats.iter().any(|format| format == "blob")
                    {
                        return Err(Status::invalid_argument("binary port metadata"));
                    }
                    binaries.insert(input.port.clone(), metadata.clone());
                    Declaration::Binary(metadata)
                }
                _ => return Err(Status::invalid_argument("input format mismatch")),
            };
            logical.ports.insert(input.port, (declaration, false));
        }
        if definition.envelope().spec.ports.iter().any(|(name, port)| {
            matches!(port.direction, Direction::Input)
                && port.required
                && !logical.ports.contains_key(name)
        }) {
            return Err(Status::invalid_argument(
                "missing required input declaration",
            ));
        }
        let cancellation = CancellationToken::new();
        let deadline = Duration::from_millis(definition.envelope().spec.budgets.deadline_ms);
        let limits = stream::Limits {
            input_duration: deadline.min(Duration::from_secs(3900)),
            total_duration: deadline,
            text_bytes: 2097152,
            ..Default::default()
        };
        let streams = Streams::new(limits, principal_pool, process_pool, cancellation)
            .map_err(|_| Status::invalid_argument("media limits"))?;
        let scope = Arc::new(Mutex::new(None));
        let mut sources = BTreeMap::new();
        let mut cold = Cold {
            readers: BTreeMap::new(),
            writers: BTreeMap::new(),
            outputs: BTreeMap::new(),
            consumers: BTreeMap::new(),
            uploads: BTreeMap::new(),
        };
        for (name, (declaration, _)) in &logical.ports {
            match declaration {
                Declaration::Stream(format) => {
                    let (writer, reader) =
                        streams
                            .open(stream::Direction::Input, *format, false)
                            .map_err(|_| Status::invalid_argument("input stream limit"))?;
                    cold.writers.insert(name.clone(), writer);
                    cold.readers.insert(name.clone(), reader);
                }
                Declaration::Binary(metadata) => {
                    let pending = Arc::new(SourcePending {
                        state: tokio::sync::Mutex::new(BlobSourceState {
                            blob: None,
                            reader: None,
                            done: false,
                        }),
                        ready: tokio::sync::Notify::new(),
                        cancel: streams.cancellation().child_token(),
                        store: blobs.clone(),
                        scope: scope.clone(),
                    });
                    sources.insert(name.clone(), pending.clone());
                    cold.uploads.insert(
                        name.clone(),
                        BlobInput {
                            metadata: metadata.clone(),
                            source: Box::new(Source { pending }),
                        },
                    );
                }
            }
        }
        for (name, port) in &definition.envelope().spec.ports {
            if !matches!(port.direction, Direction::Output) {
                continue;
            }
            let format = match port.media {
                Media::Text if port.formats.iter().any(|f| f == "utf8") => Format::Text,
                Media::Audio if port.formats.iter().any(|f| f == "pcm16le-24000-mono") => {
                    Format::Pcm16Le {
                        rate: 24000,
                        channels: 1,
                    }
                }
                Media::Image | Media::File => continue,
                _ => return Err(Status::invalid_argument("unsupported output format")),
            };
            let (writer, reader) = streams
                .open(stream::Direction::Output, format, false)
                .map_err(|_| Status::invalid_argument("output stream limit"))?;
            cold.outputs.insert(name.clone(), writer);
            cold.consumers.insert(name.clone(), reader);
        }
        let (ingress, ingress_rx) = mpsc::channel(8);
        let (output, output_rx) = mpsc::channel(16);
        Ok(Arc::new(Self {
            principal,
            definition,
            streams,
            control: CancellationToken::new(),
            cold: Mutex::new(Some(cold)),
            logical: tokio::sync::Mutex::new(logical),
            ingress,
            ingress_rx: tokio::sync::Mutex::new(Some(ingress_rx)),
            output,
            output_rx: Mutex::new(Some(output_rx)),
            binding: Mutex::new(None),
            scope,
            owner: Mutex::new(None),
            declarations: binaries,
            committed: Mutex::new(BTreeMap::new()),
            sources,
            jobs: tokio::sync::Mutex::new(Vec::new()),
            started: Mutex::new(false),
            joined: Mutex::new(None),
            cleanup_failed: Mutex::new(false),
            input_done: Mutex::new(false),
            principal_credit,
            process_credit,
            store,
            blob_registry: Mutex::new(None),
            input_finish: CancellationToken::new(),
            native_required: std::sync::atomic::AtomicBool::new(false),
            native_eof: CancellationToken::new(),
        }))
    }
    /// Inject cold request-local edges into an existing normal media composition.
    pub fn bind_factory(
        self: &Arc<Self>,
        inner: Arc<dyn MediaBindingFactory>,
    ) -> Arc<dyn MediaBindingFactory> {
        Arc::new(Bind {
            pending: self.clone(),
            inner,
        })
    }
    pub(super) fn binding(&self) -> Result<InvocationBinding, PortError> {
        self.binding
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clone()
            .ok_or(PortError::Denied)
    }
    pub(super) fn scope(&self) -> Result<blob::BlobScope, PortError> {
        self.scope
            .lock()
            .map_err(|_| PortError::Uncertain)?
            .clone()
            .ok_or(PortError::Denied)
    }
    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.streams.deadline()
    }
    pub(super) async fn cancelled(&self) {
        self.streams.cancellation().cancelled().await;
    }
    pub(super) fn wait(&self) -> blob::Wait {
        blob::Wait {
            cancellation: self.streams.cancellation(),
            deadline: self
                .streams
                .deadline()
                .min(tokio::time::Instant::now() + Duration::from_secs(30)),
        }
    }
    pub(super) async fn credits(&self, bytes: usize) -> Result<Vec<OwnedSemaphorePermit>, Status> {
        let bytes = u32::try_from(bytes.max(1))
            .map_err(|_| Status::resource_exhausted("transport credit"))?;
        let mut permits = Vec::new();
        for pool in [&self.process_credit, &self.principal_credit] {
            let permit = tokio::select! { ()=self.control.cancelled()=>return Err(Status::cancelled("interaction cancelled")),()=self.streams.cancellation().cancelled_owned()=>return Err(Status::cancelled("media cancelled")),result=tokio::time::timeout(Duration::from_secs(30),pool.clone().acquire_many_owned(bytes))=>result.map_err(|_|Status::deadline_exceeded("transport backpressure"))?.map_err(|_|Status::unavailable("transport closed"))? };
            permits.push(permit);
        }
        Ok(permits)
    }
    pub(super) fn try_credits(&self, bytes: usize) -> Result<Vec<OwnedSemaphorePermit>, Status> {
        let bytes = u32::try_from(bytes.max(1))
            .map_err(|_| Status::resource_exhausted("transport credit"))?;
        Ok(vec![
            self.process_credit
                .clone()
                .try_acquire_many_owned(bytes)
                .map_err(|_| Status::resource_exhausted("process transport credit"))?,
            self.principal_credit
                .clone()
                .try_acquire_many_owned(bytes)
                .map_err(|_| Status::resource_exhausted("principal transport credit"))?,
        ])
    }
    pub(super) async fn send(
        &self,
        sequence: u64,
        events: Vec<p::InputEvent>,
    ) -> Result<(), Status> {
        let bytes = events
            .iter()
            .map(prost::Message::encoded_len)
            .sum::<usize>();
        if events.is_empty() || events.len() > 32 || bytes > 131072 {
            return Err(Status::invalid_argument("input batch bound"));
        }
        let credit = self.try_credits(bytes * 2 + 1024)?;
        let permit = tokio::select! { ()=self.control.cancelled()=>return Err(Status::cancelled("interaction cancelled")),result=tokio::time::timeout(Duration::from_secs(30),self.ingress.reserve())=>result.map_err(|_|Status::deadline_exceeded("input backpressure"))?.map_err(|_|Status::failed_precondition("input closed"))? };
        let mut logical = self.logical.lock().await;
        if logical.finished || sequence != logical.batch {
            return Err(Status::invalid_argument("input batch sequence"));
        }
        let metadata = logical
            .streams
            .iter()
            .map(|(id, stream)| {
                id.len()
                    + stream.port.len()
                    + 128
                    + stream
                        .segments
                        .keys()
                        .map(|key| key.len() + 64)
                        .sum::<usize>()
            })
            .sum::<usize>()
            + 65536;
        let metadata_credit = self.try_credits(metadata * 2 + bytes * 2 + 1024)?;
        let mut next = logical.clone();
        next.credit = Arc::new(metadata_credit);
        let mut admitted = Vec::with_capacity(events.len());
        for event in events {
            admitted.push(self.validate(&mut next, event)?);
        }
        next.batch = next
            .batch
            .checked_add(1)
            .ok_or_else(|| Status::resource_exhausted("input batch count"))?;
        *logical = next;
        permit.send(Batch {
            events: admitted,
            _credit: credit,
        });
        Ok(())
    }
    fn validate(&self, state: &mut Logical, event: p::InputEvent) -> Result<Event, Status> {
        if state.finished {
            return Err(Status::invalid_argument("input already finished"));
        }
        let invalid = || Status::invalid_argument("invalid ordered input event");
        match event.event.ok_or_else(invalid)? {
            p::input_event::Event::Open(open) => {
                wire::id(&open.stream_id, 128)?;
                wire::id(&open.port, 128)?;
                if state.streams.contains_key(&open.stream_id)
                    || state.streams.values().any(|s| s.port == open.port)
                {
                    return Err(invalid());
                }
                let Some((Declaration::Stream(format), false)) = state.ports.get(&open.port) else {
                    return Err(invalid());
                };
                let actual = match open.format {
                    Some(p::stream_open::Format::Text(_)) => Format::Text,
                    Some(p::stream_open::Format::Pcm16le(pcm)) => Format::Pcm16Le {
                        rate: pcm.sample_rate,
                        channels: u16::try_from(pcm.channels).map_err(|_| invalid())?,
                    },
                    _ => return Err(invalid()),
                };
                if actual != *format {
                    return Err(invalid());
                }
                state.streams.insert(
                    open.stream_id,
                    InputStream {
                        port: open.port,
                        format: actual,
                        next: 0,
                        closed: false,
                        segments: BTreeMap::new(),
                        bytes: 0,
                    },
                );
                Ok(Event::Open)
            }
            p::input_event::Event::Chunk(chunk) => {
                let stream = state
                    .streams
                    .get_mut(&chunk.stream_id)
                    .ok_or_else(invalid)?;
                if stream.closed || stream.next != chunk.sequence {
                    return Err(invalid());
                }
                let payload = match (stream.format, chunk.payload) {
                    (
                        Format::Pcm16Le { channels, .. },
                        Some(p::stream_chunk::Payload::Audio(bytes)),
                    ) if !bytes.is_empty()
                        && bytes.len() <= 65536
                        && bytes.len() % (usize::from(channels) * 2) == 0 =>
                    {
                        Chunk::Audio(bytes)
                    }
                    (Format::Text, Some(p::stream_chunk::Payload::Text(text))) => {
                        wire::id(&text.segment_id, 128)?;
                        if text.segment_id.len() + text.text.len() > 65536 {
                            return Err(invalid());
                        }
                        let previous = stream.segments.get(&text.segment_id).copied();
                        if previous.is_some_and(|(_, committed)| committed)
                            || (!matches!(text.mutation, 1 | 2))
                            || text.mutation == 2 && previous.is_none()
                            || previous.is_none() && stream.segments.len() >= 4096
                        {
                            return Err(invalid());
                        }
                        let length = if text.mutation == 1 {
                            previous.map_or(0, |p| p.0) + text.text.len()
                        } else {
                            text.text.len()
                        };
                        stream
                            .segments
                            .insert(text.segment_id.clone(), (length, text.committed));
                        if stream.segments.values().map(|p| p.0).sum::<usize>() > 1048576 {
                            return Err(invalid());
                        }
                        Chunk::Text(Text {
                            segment: text.segment_id,
                            text: text.text,
                            replace: text.mutation == 2,
                            committed: text.committed,
                        })
                    }
                    _ => return Err(invalid()),
                };
                stream.bytes = stream
                    .bytes
                    .checked_add(match &payload {
                        Chunk::Audio(bytes) => bytes.len() as u64,
                        Chunk::Text(text) => text.text.len() as u64,
                    })
                    .ok_or_else(invalid)?;
                if stream.bytes
                    > u64::from(self.definition.envelope().spec.ports[&stream.port].max_bytes)
                {
                    return Err(invalid());
                }
                stream.next = stream.next.checked_add(1).ok_or_else(invalid)?;
                Ok(Event::Chunk(stream.port.clone(), payload))
            }
            p::input_event::Event::Close(close) => {
                let stream = state
                    .streams
                    .get_mut(&close.stream_id)
                    .ok_or_else(invalid)?;
                if stream.closed
                    || close.next_sequence != stream.next
                    || stream.segments.values().any(|(_, committed)| !committed)
                {
                    return Err(invalid());
                }
                stream.closed = true;
                state.ports.get_mut(&stream.port).ok_or_else(invalid)?.1 = true;
                Ok(Event::Close(stream.port.clone()))
            }
            p::input_event::Event::Blob(input) => {
                let Some((Declaration::Binary(_), closed)) = state.ports.get_mut(&input.port)
                else {
                    return Err(invalid());
                };
                if *closed {
                    return Err(invalid());
                }
                let committed = self
                    .committed
                    .lock()
                    .map_err(|_| Status::unavailable("blob registry unavailable"))?;
                let Some((port, blob)) = committed.get(&input.blob_id) else {
                    return Err(invalid());
                };
                if port != &input.port {
                    return Err(invalid());
                }
                *closed = true;
                Ok(Event::Blob(input.port, blob.clone()))
            }
            p::input_event::Event::Finish(_) => {
                if state.ports.values().any(|(_, closed)| !closed) {
                    return Err(invalid());
                }
                state.finished = true;
                Ok(Event::Finish)
            }
            p::input_event::Event::Cancel(_) => {
                Err(Status::invalid_argument("cancel uses independent control"))
            }
        }
    }
}
struct Bind {
    pending: Arc<PendingTransport>,
    inner: Arc<dyn MediaBindingFactory>,
}
impl MediaBindingFactory for Bind {
    fn bind(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<MediaBindings> {
        let pending = self.pending.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            if binding.principal() != pending.principal
                || definition.revision() != pending.definition.revision()
            {
                return Err(PortError::Denied);
            }
            {
                let mut bound = pending.binding.lock().map_err(|_| PortError::Uncertain)?;
                if bound.is_some() {
                    return Err(PortError::Conflict);
                }
                *pending.scope.lock().map_err(|_| PortError::Uncertain)? = Some(blob::BlobScope {
                    principal: binding.principal().into(),
                    invocation: binding.id().into(),
                });
                *bound = Some(binding.clone());
            }
            let mut bindings = inner.bind(binding, definition, selected, caller).await?;
            let mut cold = pending.cold.lock().map_err(|_| PortError::Uncertain)?;
            let cold = cold.as_mut().ok_or(PortError::Conflict)?;
            bindings.inputs = std::mem::take(&mut cold.readers);
            bindings.outputs = std::mem::take(&mut cold.outputs);
            bindings.uploads = std::mem::take(&mut cold.uploads);
            bindings.streams = pending.streams.clone();
            bindings.driver = Arc::new(Driver(pending.clone()));
            Ok(bindings)
        })
    }
}
struct Driver(Arc<PendingTransport>);
impl MediaDriver for Driver {
    fn committed_input(&self, port: String) -> PortFuture<Option<blob::CommittedBlob>> {
        let pending = self.0.clone();
        Box::pin(async move {
            let Some(source) = pending.sources.get(&port) else {
                return Ok(None);
            };
            loop {
                let ready = source.ready.notified();
                if let Some(blob) = source.state.lock().await.blob.clone() {
                    return Ok(Some(blob));
                }
                tokio::select! { ()=source.cancel.cancelled()=>return Err(PortError::Unavailable),()=ready=>{} }
            }
        })
    }
    fn bind_blobs(
        &self,
        binding: InvocationBinding,
        owner: blob::InvocationOwner,
    ) -> Result<(), PortError> {
        if self.0.binding()?.id() != binding.id() || binding.principal() != self.0.principal {
            return Err(PortError::Denied);
        }
        *self.0.owner.lock().map_err(|_| PortError::Uncertain)? = Some(owner);
        Ok(())
    }
    fn record_helper(&self, session_id: String, definition_identity: String) -> PortFuture<()> {
        let pending = self.0.clone();
        Box::pin(async move {
            let binding = pending.binding()?;
            pending
                .store
                .publish(
                    ObservationScope {
                        principal: binding.principal().into(),
                        source_session: None,
                    },
                    binding.id().into(),
                    Publication::Helper {
                        session_id,
                        definition_identity,
                    },
                )
                .await
        })
    }
    fn record_binary(&self, port: String, lease: blob::OutputLease) -> PortFuture<()> {
        let pending = self.0.clone();
        Box::pin(async move {
            let registry = pending
                .blob_registry
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .ok_or(PortError::UnsupportedCapability)?;
            let output = registry.publish(pending.binding()?, port, lease).await?;
            pending
                .emit(p::output_event::Event::Blob(output), None)
                .await
        })
    }
    fn start(&self) -> PortFuture<()> {
        let pending = self.0.clone();
        Box::pin(async move {
            {
                let mut started = pending.started.lock().map_err(|_| PortError::Uncertain)?;
                if *started {
                    return Ok(());
                }
                *started = true;
            }
            let mut rx = pending
                .ingress_rx
                .lock()
                .await
                .take()
                .ok_or(PortError::Conflict)?;
            let (mut writers, consumers) = {
                let mut cold = pending.cold.lock().map_err(|_| PortError::Uncertain)?;
                let cold = cold.as_mut().ok_or(PortError::Conflict)?;
                (
                    std::mem::take(&mut cold.writers),
                    std::mem::take(&mut cold.consumers),
                )
            };
            let input = pending.clone();
            let mut jobs = pending.jobs.lock().await;
            jobs.push(tokio::spawn(async move {
                loop {
                    if rx.is_empty()&&input.input_finish.is_cancelled() { *input.input_done.lock().map_err(|_|PortError::Uncertain)?=true;return Ok(()); }
                    let batch=tokio::select! { ()=input.streams.cancellation().cancelled_owned()=>return Err(PortError::Unavailable),()=input.input_finish.cancelled(),if rx.is_empty()=>continue,batch=rx.recv()=>batch.ok_or(PortError::Unavailable)? };
                    for event in batch.events {
                        match event {
                            Event::Open=>{},Event::Chunk(port,chunk)=> { let writer=writers.get_mut(&port).ok_or(PortError::Invalid)?;writer.write(writer.key(),chunk).await.map_err(|_|PortError::Unavailable)?; },
                            Event::Close(port)=> { let writer=writers.get_mut(&port).ok_or(PortError::Invalid)?;writer.close(writer.key()).await.map_err(|_|PortError::Unavailable)?; },
                            Event::Blob(port,blob)=> { let source=input.sources.get(&port).ok_or(PortError::Invalid)?;source.state.lock().await.blob=Some(blob);source.ready.notify_waiters(); },
                            Event::Finish=> { *input.input_done.lock().map_err(|_|PortError::Uncertain)?=true;return Ok(()); },
                        }
                    }
                }
            }));
            for (port, mut reader) in consumers {
                let output = pending.clone();
                jobs.push(tokio::spawn(async move {
                    let stream_id = uuid::Uuid::new_v4().to_string();
                    let format = match reader.format() {
                        Format::Text => p::stream_open::Format::Text(p::TextFormat {}),
                        Format::Pcm16Le { rate, channels } => {
                            p::stream_open::Format::Pcm16le(p::Pcm16Le {
                                sample_rate: rate,
                                channels: u32::from(channels),
                            })
                        }
                    };
                    output
                        .emit(
                            p::output_event::Event::Open(p::StreamOpen {
                                port,
                                stream_id: stream_id.clone(),
                                format: Some(format),
                            }),
                            None,
                        )
                        .await?;
                    let mut sequence = 0;
                    while let Some(packet) = reader
                        .read_rpc_output()
                        .await
                        .map_err(|_| PortError::Unavailable)?
                    {
                        let credit = output
                            .credits(packet.chunk().bytes() * 2 + 2048)
                            .await
                            .map_err(|_| PortError::Unavailable)?;
                        let payload = match packet.chunk() {
                            Chunk::Audio(bytes) => p::stream_chunk::Payload::Audio(bytes.clone()),
                            Chunk::Text(text) => p::stream_chunk::Payload::Text(p::TextRevision {
                                segment_id: text.segment.clone(),
                                mutation: if text.replace { 2 } else { 1 },
                                text: text.text.clone(),
                                committed: text.committed,
                            }),
                        };
                        output
                            .emit_credited(
                                p::output_event::Event::Chunk(p::StreamChunk {
                                    stream_id: stream_id.clone(),
                                    sequence,
                                    payload: Some(payload),
                                }),
                                Some(packet),
                                credit,
                            )
                            .await?;
                        sequence += 1;
                    }
                    output
                        .emit(
                            p::output_event::Event::Close(p::StreamClose {
                                stream_id,
                                next_sequence: sequence,
                            }),
                            None,
                        )
                        .await
                }));
            }
            Ok(())
        })
    }
    fn cancel(&self) {
        self.0.streams.cancellation().cancel();
    }
    fn finish(&self) -> PortFuture<()> {
        let pending = self.0.clone();
        let join = self.join();
        Box::pin(async move {
            if pending
                .native_required
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::select! { ()=pending.native_eof.cancelled()=>{},()=pending.streams.cancellation().cancelled_owned()=>return Err(PortError::Unavailable) }
            }
            join.await?;
            if *pending
                .input_done
                .lock()
                .map_err(|_| PortError::Uncertain)?
                && pending
                    .joined
                    .lock()
                    .map_err(|_| PortError::Uncertain)?
                    .is_some_and(|done| done)
            {
                Ok(())
            } else {
                Err(PortError::Uncertain)
            }
        })
    }
    fn join(&self) -> PortFuture<()> {
        let pending = self.0.clone();
        Box::pin(async move {
            let mut jobs = pending.jobs.lock().await;
            let mut success = pending
                .joined
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .unwrap_or(true);
            while !jobs.is_empty() {
                match (&mut jobs[0]).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => success = false,
                    Err(_) => {
                        success = false;
                        *pending
                            .cleanup_failed
                            .lock()
                            .map_err(|_| PortError::Uncertain)? = true;
                    }
                }
                *pending.joined.lock().map_err(|_| PortError::Uncertain)? = Some(success);
                drop(jobs.remove(0));
            }
            *pending.joined.lock().map_err(|_| PortError::Uncertain)? = Some(success);
            pending
                .owner
                .lock()
                .map_err(|_| PortError::Uncertain)?
                .take();
            if *pending
                .cleanup_failed
                .lock()
                .map_err(|_| PortError::Uncertain)?
            {
                Err(PortError::Uncertain)
            } else {
                Ok(())
            }
        })
    }
}
impl PendingTransport {
    pub(super) async fn emit(
        &self,
        event: p::output_event::Event,
        packet: Option<stream::Packet>,
    ) -> Result<(), PortError> {
        let event = p::OutputEvent { event: Some(event) };
        let credit = self
            .credits(event.encoded_len() * 2 + 256)
            .await
            .map_err(|_| PortError::Unavailable)?;
        self.emit_credited(event.event.ok_or(PortError::Invalid)?, packet, credit)
            .await
    }
    async fn emit_credited(
        &self,
        event: p::output_event::Event,
        packet: Option<stream::Packet>,
        credit: Vec<OwnedSemaphorePermit>,
    ) -> Result<(), PortError> {
        let event = p::OutputEvent { event: Some(event) };
        let (consumed, ack) = tokio::sync::oneshot::channel();
        let frame = OutputFrame {
            event,
            consumed: Some(consumed),
            _packet: packet,
            _credit: credit,
        };
        tokio::select! { ()=self.streams.cancellation().cancelled_owned()=>return Err(PortError::Unavailable),result=tokio::time::timeout(Duration::from_secs(30),self.output.send(frame))=>result.map_err(|_|PortError::Unavailable)?.map_err(|_|PortError::Unavailable)? }
        tokio::select! { ()=self.streams.cancellation().cancelled_owned()=>Err(PortError::Unavailable),result=tokio::time::timeout(Duration::from_secs(30),ack)=>result.map_err(|_|PortError::Unavailable)?.map_err(|_|PortError::Unavailable) }
    }
}
pub(super) fn binary_metadata(
    value: p::BinaryInputDeclaration,
) -> Result<blob::UploadMetadata, Status> {
    if value.display_name.is_empty()
        || value.display_name.len() > 255
        || value
            .display_name
            .chars()
            .any(|c| c.is_control() || "/\\:".contains(c))
        || value.display_name == "."
        || value.display_name == ".."
        || value.content_type.is_empty()
        || value.content_type.len() > 128
        || !value.content_type.is_ascii()
        || !value.content_type.contains('/')
        || value
            .content_type
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(Status::invalid_argument("binary metadata"));
    }
    let kind = match value.purpose {
        Some(p::binary_input_declaration::Purpose::File(_)) => blob::BlobKind::File,
        Some(p::binary_input_declaration::Purpose::Image(1)) => {
            blob::BlobKind::Image(blob::ImageFormat::Png)
        }
        Some(p::binary_input_declaration::Purpose::Image(2)) => {
            blob::BlobKind::Image(blob::ImageFormat::Jpeg)
        }
        Some(p::binary_input_declaration::Purpose::Image(3)) => {
            blob::BlobKind::Image(blob::ImageFormat::WebP)
        }
        _ => return Err(Status::invalid_argument("binary purpose")),
    };
    if matches!(kind, blob::BlobKind::Image(blob::ImageFormat::Png))
        && value.content_type != "image/png"
        || matches!(kind, blob::BlobKind::Image(blob::ImageFormat::Jpeg))
            && value.content_type != "image/jpeg"
        || matches!(kind, blob::BlobKind::Image(blob::ImageFormat::WebP))
            && value.content_type != "image/webp"
    {
        return Err(Status::invalid_argument("image MIME mismatch"));
    }
    let Some(bytes) = value.declared_bytes else {
        return Err(Status::invalid_argument("finite binary size required"));
    };
    if bytes
        > if matches!(kind, blob::BlobKind::Image(_)) {
            20971520
        } else {
            67108864
        }
    {
        return Err(Status::invalid_argument("binary size"));
    }
    let expected_sha256 = value
        .expected_sha256
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| Status::invalid_argument("binary digest"))
        })
        .transpose()?;
    Ok(blob::UploadMetadata {
        display_name: value.display_name,
        content_type: value.content_type,
        kind,
        declared_bytes: Some(bytes),
        expected_sha256,
    })
}
