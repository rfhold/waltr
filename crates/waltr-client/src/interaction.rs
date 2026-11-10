//! Caller-driven v2 media ownership. Poll output concurrently with input.
#![allow(missing_docs)]

use crate::transport::routine::{identity, invalid, validate_limits};
use crate::{ClientError, ClientManualRoutineDefinition, RoutineDirection, RoutineMedia};
use futures_core::Stream;
use futures_util::{StreamExt, task::AtomicWaker};
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
pub use waltr::grpc::waltr::routine::v2 as wire;
use wire::{input_declaration, input_event, output_event, stream_chunk, stream_open};
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod browser;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod native_tests;
pub(crate) mod split;
pub(crate) mod transfer;
pub use split::{AttachmentControl, AttachmentInput, AttachmentSession};
pub use transfer::{BlobReader, BlobUpload, DurableTextReader, MediaTransfers};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type ProtocolStream<T> = Pin<Box<dyn Stream<Item = Result<T, ClientError>> + Send>>;
#[cfg(target_arch = "wasm32")]
pub(crate) type ProtocolStream<T> = Pin<Box<dyn Stream<Item = Result<T, ClientError>>>>;

#[derive(Clone, Debug)]
pub struct InteractionLimits {
    pub input_items: usize,
    pub input_bytes: usize,
    pub chunk_bytes: usize,
    pub output_message_bytes: usize,
    pub text_bytes: usize,
    pub text_segments: usize,
    pub streams: usize,
    pub definition: ClientManualRoutineDefinition,
    pub server: wire::ServerLimits,
}
impl InteractionLimits {
    pub fn for_definition(
        definition: ClientManualRoutineDefinition,
        server: wire::ServerLimits,
    ) -> Result<Self, ClientError> {
        validate_limits(&server)?;
        Ok(Self {
            input_items: server.edge_chunks.min(server.input_batch_events) as usize,
            input_bytes: server.edge_bytes.min(server.input_batch_bytes) as usize,
            chunk_bytes: server.chunk_bytes as usize,
            output_message_bytes: server.input_batch_bytes.max(131072) as usize,
            text_bytes: server.retained_text_bytes as usize,
            text_segments: server.text_segments as usize,
            streams: server.streams_per_direction as usize,
            definition,
            server,
        })
    }
    pub(crate) fn validate(&self) -> Result<(), ClientError> {
        validate_limits(&self.server)?;
        if [
            self.input_items,
            self.input_bytes,
            self.chunk_bytes,
            self.output_message_bytes,
            self.text_bytes,
            self.text_segments,
            self.streams,
        ]
        .contains(&0)
            || self.chunk_bytes > 65536
            || self.streams > 32
        {
            return Err(invalid());
        }
        Ok(())
    }
    /// Validates the complete declaration before opening devices or starting an RPC.
    ///
    /// # Errors
    /// Returns an error when the selection, parameters, or inputs violate these limits.
    pub fn validate_open(&self, open: &wire::InteractionOpen) -> Result<(), ClientError> {
        self.validate()?;
        let s = open.selection.as_ref().ok_or_else(invalid)?;
        if s.routine != self.definition.routine_name
            || s.revision != self.definition.routine_revision
            || !self
                .definition
                .permits(&s.entrypoint, true, s.source_session.as_deref())
            || open.inputs.len() > 32
            || open.encoded_len() > self.server.input_batch_bytes as usize
        {
            return Err(invalid());
        }
        identity(&s.correlation, 128)?;
        let params = crate::transport::routine::json(
            s.parameters.clone().ok_or_else(invalid)?,
            self.server.structured_value_bytes as usize,
        )?;
        self.definition.parameters.validate(&params)?;
        let mut names = BTreeSet::new();
        for input in &open.inputs {
            identity(&input.port, 128)?;
            if !names.insert(input.port.as_str()) {
                return Err(invalid());
            }
            let port = self
                .definition
                .ports
                .iter()
                .find(|p| p.name == input.port && p.direction == RoutineDirection::Input)
                .ok_or_else(invalid)?;
            let format = match &input.format {
                Some(input_declaration::Format::Pcm16le(p))
                    if port.media == RoutineMedia::Audio =>
                {
                    pcm(p)?;
                    "pcm16le-24000-mono"
                }
                Some(input_declaration::Format::Text(_)) if port.media == RoutineMedia::Text => {
                    "utf8"
                }
                Some(input_declaration::Format::Binary(b)) => {
                    binary_declaration(b, port.media, &self.server)?;
                    if b.declared_bytes.unwrap() > port.max_bytes.0 {
                        return Err(invalid());
                    }
                    match &b.purpose {
                        Some(wire::binary_input_declaration::Purpose::File(_)) => {
                            b.content_type.as_str()
                        }
                        Some(wire::binary_input_declaration::Purpose::Image(i)) => image_mime(*i)?,
                        _ => return Err(invalid()),
                    }
                }
                _ => return Err(invalid()),
            };
            if !port.formats.iter().any(|f| f == format) {
                return Err(invalid());
            }
        }
        if self.definition.ports.iter().any(|p| {
            p.required && p.direction == RoutineDirection::Input && !names.contains(p.name.as_str())
        }) {
            return Err(invalid());
        }
        Ok(())
    }
}
pub(crate) fn image_mime(i: i32) -> Result<&'static str, ClientError> {
    match wire::ImageFormat::try_from(i) {
        Ok(wire::ImageFormat::Png) => Ok("image/png"),
        Ok(wire::ImageFormat::Jpeg) => Ok("image/jpeg"),
        Ok(wire::ImageFormat::Webp) => Ok("image/webp"),
        _ => Err(invalid()),
    }
}
fn binary_declaration(
    b: &wire::BinaryInputDeclaration,
    media: RoutineMedia,
    l: &wire::ServerLimits,
) -> Result<(), ClientError> {
    identity(&b.display_name, 255)?;
    identity(&b.content_type, 128)?;
    if b.display_name
        .chars()
        .any(|c| c.is_control() || c == '/' || c == '\\')
        || b.expected_sha256.as_ref().is_some_and(|d| d.len() != 32)
    {
        return Err(invalid());
    }
    let bytes = b.declared_bytes.ok_or_else(invalid)?;
    match &b.purpose {
        Some(wire::binary_input_declaration::Purpose::File(_))
            if media == RoutineMedia::File && bytes <= l.file_bytes =>
        {
            Ok(())
        }
        Some(wire::binary_input_declaration::Purpose::Image(i))
            if media == RoutineMedia::Image
                && bytes <= l.image_bytes
                && image_mime(*i)? == b.content_type =>
        {
            Ok(())
        }
        _ => Err(invalid()),
    }
}
fn pcm(p: &wire::Pcm16Le) -> Result<(), ClientError> {
    if p.sample_rate == 24000 && p.channels == 1 {
        Ok(())
    } else {
        Err(invalid())
    }
}

#[derive(Clone, Debug, Default)]
pub struct InteractionText {
    segments: Vec<(String, String, bool)>,
    bytes: usize,
}
impl InteractionText {
    pub fn update(
        &mut self,
        u: &wire::TextRevision,
        l: &InteractionLimits,
    ) -> Result<(), ClientError> {
        identity(&u.segment_id, 128)?;
        if !matches!(u.mutation, 1 | 2) || u.segment_id.len() + u.text.len() > l.chunk_bytes {
            return Err(invalid());
        }
        let index = self.segments.iter().position(|s| s.0 == u.segment_id);
        let old = index.map(|i| &self.segments[i]);
        if old.is_some_and(|s| s.2) {
            return Err(invalid());
        }
        let old_bytes = old.map_or(0, |s| s.1.len());
        let new_bytes = if u.mutation == 1 {
            old_bytes.checked_add(u.text.len()).ok_or_else(invalid)?
        } else {
            u.text.len()
        };
        let total = self.bytes - old_bytes + new_bytes;
        let ids: usize = self.segments.iter().map(|s| s.0.len()).sum();
        if total
            .saturating_add(ids)
            .saturating_add(if index.is_none() {
                u.segment_id.len()
            } else {
                0
            })
            > l.text_bytes
            || (index.is_none() && self.segments.len() >= l.text_segments)
        {
            return Err(invalid());
        }
        let i = index.unwrap_or_else(|| {
            self.segments
                .push((u.segment_id.clone(), String::new(), false));
            self.segments.len() - 1
        });
        if u.mutation == 2 {
            self.segments[i].1 = u.text.clone();
        } else {
            self.segments[i].1.push_str(&u.text);
        }
        self.segments[i].2 = u.committed;
        self.bytes = total;
        Ok(())
    }
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.1.as_str()).collect()
    }
    pub fn committed_prefix(&self) -> String {
        self.segments
            .iter()
            .take_while(|s| s.2)
            .map(|s| s.1.as_str())
            .collect()
    }
    pub fn is_committed(&self) -> bool {
        self.segments.iter().all(|s| s.2)
    }
}
#[derive(Clone, Debug)]
struct PortStream {
    port: String,
    format: stream_open::Format,
    text: InteractionText,
    next: u64,
    bytes: u64,
    closed: bool,
}
#[derive(Clone, Debug, Default)]
struct Streams {
    entries: BTreeMap<String, PortStream>,
    total: u64,
    port_bytes: BTreeMap<String, u64>,
}
impl Streams {
    fn open(
        &mut self,
        o: &wire::StreamOpen,
        l: &InteractionLimits,
        input: bool,
        declarations: &[wire::InputDeclaration],
    ) -> Result<(), ClientError> {
        identity(&o.stream_id, 128)?;
        identity(&o.port, 128)?;
        if self.entries.contains_key(&o.stream_id) || self.entries.len() >= l.streams {
            return Err(invalid());
        }
        let format = o.format.ok_or_else(invalid)?;
        if let stream_open::Format::Pcm16le(p) = &format {
            pcm(p)?;
        }
        let p = l
            .definition
            .ports
            .iter()
            .find(|p| {
                p.name == o.port
                    && p.direction
                        == if input {
                            RoutineDirection::Input
                        } else {
                            RoutineDirection::Output
                        }
            })
            .ok_or_else(invalid)?;
        let f = match &format {
            stream_open::Format::Pcm16le(_) => "pcm16le-24000-mono",
            stream_open::Format::Text(_) => "utf8",
        };
        if !p.formats.iter().any(|s| s == f)
            || !matches!(
                (&format, p.media),
                (stream_open::Format::Pcm16le(_), RoutineMedia::Audio)
                    | (stream_open::Format::Text(_), RoutineMedia::Text)
            )
        {
            return Err(invalid());
        }
        if input {
            let declaration = declarations
                .iter()
                .find(|d| d.port == o.port)
                .ok_or_else(invalid)?;
            let matches = match (&declaration.format, &format) {
                (Some(input_declaration::Format::Pcm16le(a)), stream_open::Format::Pcm16le(b)) => {
                    a == b
                }
                (Some(input_declaration::Format::Text(_)), stream_open::Format::Text(_)) => true,
                _ => false,
            };
            if !matches {
                return Err(invalid());
            }
        }
        self.entries.insert(
            o.stream_id.clone(),
            PortStream {
                port: o.port.clone(),
                format,
                text: InteractionText::default(),
                next: 0,
                bytes: 0,
                closed: false,
            },
        );
        Ok(())
    }
    fn chunk(
        &mut self,
        c: &wire::StreamChunk,
        l: &InteractionLimits,
        input: bool,
    ) -> Result<(), ClientError> {
        let mut next = self.entries.get(&c.stream_id).ok_or_else(invalid)?.clone();
        let s = &mut next;
        if s.closed || c.sequence != s.next {
            return Err(invalid());
        }
        let bytes = match (&s.format, &c.payload) {
            (stream_open::Format::Pcm16le(p), Some(stream_chunk::Payload::Audio(a)))
                if !a.is_empty()
                    && a.len() <= l.chunk_bytes
                    && a.len() % (p.channels as usize * 2) == 0 =>
            {
                let duration_bytes = u64::from(p.sample_rate)
                    * u64::from(p.channels)
                    * 2
                    * l.server.input_duration_ms
                    / 1000;
                if input && s.bytes.saturating_add(a.len() as u64) > duration_bytes {
                    return Err(invalid());
                }
                a.len() as u64
            }
            (stream_open::Format::Text(_), Some(stream_chunk::Payload::Text(t))) => {
                s.text.update(t, l)?;
                t.text.len() as u64
            }
            _ => return Err(invalid()),
        };
        let p = l
            .definition
            .ports
            .iter()
            .find(|p| {
                p.name == s.port
                    && p.direction
                        == if input {
                            RoutineDirection::Input
                        } else {
                            RoutineDirection::Output
                        }
            })
            .ok_or_else(invalid)?;
        let port_bytes = self
            .port_bytes
            .get(&s.port)
            .copied()
            .unwrap_or(0)
            .checked_add(bytes)
            .ok_or_else(invalid)?;
        let total = self.total.checked_add(bytes).ok_or_else(invalid)?;
        if port_bytes > p.max_bytes.0
            || total
                > if input {
                    l.server.input_bytes
                } else {
                    l.server.output_bytes
                }
        {
            return Err(invalid());
        }
        s.bytes = s.bytes.checked_add(bytes).ok_or_else(invalid)?;
        s.next = s.next.checked_add(1).ok_or_else(invalid)?;
        let retained: usize = self
            .entries
            .iter()
            .filter(|(id, _)| *id != &c.stream_id)
            .map(|(_, s)| s.text.bytes + s.text.segments.iter().map(|s| s.0.len()).sum::<usize>())
            .sum::<usize>()
            + next.text.bytes
            + next.text.segments.iter().map(|s| s.0.len()).sum::<usize>();
        let segments: usize = self
            .entries
            .iter()
            .filter(|(id, _)| *id != &c.stream_id)
            .map(|(_, s)| s.text.segments.len())
            .sum::<usize>()
            + next.text.segments.len();
        if retained > l.text_bytes || segments > l.text_segments {
            return Err(invalid());
        }
        self.port_bytes.insert(next.port.clone(), port_bytes);
        self.total = total;
        self.entries.insert(c.stream_id.clone(), next);
        Ok(())
    }
    fn close(&mut self, c: &wire::StreamClose) -> Result<(), ClientError> {
        let s = self.entries.get_mut(&c.stream_id).ok_or_else(invalid)?;
        if s.closed || c.next_sequence != s.next || !s.text.is_committed() {
            return Err(invalid());
        }
        s.closed = true;
        Ok(())
    }
    fn all_closed(&self) -> bool {
        self.entries.values().all(|s| s.closed)
    }
}
#[derive(Clone, Debug)]
pub(crate) struct InputProtocol {
    streams: Streams,
    declarations: Vec<wire::InputDeclaration>,
    blobs: BTreeSet<String>,
    finished: bool,
}
impl InputProtocol {
    pub(crate) fn new(open: &wire::InteractionOpen) -> Self {
        Self {
            streams: Streams::default(),
            declarations: open.inputs.clone(),
            blobs: BTreeSet::new(),
            finished: false,
        }
    }
    pub(crate) fn apply(
        &mut self,
        e: &wire::InputEvent,
        l: &InteractionLimits,
    ) -> Result<(), ClientError> {
        if self.finished {
            return Err(invalid());
        }
        match e.event.as_ref().ok_or_else(invalid)? {
            input_event::Event::Open(o) => self.streams.open(o, l, true, &self.declarations),
            input_event::Event::Chunk(c) => self.streams.chunk(c, l, true),
            input_event::Event::Close(c) => self.streams.close(c),
            input_event::Event::Blob(b) => {
                identity(&b.blob_id, 256)?;
                if !self.declarations.iter().any(|d| {
                    d.port == b.port
                        && matches!(d.format, Some(input_declaration::Format::Binary(_)))
                }) || !self.blobs.insert(b.port.clone())
                {
                    return Err(invalid());
                }
                Ok(())
            }
            input_event::Event::Finish(_) => {
                if !self.streams.all_closed()
                    || self.declarations.iter().any(|d| match d.format {
                        Some(input_declaration::Format::Binary(_)) => !self.blobs.contains(&d.port),
                        _ => !self
                            .streams
                            .entries
                            .values()
                            .any(|s| s.port == d.port && s.closed),
                    })
                {
                    return Err(invalid());
                }
                self.finished = true;
                Ok(())
            }
            input_event::Event::Cancel(_) => Err(invalid()),
        }?;
        let binary = self
            .declarations
            .iter()
            .filter(|d| self.blobs.contains(&d.port))
            .try_fold(0u64, |total, d| match &d.format {
                Some(input_declaration::Format::Binary(b)) => total
                    .checked_add(b.declared_bytes.ok_or_else(invalid)?)
                    .ok_or_else(invalid),
                _ => Err(invalid()),
            })?;
        if self.streams.total.checked_add(binary).ok_or_else(invalid)? > l.server.input_bytes {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct QueueState {
    split: bool,
    items: VecDeque<(wire::InteractRequest, usize)>,
    bytes: usize,
    cancelled: bool,
    #[cfg(not(target_arch = "wasm32"))]
    cancel_sent: bool,
    #[cfg(not(target_arch = "wasm32"))]
    native_closed: bool,
    accepted: bool,
    input_finished: bool,
    ended: bool,
    accepted_limits: Option<wire::ServerLimits>,
    invocation_id: Option<String>,
}
#[derive(Debug)]
pub(crate) struct InteractionQueue {
    state: Mutex<QueueState>,
    reader: AtomicWaker,
    writer: AtomicWaker,
    output: AtomicWaker,
    upload_wakers: Mutex<Vec<std::sync::Weak<AtomicWaker>>>,
}
impl InteractionQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState {
                split: false,
                items: VecDeque::new(),
                bytes: 0,
                cancelled: false,
                #[cfg(not(target_arch = "wasm32"))]
                cancel_sent: false,
                #[cfg(not(target_arch = "wasm32"))]
                native_closed: false,
                accepted: false,
                input_finished: false,
                ended: false,
                accepted_limits: None,
                invocation_id: None,
            }),
            reader: AtomicWaker::new(),
            writer: AtomicWaker::new(),
            output: AtomicWaker::new(),
            upload_wakers: Mutex::new(vec![]),
        })
    }
    pub(crate) fn cancel(&self) {
        let mut s = self.state.lock().unwrap();
        s.cancelled = true;
        s.items.clear();
        s.bytes = 0;
        drop(s);
        self.reader.wake();
        self.writer.wake();
        self.output.wake();
        self.wake_uploads();
    }
    fn permits_upload(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.accepted && !s.cancelled && !s.ended && !s.input_finished
    }
    async fn upload_wait<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        let waker = Arc::new(AtomicWaker::new());
        {
            let mut waiters = self.upload_wakers.lock().unwrap();
            waiters.retain(|w| w.strong_count() > 0);
            if waiters.len() >= 32 {
                return Err(invalid());
            }
            waiters.push(Arc::downgrade(&waker));
        }
        futures_util::pin_mut!(future);
        futures_util::future::poll_fn(|cx| {
            waker.register(cx.waker());
            if !self.permits_upload() {
                return Poll::Ready(Err(invalid()));
            }
            future.as_mut().poll(cx)
        })
        .await
    }
    fn wake_uploads(&self) {
        let waiters = self
            .upload_wakers
            .lock()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect::<Vec<_>>();
        for w in waiters {
            w.wake();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn seed(&self, r: wire::InteractRequest) {
        let bytes = r.encoded_len();
        let mut s = self.state.lock().unwrap();
        s.bytes += bytes;
        s.items.push_back((r, bytes));
    }
    fn finish_input(&self) {
        self.state.lock().unwrap().input_finished = true;
        self.reader.wake();
        self.wake_uploads();
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn take_native_cancel(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        if !state.cancelled || !state.native_closed || state.cancel_sent || state.ended {
            return None;
        }
        let invocation = state.invocation_id.clone()?;
        state.cancel_sent = true;
        Some(invocation)
    }
    fn end(&self) {
        let mut s = self.state.lock().unwrap();
        s.ended = true;
        s.items.clear();
        s.bytes = 0;
        drop(s);
        self.reader.wake();
        self.writer.wake();
        self.wake_uploads();
    }
}
pub(crate) struct PendingMediaWait {
    queue: Arc<InteractionQueue>,
    armed: bool,
}
impl PendingMediaWait {
    pub(crate) fn new(queue: Arc<InteractionQueue>) -> Self {
        Self { queue, armed: true }
    }
    pub(crate) fn complete(&mut self) {
        self.armed = false;
    }
}
impl Drop for PendingMediaWait {
    fn drop(&mut self) {
        let active = {
            let s = self.queue.state.lock().unwrap();
            !s.ended && !s.input_finished
        };
        if self.armed && active {
            self.queue.cancel();
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct InteractionRequests(pub Arc<InteractionQueue>);
#[cfg(not(target_arch = "wasm32"))]
impl Stream for InteractionRequests {
    type Item = wire::InteractRequest;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.reader.register(cx.waker());
        let mut s = self.0.state.lock().unwrap();
        if s.ended {
            return Poll::Ready(None);
        }
        if s.cancelled {
            if s.cancel_sent {
                return Poll::Ready(None);
            }
            s.cancel_sent = true;
            return Poll::Ready(Some(wire::InteractRequest {
                frame: Some(wire::interact_request::Frame::Input(wire::InputEvent {
                    event: Some(input_event::Event::Cancel(wire::InteractionCancel {})),
                })),
            }));
        }
        if let Some((r, bytes)) = s.items.pop_front() {
            s.bytes -= bytes;
            drop(s);
            self.0.writer.wake();
            Poll::Ready(Some(r))
        } else if s.input_finished {
            s.native_closed = true;
            drop(s);
            self.0.output.wake();
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}
#[derive(Debug)]
pub struct InteractionInputHandle {
    queue: Arc<InteractionQueue>,
    limits: InteractionLimits,
    protocol: InputProtocol,
}
impl InteractionInputHandle {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn new(
        queue: Arc<InteractionQueue>,
        limits: InteractionLimits,
        open: &wire::InteractionOpen,
    ) -> Self {
        Self {
            queue,
            limits,
            protocol: InputProtocol::new(open),
        }
    }
    pub fn cancel(&self) {
        self.queue.cancel();
    }
    pub fn control(&self) -> InteractionControl {
        InteractionControl(self.queue.clone())
    }
    /// Return the validated negotiated limits after acceptance, while input remains active.
    pub fn accepted_limits(&self) -> Result<InteractionLimits, ClientError> {
        let state = self.queue.state.lock().unwrap();
        if !state.accepted || state.cancelled || state.ended {
            return Err(invalid());
        }
        InteractionLimits::for_definition(
            self.limits.definition.clone(),
            state.accepted_limits.ok_or_else(invalid)?,
        )
    }
    pub async fn send(&mut self, event: input_event::Event) -> Result<(), ClientError> {
        self.limits = self.accepted_limits()?;
        let e = wire::InputEvent { event: Some(event) };
        let mut protocol = self.protocol.clone();
        protocol.apply(&e, &self.limits)?;
        self.enqueue(wire::InteractRequest {
            frame: Some(wire::interact_request::Frame::Input(e)),
        })
        .await?;
        self.protocol = protocol;
        if self.protocol.finished {
            self.queue.finish_input();
        }
        Ok(())
    }
    pub async fn finish_input(&mut self) -> Result<(), ClientError> {
        self.send(input_event::Event::Finish(wire::InputFinish {}))
            .await
    }
    /// Bind a quarantine upload to this accepted finite declaration.
    pub fn begin_upload(
        &self,
        transfers: &MediaTransfers,
        port: &str,
    ) -> Result<BlobUpload, ClientError> {
        let state = self.queue.state.lock().unwrap();
        if !state.accepted
            || state.cancelled
            || state.ended
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
        let Some(wire::input_declaration::Format::Binary(binary)) = &declaration.format else {
            return Err(invalid());
        };
        BlobUpload::new(
            transfers.rpc.clone(),
            state.invocation_id.clone().ok_or_else(invalid)?,
            port.into(),
            binary.clone(),
            self.queue.clone(),
        )
    }
    async fn enqueue(&self, r: wire::InteractRequest) -> Result<(), ClientError> {
        let bytes = r.encoded_len();
        if bytes > self.limits.input_bytes || bytes > self.limits.output_message_bytes {
            return Err(invalid());
        }
        let mut r = Some(r);
        futures_util::future::poll_fn(|cx| {
            self.queue.writer.register(cx.waker());
            let mut s = self.queue.state.lock().unwrap();
            if s.cancelled || s.ended || !s.accepted {
                return Poll::Ready(Err(invalid()));
            }
            if s.items.len() >= self.limits.input_items || s.bytes + bytes > self.limits.input_bytes
            {
                return Poll::Pending;
            }
            s.items.push_back((r.take().unwrap(), bytes));
            s.bytes += bytes;
            drop(s);
            self.queue.reader.wake();
            Poll::Ready(Ok(()))
        })
        .await
    }
}
impl Drop for InteractionInputHandle {
    fn drop(&mut self) {
        if !self.queue.state.lock().unwrap().ended {
            self.cancel();
        }
    }
}
#[derive(Clone, Debug)]
pub struct InteractionControl(Arc<InteractionQueue>);
impl InteractionControl {
    pub fn cancel(&self) {
        self.0.cancel();
    }
}
#[derive(Debug)]
pub struct InteractionSession {
    pub input: InteractionInputHandle,
    pub output: InteractionOutput,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractionOutputFailure {
    RemoteFailed,
    RemoteInterrupted,
    Rejected,
    Protocol,
}

pub struct BinaryOutputGrant(pub(crate) wire::BinaryOutputCredentials);
impl std::fmt::Debug for BinaryOutputGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BinaryOutputGrant([redacted])")
    }
}
#[derive(Debug)]
pub enum InteractionEvent {
    Accepted {
        invocation_id: String,
    },
    Rejected {
        code: i32,
    },
    Open(wire::StreamOpen),
    Chunk(wire::StreamChunk),
    Close(wire::StreamClose),
    Blob {
        port: String,
        descriptor: crate::RoutineBlobDescriptor,
    },
    Terminal {
        status: crate::ClientRoutineInvocationStatus,
        cleanup_complete: bool,
    },
}
pub struct InteractionOutput {
    source: ProtocolStream<wire::OutputEvent>,
    queue: Arc<InteractionQueue>,
    limits: InteractionLimits,
    streams: Streams,
    accepted: Option<String>,
    terminal: Option<wire::InteractionTerminal>,
    rejected: bool,
    done: bool,
    clean: bool,
    grants: BTreeMap<String, BinaryOutputGrant>,
    blob_ports: BTreeSet<String>,
    blob_bytes: u64,
}
impl std::fmt::Debug for InteractionOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionOutput")
            .field("done", &self.done)
            .field("clean", &self.clean)
            .finish_non_exhaustive()
    }
}
impl InteractionOutput {
    pub(crate) fn new(
        source: ProtocolStream<wire::OutputEvent>,
        queue: Arc<InteractionQueue>,
        limits: InteractionLimits,
    ) -> Self {
        Self {
            source,
            queue,
            limits,
            streams: Streams::default(),
            accepted: None,
            terminal: None,
            rejected: false,
            done: false,
            clean: false,
            grants: BTreeMap::new(),
            blob_ports: BTreeSet::new(),
            blob_bytes: 0,
        }
    }
    pub fn cancel(&self) {
        self.queue.cancel();
    }
    pub fn control(&self) -> InteractionControl {
        InteractionControl(self.queue.clone())
    }
    pub fn terminal(&self) -> Option<&wire::InteractionTerminal> {
        self.terminal.as_ref()
    }
    pub fn take_blob_grant(&mut self, port: &str) -> Option<BinaryOutputGrant> {
        self.grants.remove(port)
    }
    pub fn output_failure(&self) -> InteractionOutputFailure {
        if self.rejected {
            return InteractionOutputFailure::Rejected;
        }
        match self.terminal.as_ref().map(|t| t.status) {
            Some(4 | 6) => InteractionOutputFailure::RemoteFailed,
            Some(5) => InteractionOutputFailure::RemoteInterrupted,
            _ => InteractionOutputFailure::Protocol,
        }
    }
    pub fn succeeded(&self) -> bool {
        self.clean
            && !self.queue.state.lock().unwrap().cancelled
            && self
                .terminal
                .as_ref()
                .is_some_and(|t| t.status == 3 && t.cleanup_complete)
    }
    fn selected(&self, port: &str) -> Result<Option<&PortStream>, ClientError> {
        if self.queue.state.lock().unwrap().cancelled
            || self.rejected
            || self.terminal.as_ref().is_some_and(|t| t.status != 3)
        {
            return Err(invalid());
        }
        let mut s = self.streams.entries.values().filter(|s| s.port == port);
        let Some(first) = s.next() else {
            return Ok(None);
        };
        if s.next().is_some() || !matches!(first.format, stream_open::Format::Text(_)) {
            return Err(invalid());
        }
        Ok(Some(first))
    }
    pub fn preview_text(&self, port: &str) -> Option<String> {
        self.selected(port).ok().flatten().map(|s| s.text.text())
    }
    pub fn live_text(&self, port: &str) -> Result<Option<String>, ClientError> {
        Ok(self.selected(port)?.map(|s| s.text.text()))
    }
    pub fn committed_prefix(&self, port: &str) -> Result<Option<String>, ClientError> {
        Ok(self.selected(port)?.map(|s| s.text.committed_prefix()))
    }
    pub fn final_text(&self, port: &str) -> Result<String, ClientError> {
        if !self.succeeded() {
            return Err(invalid());
        }
        let s = self
            .selected(port)?
            .filter(|s| s.closed && s.text.is_committed())
            .ok_or_else(invalid)?;
        Ok(s.text.text())
    }
    /// This proof is available only after a validated terminal followed by clean EOF.
    pub fn cleanup_verified(&self) -> bool {
        self.clean && self.terminal.as_ref().is_some_and(|t| t.cleanup_complete)
    }
    pub async fn cancel_and_join(&mut self) -> Result<bool, ClientError> {
        self.cancel();
        while let Some(r) = self.next().await {
            r?;
        }
        Ok(self.cleanup_verified())
    }
    fn validate(&mut self, o: wire::OutputEvent) -> Result<InteractionEvent, ClientError> {
        if o.encoded_len() > self.limits.output_message_bytes
            || self.terminal.is_some()
            || self.rejected
        {
            return Err(invalid());
        }
        let event = o.event.ok_or_else(invalid)?;
        if self.accepted.is_none() {
            return match event {
                output_event::Event::Accepted(a) => {
                    identity(&a.invocation_id, 256)?;
                    if a.revision != self.limits.definition.routine_revision {
                        return Err(invalid());
                    }
                    let server = a.limits.ok_or_else(invalid)?;
                    validate_limits(&server)?;
                    if !crate::RoutineLimits::from(server).is_within(&self.limits.server.into()) {
                        return Err(invalid());
                    }
                    self.limits =
                        InteractionLimits::for_definition(self.limits.definition.clone(), server)?;
                    self.accepted = Some(a.invocation_id.clone());
                    {
                        let mut state = self.queue.state.lock().unwrap();
                        state.accepted = true;
                        state.accepted_limits = Some(server);
                        state.invocation_id = Some(a.invocation_id.clone());
                    }
                    self.queue.writer.wake();
                    Ok(InteractionEvent::Accepted {
                        invocation_id: a.invocation_id,
                    })
                }
                output_event::Event::Rejected(r) if matches!(r.code, 1..=6) => {
                    self.rejected = true;
                    self.queue.end();
                    Ok(InteractionEvent::Rejected { code: r.code })
                }
                _ => Err(invalid()),
            };
        }
        match event {
            output_event::Event::Open(o) => {
                self.streams.open(&o, &self.limits, false, &[])?;
                Ok(InteractionEvent::Open(o))
            }
            output_event::Event::Chunk(c) => {
                self.streams.chunk(&c, &self.limits, false)?;
                if self
                    .streams
                    .total
                    .checked_add(self.blob_bytes)
                    .ok_or_else(invalid)?
                    > self.limits.server.output_bytes
                {
                    return Err(invalid());
                }
                Ok(InteractionEvent::Chunk(c))
            }
            output_event::Event::Close(c) => {
                self.streams.close(&c)?;
                Ok(InteractionEvent::Close(c))
            }
            output_event::Event::Blob(b) => {
                let grant = b.output_grant.ok_or_else(invalid)?;
                let descriptor = crate::transport::routine::blob(b.blob.ok_or_else(invalid)?)?;
                let port = self
                    .limits
                    .definition
                    .ports
                    .iter()
                    .find(|p| {
                        p.name == b.port
                            && p.direction == RoutineDirection::Output
                            && matches!(p.media, RoutineMedia::Image | RoutineMedia::File)
                    })
                    .ok_or_else(invalid)?;
                if !descriptor
                    .remaining_validity_ms
                    .is_some_and(|ms| ms.0 > 0 && ms.0 <= self.limits.server.binary_output_lease_ms)
                    || !port.formats.contains(&descriptor.content_type)
                    || (port.media == RoutineMedia::Image
                        && (descriptor.image.is_none()
                            || descriptor.bytes.0 > self.limits.server.image_bytes))
                    || (port.media == RoutineMedia::File
                        && (descriptor.image.is_some()
                            || descriptor.bytes.0 > self.limits.server.file_bytes))
                {
                    return Err(invalid());
                }
                let next_blob = self
                    .blob_bytes
                    .checked_add(descriptor.bytes.0)
                    .ok_or_else(invalid)?;
                if self
                    .streams
                    .total
                    .checked_add(next_blob)
                    .ok_or_else(invalid)?
                    > self.limits.server.output_bytes
                {
                    return Err(invalid());
                }
                if grant.invocation_id != *self.accepted.as_ref().unwrap()
                    || grant.port != b.port
                    || grant.blob_id != descriptor.blob_id
                    || !(32..=64).contains(&grant.secret.len())
                    || grant.generation.is_empty()
                    || descriptor.bytes.0 > port.max_bytes.0
                    || !self.blob_ports.insert(b.port.clone())
                {
                    return Err(invalid());
                }
                self.grants.insert(b.port.clone(), BinaryOutputGrant(grant));
                self.blob_bytes = next_blob;
                Ok(InteractionEvent::Blob {
                    port: b.port,
                    descriptor,
                })
            }
            output_event::Event::Terminal(t) => {
                let status = crate::transport::routine::invocation_status(t.status)?;
                if !status.is_terminal()
                    || (t.status == 3
                        && (!t.cleanup_complete
                            || !self.streams.all_closed()
                            || self.limits.definition.ports.iter().any(|p| {
                                p.direction == RoutineDirection::Output
                                    && p.required
                                    && !self.blob_ports.contains(&p.name)
                                    && !self
                                        .streams
                                        .entries
                                        .values()
                                        .any(|s| s.port == p.name && s.closed)
                            })))
                {
                    return Err(invalid());
                }
                if let Some(value) = &t.result {
                    self.limits
                        .definition
                        .result
                        .validate(&crate::transport::routine::json(value.clone(), 65536)?)?;
                }
                for descriptor in &t.text_results {
                    crate::transport::routine::text_result(descriptor.clone())?;
                }
                let cleanup_complete = t.cleanup_complete;
                self.terminal = Some(t);
                self.queue.end();
                Ok(InteractionEvent::Terminal {
                    status,
                    cleanup_complete,
                })
            }
            _ => Err(invalid()),
        }
    }
}
impl Stream for InteractionOutput {
    type Item = Result<InteractionEvent, ClientError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let split_cancelled = {
            let state = self.queue.state.lock().unwrap();
            state.split && state.cancelled
        };
        if split_cancelled {
            self.done = true;
            self.source = Box::pin(futures_util::stream::empty());
            return Poll::Ready(Some(Err(invalid())));
        }
        self.queue.output.register(cx.waker());
        match self.source.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(r)) => match r.and_then(|r| self.validate(r)) {
                Ok(r) => Poll::Ready(Some(Ok(r))),
                Err(e) => {
                    self.done = true;
                    self.terminal = None;
                    self.grants.clear();
                    self.queue.cancel();
                    self.source = Box::pin(futures_util::stream::empty());
                    Poll::Ready(Some(Err(e)))
                }
            },
            Poll::Ready(None) => {
                self.done = true;
                if self.terminal.is_none() && !self.rejected {
                    self.queue.cancel();
                    Poll::Ready(Some(Err(invalid())))
                } else {
                    self.clean = true;
                    Poll::Ready(None)
                }
            }
        }
    }
}
impl Drop for InteractionOutput {
    fn drop(&mut self) {
        if !self.clean {
            self.queue.cancel();
        }
    }
}
