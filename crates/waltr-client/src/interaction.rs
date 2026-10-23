//! Caller-driven, UI-independent Routine interactions. No task or runtime is spawned.
//!
//! Poll output concurrently with input production. `finish_input` releases input without
//! cancelling output; retain the input handle until completion. Dropping either half or
//! calling `cancel` cancels the owned RPC. Never retry an interaction automatically.
//!
//! The host must retain an `InteractionControl` and cancel it on access invalidation,
//! logout, or host disposal. These caller-owned operations are deliberately outside
//! `ClientRuntime`'s watch/effect loop and do not change its snapshots or command lane.
//! Native cancellation also bypasses a full request queue with a protocol Cancel;
//! dropping or polling cancelled output disconnects the response RPC. A Run operation
//! has no request-side control stream, so poll or drop its output after cancellation.

use crate::ClientError;
use futures_core::Stream;
use futures_util::{StreamExt, task::AtomicWaker};
use prost::Message;
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

/// Wire values are shared with the server contract, without UI serialization.
pub use waltr::grpc::waltr::routine::v1 as wire;
use wire::{
    interact_request, interact_response, interaction_input_chunk, interaction_output_chunk,
    interaction_stream_format,
};

/// Memory limits apply before enqueue/upload and before retaining response state.
#[derive(Clone, Debug)]
pub struct InteractionLimits {
    /// Maximum queued request messages.
    pub input_items: usize,
    /// Maximum encoded bytes in the request queue.
    pub input_bytes: usize,
    /// Maximum audio/text chunk payload bytes.
    pub chunk_bytes: usize,
    /// Maximum encoded streaming message size in either direction.
    pub output_message_bytes: usize,
    /// Maximum encoded complete Run body, including metadata.
    pub complete_body_bytes: usize,
    /// Maximum retained text and segment-ID bytes per direction.
    pub text_bytes: usize,
    /// Maximum retained text segments per direction.
    pub text_segments: usize,
    /// Maximum lifetime stream IDs per direction, including closed streams.
    pub streams: usize,
}
impl Default for InteractionLimits {
    fn default() -> Self {
        Self {
            input_items: 8,
            input_bytes: 1024 * 1024,
            chunk_bytes: 256 * 1024,
            output_message_bytes: 1024 * 1024,
            complete_body_bytes: 32 * 1024 * 1024,
            text_bytes: 1024 * 1024,
            text_segments: 4096,
            streams: 64,
        }
    }
}
impl InteractionLimits {
    pub(crate) fn validate(&self) -> Result<(), ClientError> {
        if [
            self.input_items,
            self.input_bytes,
            self.chunk_bytes,
            self.output_message_bytes,
            self.complete_body_bytes,
            self.text_bytes,
            self.text_segments,
            self.streams,
        ]
        .contains(&0)
        {
            return Err(invalid("interaction limits must be positive"));
        }
        Ok(())
    }
}
fn invalid(message: &str) -> ClientError {
    ClientError::new(message)
}

/// Ordered segment assembler. A commitment freezes the segment permanently.
#[derive(Clone, Debug, Default)]
pub struct InteractionText {
    segments: Vec<(String, String, bool)>,
    bytes: usize,
}
impl InteractionText {
    /// Apply one revision atomically; errors leave the assembly unchanged.
    ///
    /// # Errors
    /// Rejects invalid operations or IDs, revisions to frozen segments, and text-state limits.
    pub fn update(
        &mut self,
        update: &wire::TextUpdate,
        limits: &InteractionLimits,
    ) -> Result<(), ClientError> {
        if update.segment_id.is_empty() || !matches!(update.operation, 1 | 2) {
            return Err(invalid("invalid text segment or operation"));
        }
        let index = self.segments.iter().position(|s| s.0 == update.segment_id);
        let old = index.map(|i| &self.segments[i]);
        if old.is_some_and(|s| s.2) {
            return Err(invalid("text segment is committed"));
        }
        let old_bytes = old.map_or(0, |s| s.1.len());
        let new_bytes = if update.operation == 1 {
            old_bytes.checked_add(update.text.len())
        } else {
            Some(update.text.len())
        }
        .ok_or_else(|| invalid("text size overflow"))?;
        let bytes = self.bytes - old_bytes + new_bytes;
        let id_bytes: usize = self.segments.iter().map(|s| s.0.len()).sum();
        if bytes
            .saturating_add(id_bytes)
            .saturating_add(if index.is_none() {
                update.segment_id.len()
            } else {
                0
            })
            > limits.text_bytes
            || (index.is_none() && self.segments.len() >= limits.text_segments)
        {
            return Err(invalid("text state limit exceeded"));
        }
        let i = index.unwrap_or_else(|| {
            self.segments
                .push((update.segment_id.clone(), String::new(), false));
            self.segments.len() - 1
        });
        if update.operation == 2 {
            // Fresh allocation releases old capacity; clone_from would retain it.
            #[allow(clippy::assigning_clones)]
            {
                self.segments[i].1 = update.text.clone();
            }
        } else {
            self.segments[i].1.push_str(&update.text);
        }
        self.segments[i].2 = update.committed;
        self.bytes = bytes;
        Ok(())
    }
    /// Current text in segment first-appearance order, without inserted separators.
    #[must_use]
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.1.as_str()).collect()
    }
    /// Contiguous frozen text in first-appearance order, stopping at the first draft.
    #[must_use]
    pub fn committed_prefix(&self) -> String {
        self.segments
            .iter()
            .take_while(|s| s.2)
            .map(|s| s.1.as_str())
            .collect()
    }
    /// Whether every observed segment is frozen.
    #[must_use]
    pub fn is_committed(&self) -> bool {
        self.segments.iter().all(|s| s.2)
    }
}

#[derive(Clone, Debug)]
struct PortStream {
    port: String,
    format: wire::InteractionStreamFormat,
    text: InteractionText,
    closed: Option<i32>,
}
#[derive(Clone, Debug, Default)]
struct Streams {
    entries: BTreeMap<String, PortStream>,
}
fn validate_format(format: wire::InteractionStreamFormat) -> Result<(), ClientError> {
    match &format.content {
        Some(interaction_stream_format::Content::Text(_)) => Ok(()),
        Some(interaction_stream_format::Content::Audio(a))
            if a.encoding == 1
                && a.sample_rate_hz > 0
                && a.channels > 0
                && (a.channels as usize).checked_mul(2).is_some() =>
        {
            Ok(())
        }
        _ => Err(invalid("unsupported interaction stream format")),
    }
}
impl Streams {
    fn open(
        &mut self,
        id: String,
        port: String,
        format: Option<wire::InteractionStreamFormat>,
        limits: &InteractionLimits,
    ) -> Result<(), ClientError> {
        if id.is_empty()
            || port.is_empty()
            || self.entries.contains_key(&id)
            || self.entries.len() >= limits.streams
            || id.len().saturating_add(port.len()) > limits.chunk_bytes
        {
            return Err(invalid(
                "invalid, duplicate, or excessive interaction stream",
            ));
        }
        let format = format.ok_or_else(|| invalid("missing stream format"))?;
        validate_format(format)?;
        self.entries.insert(
            id,
            PortStream {
                port,
                format,
                text: InteractionText::default(),
                closed: None,
            },
        );
        Ok(())
    }
    fn chunk(
        &mut self,
        id: &str,
        audio: Option<&[u8]>,
        text: Option<&wire::TextUpdate>,
        limits: &InteractionLimits,
    ) -> Result<(), ClientError> {
        let stream = self
            .entries
            .get_mut(id)
            .ok_or_else(|| invalid("chunk before stream open"))?;
        if stream.closed.is_some() {
            return Err(invalid("chunk after stream close"));
        }
        match (&stream.format.content, audio, text) {
            (Some(interaction_stream_format::Content::Audio(format)), Some(data), None)
                if data.len() <= limits.chunk_bytes
                    && data.len() % (format.channels as usize * 2) == 0 =>
            {
                Ok(())
            }
            (Some(interaction_stream_format::Content::Text(_)), None, Some(update))
                if update.encoded_len() <= limits.chunk_bytes =>
            {
                stream.text.update(update, limits)
            }
            _ => Err(invalid(
                "chunk does not match stream format or exceeds limit",
            )),
        }?;
        let segments: usize = self.entries.values().map(|s| s.text.segments.len()).sum();
        let bytes: usize = self
            .entries
            .values()
            .map(|s| s.text.bytes + s.text.segments.iter().map(|v| v.0.len()).sum::<usize>())
            .sum();
        if segments > limits.text_segments || bytes > limits.text_bytes {
            return Err(invalid("direction text state limit exceeded"));
        }
        Ok(())
    }
    fn close(&mut self, id: &str, status: i32, failure: bool) -> Result<(), ClientError> {
        let stream = self
            .entries
            .get_mut(id)
            .ok_or_else(|| invalid("close before stream open"))?;
        if stream.closed.is_some()
            || !matches!(status, 1..=3)
            || failure != (status == 3)
            || (status == 1 && !stream.text.is_committed())
        {
            return Err(invalid("invalid stream close or uncommitted text"));
        }
        stream.closed = Some(status);
        Ok(())
    }
    fn all_closed(&self) -> bool {
        self.entries.values().all(|s| s.closed.is_some())
    }
}

// Admission, cancellation delivery, and RPC completion are independent queue facts.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct QueueState {
    items: std::collections::VecDeque<(wire::InteractRequest, usize)>,
    bytes: usize,
    cancelled: bool,
    #[cfg(not(target_arch = "wasm32"))]
    cancel_sent: bool,
    accepted: bool,
    ended: bool,
}
#[derive(Debug)]
pub(crate) struct InteractionQueue {
    state: Mutex<QueueState>,
    reader: AtomicWaker,
    writer: AtomicWaker,
    output: AtomicWaker,
}
impl InteractionQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState {
                items: std::collections::VecDeque::default(),
                bytes: 0,
                cancelled: false,
                #[cfg(not(target_arch = "wasm32"))]
                cancel_sent: false,
                accepted: false,
                ended: false,
            }),
            reader: AtomicWaker::new(),
            writer: AtomicWaker::new(),
            output: AtomicWaker::new(),
        })
    }
    fn cancel(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.cancelled = true;
            state.items.clear();
            state.bytes = 0;
        }
        self.reader.wake();
        self.writer.wake();
        self.output.wake();
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn seed(&self, request: wire::InteractRequest) {
        let bytes = request.encoded_len();
        let mut state = self.state.lock().unwrap();
        state.bytes += bytes;
        state.items.push_back((request, bytes));
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct InteractionRequests(pub Arc<InteractionQueue>);
#[cfg(not(target_arch = "wasm32"))]
impl Stream for InteractionRequests {
    type Item = wire::InteractRequest;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.reader.register(cx.waker());
        let mut state = self.0.state.lock().unwrap();
        if state.ended {
            return Poll::Ready(None);
        }
        if state.cancelled {
            if state.cancel_sent {
                return Poll::Ready(None);
            }
            state.cancel_sent = true;
            return Poll::Ready(Some(wire::InteractRequest {
                event: Some(interact_request::Event::Cancel(wire::InteractionCancel {})),
            }));
        }
        if let Some((request, bytes)) = state.items.pop_front() {
            state.bytes -= bytes;
            drop(state);
            self.0.writer.wake();
            Poll::Ready(Some(request))
        } else {
            Poll::Pending
        }
    }
}

/// Single-producer input/control half. Sending is bounded and cancellation-responsive.
/// Dropping this handle cancels, including after `finish_input`; keep it alive while reading output.
#[derive(Debug)]
pub struct InteractionInputHandle {
    queue: Arc<InteractionQueue>,
    limits: InteractionLimits,
    streams: Streams,
    finished: bool,
}
impl InteractionInputHandle {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn new(queue: Arc<InteractionQueue>, limits: InteractionLimits) -> Self {
        Self {
            queue,
            limits,
            streams: Streams::default(),
            finished: false,
        }
    }
    /// Cancel locally and disconnect the owned RPC on the next poll/drop.
    pub fn cancel(&self) {
        self.queue.cancel();
    }
    /// Clone an out-of-band control handle usable while a send/read is pending.
    #[must_use]
    pub fn control(&self) -> InteractionControl {
        InteractionControl(self.queue.clone())
    }
    /// Send one input event after observing Accepted. Open/cancel/completion have dedicated APIs.
    /// If this future is dropped while backpressured, no event or validation state is committed.
    ///
    /// # Errors
    /// Rejects input before acceptance, after completion/cancellation, invalid stream events,
    /// and messages or retained text exceeding the configured limits.
    pub async fn send(&mut self, event: interact_request::Event) -> Result<(), ClientError> {
        if self.finished {
            return Err(invalid("input already completed"));
        }
        let mut streams = self.streams.clone();
        match &event {
            interact_request::Event::InputOpened(o) => streams.open(
                o.stream_id.clone(),
                o.port_name.clone(),
                o.format,
                &self.limits,
            )?,
            interact_request::Event::InputChunk(c) => {
                let (audio, text) = match &c.content {
                    Some(interaction_input_chunk::Content::Audio(a)) => (Some(a.as_slice()), None),
                    Some(interaction_input_chunk::Content::Text(t)) => (None, Some(t)),
                    _ => (None, None),
                };
                streams.chunk(&c.stream_id, audio, text, &self.limits)?;
            }
            interact_request::Event::InputClosed(c) => {
                streams.close(&c.stream_id, c.status, c.failure.is_some())?;
            }
            _ => return Err(invalid("expected input open/chunk/close")),
        }
        self.enqueue(wire::InteractRequest { event: Some(event) })
            .await?;
        self.streams = streams;
        Ok(())
    }
    /// All streams must already be closed. Keeps the RPC and cancellation handle alive.
    ///
    /// # Errors
    /// Rejects open streams, repeated completion, missing acceptance, cancellation,
    /// or a completion message exceeding the configured queue/message limits.
    pub async fn finish_input(&mut self) -> Result<(), ClientError> {
        if self.finished || !self.streams.all_closed() {
            return Err(invalid("input is completed or streams remain open"));
        }
        self.enqueue(wire::InteractRequest {
            event: Some(interact_request::Event::InputCompleted(
                wire::InteractionInputCompleted {},
            )),
        })
        .await?;
        self.finished = true;
        Ok(())
    }
    async fn enqueue(&self, request: wire::InteractRequest) -> Result<(), ClientError> {
        let bytes = request.encoded_len();
        if bytes > self.limits.input_bytes || bytes > self.limits.output_message_bytes {
            return Err(invalid("input message limit exceeded"));
        }
        let mut request = Some(request);
        futures_util::future::poll_fn(|cx| {
            self.queue.writer.register(cx.waker());
            let mut state = self.queue.state.lock().unwrap();
            if state.cancelled || state.ended {
                return Poll::Ready(Err(invalid("interaction cancelled or ended")));
            }
            if !state.accepted {
                return Poll::Ready(Err(invalid("wait for interaction Accepted before input")));
            }
            if state.items.len() >= self.limits.input_items
                || state.bytes + bytes > self.limits.input_bytes
            {
                return Poll::Pending;
            }
            state.items.push_back((request.take().unwrap(), bytes));
            state.bytes += bytes;
            drop(state);
            self.queue.reader.wake();
            Poll::Ready(Ok(()))
        })
        .await
    }
}
impl Drop for InteractionInputHandle {
    fn drop(&mut self) {
        self.queue.cancel();
    }
}

/// The two independently polled halves of one native interaction.
#[derive(Debug)]
pub struct InteractionSession {
    /// Retain until output completes, even after finishing input.
    pub input: InteractionInputHandle,
    /// Pull ordered output events concurrently with input production.
    pub output: InteractionOutput,
}

/// Clonable out-of-band cancellation; dropping this control alone does not cancel.
#[derive(Clone, Debug)]
pub struct InteractionControl(Arc<InteractionQueue>);
impl InteractionControl {
    /// Cancel both halves; wakes pending bounded send and output read.
    pub fn cancel(&self) {
        self.0.cancel();
    }
}

/// Content-free reason selected interaction output is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractionOutputFailure {
    /// A validated remote terminal reported failure.
    RemoteFailed,
    /// A validated remote terminal reported interruption.
    RemoteInterrupted,
    /// The remote admission response rejected the interaction.
    Rejected,
    /// Required selected output is absent or invalid.
    Protocol,
}

/// Ordered pull-based output: no snapshot or unbounded event queue contains media.
/// Owns the RPC; drop cancels. Poll through EOF to detect output after terminal.
pub struct InteractionOutput {
    source: Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, ClientError>>>>,
    queue: Arc<InteractionQueue>,
    limits: InteractionLimits,
    streams: Streams,
    accepted: bool,
    terminal: Option<wire::InteractionCompleted>,
    rejected: bool,
    done: bool,
}
impl std::fmt::Debug for InteractionOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractionOutput")
            .field("accepted", &self.accepted)
            .field("terminal", &self.terminal)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}
impl InteractionOutput {
    pub(crate) fn new(
        source: Pin<Box<dyn Stream<Item = Result<wire::InteractResponse, ClientError>>>>,
        queue: Arc<InteractionQueue>,
        limits: InteractionLimits,
    ) -> Self {
        Self {
            source,
            queue,
            limits,
            streams: Streams::default(),
            accepted: false,
            terminal: None,
            rejected: false,
            done: false,
        }
    }
    /// Cancel locally and disconnect the owned RPC on the next poll/drop.
    pub fn cancel(&self) {
        self.queue.cancel();
    }
    /// Clone an out-of-band control handle usable while a send/read is pending.
    #[must_use]
    pub fn control(&self) -> InteractionControl {
        InteractionControl(self.queue.clone())
    }
    /// Observed terminal, not a success guarantee until clean EOF (see `final_text`).
    #[must_use]
    pub const fn terminal(&self) -> Option<&wire::InteractionCompleted> {
        self.terminal.as_ref()
    }
    /// Classify a selected-output access failure from validated state, never error text.
    /// Stream read errors must retain their own boundary instead of using this helper.
    #[must_use]
    pub fn output_failure(&self) -> InteractionOutputFailure {
        if self.rejected {
            return InteractionOutputFailure::Rejected;
        }
        match self.terminal.as_ref().map(|terminal| terminal.status) {
            Some(status) if status == wire::RoutineInvocationStatus::Failed as i32 => {
                InteractionOutputFailure::RemoteFailed
            }
            Some(status) if status == wire::RoutineInvocationStatus::Interrupted as i32 => {
                InteractionOutputFailure::RemoteInterrupted
            }
            _ => InteractionOutputFailure::Protocol,
        }
    }
    /// True only after SUCCEEDED and clean response EOF, never on rejection or error.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.done
            && self
                .terminal
                .as_ref()
                .is_some_and(|t| t.status == wire::RoutineInvocationStatus::Succeeded as i32)
    }
    /// Current hypothesis from exactly one text stream; never implies success.
    #[must_use]
    pub fn preview_text(&self, port: &str) -> Option<String> {
        let mut streams = self.streams.entries.values().filter(|s| s.port == port);
        let stream = streams.next()?;
        if streams.next().is_some()
            || !matches!(
                stream.format.content,
                Some(interaction_stream_format::Content::Text(_))
            )
        {
            return None;
        }
        Some(stream.text.text())
    }

    /// Stable contiguous committed prefix from exactly one selected text stream.
    /// `None` means that no selected stream has opened yet, not an empty frozen segment.
    /// This is not a terminal-success guarantee; already consumed text cannot be rolled back.
    ///
    /// # Errors
    /// Rejects cancelled/failed output and ambiguous or non-text selected streams.
    pub fn committed_prefix(&self, port: &str) -> Result<Option<String>, ClientError> {
        self.selected_text(port, false)
    }

    /// Validated current hypothesis for live external typing; never implies success.
    /// `None` means no selected stream has opened. Drafts remain revisable.
    ///
    /// # Errors
    /// Rejects cancelled/failed output and ambiguous or non-text selected streams.
    pub fn live_text(&self, port: &str) -> Result<Option<String>, ClientError> {
        self.selected_text(port, true)
    }

    fn selected_text(&self, port: &str, live: bool) -> Result<Option<String>, ClientError> {
        if self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancelled
            || self.rejected
            || self
                .terminal
                .as_ref()
                .is_some_and(|t| t.status != wire::RoutineInvocationStatus::Succeeded as i32)
        {
            return Err(invalid("interaction is not valid for committed output"));
        }
        let mut streams = self.streams.entries.values().filter(|s| s.port == port);
        let Some(stream) = streams.next() else {
            return Ok(None);
        };
        if streams.next().is_some()
            || !matches!(
                stream.format.content,
                Some(interaction_stream_format::Content::Text(_))
            )
            || stream.closed.is_some_and(|status| status != 1)
        {
            return Err(invalid("selected output is not one text stream"));
        }
        Ok(Some(if live {
            stream.text.text()
        } else {
            stream.text.committed_prefix()
        }))
    }

    /// Final text is available only after successful terminal AND clean response EOF.
    /// Multiple streams for the selected port are rejected rather than concatenated.
    ///
    /// # Errors
    /// Rejects unsuccessful or incomplete interactions and any port without exactly one
    /// successfully closed text stream.
    pub fn final_text(&self, port: &str) -> Result<String, ClientError> {
        if !self.succeeded() {
            return Err(invalid("interaction has not successfully completed"));
        }
        let streams: Vec<_> = self
            .streams
            .entries
            .values()
            .filter(|s| s.port == port)
            .collect();
        if streams.len() != 1
            || streams[0].closed != Some(1)
            || !matches!(
                streams[0].format.content,
                Some(interaction_stream_format::Content::Text(_))
            )
        {
            return Err(invalid(
                "selected output is not one successfully closed text stream",
            ));
        }
        Ok(streams[0].text.text())
    }
    fn validate(&mut self, response: &wire::InteractResponse) -> Result<(), ClientError> {
        if response.encoded_len() > self.limits.output_message_bytes {
            return Err(invalid("output message limit exceeded"));
        }
        if self.terminal.is_some() || self.rejected {
            return Err(invalid("output after terminal"));
        }
        let event = response
            .event
            .as_ref()
            .ok_or_else(|| invalid("empty interaction response"))?;
        if !self.accepted {
            match event {
                interact_response::Event::Accepted(a) if !a.invocation_id.is_empty() => {
                    self.accepted = true;
                    self.queue.state.lock().unwrap().accepted = true;
                    return Ok(());
                }
                interact_response::Event::ValidationFailure(_) => {
                    self.rejected = true;
                    self.queue.state.lock().unwrap().ended = true;
                    self.queue.reader.wake();
                    self.queue.writer.wake();
                    return Ok(());
                }
                _ => {
                    return Err(invalid(
                        "first response must be accepted or validation failure",
                    ));
                }
            }
        }
        match event {
            interact_response::Event::OutputOpened(o) => self.streams.open(
                o.stream_id.clone(),
                o.port_name.clone(),
                o.format,
                &self.limits,
            ),
            interact_response::Event::OutputChunk(c) => {
                let (audio, text) = match &c.content {
                    Some(interaction_output_chunk::Content::Audio(a)) => (Some(a.as_slice()), None),
                    Some(interaction_output_chunk::Content::Text(t)) => (None, Some(t)),
                    _ => (None, None),
                };
                self.streams.chunk(&c.stream_id, audio, text, &self.limits)
            }
            interact_response::Event::OutputClosed(c) => {
                self.streams
                    .close(&c.stream_id, c.status, c.failure.is_some())
            }
            interact_response::Event::Completed(c) => {
                let succeeded = c.status == wire::RoutineInvocationStatus::Succeeded as i32;
                if !self.streams.all_closed()
                    || !matches!(
                        wire::RoutineInvocationStatus::try_from(c.status),
                        Ok(wire::RoutineInvocationStatus::Succeeded
                            | wire::RoutineInvocationStatus::Failed
                            | wire::RoutineInvocationStatus::Interrupted)
                    )
                    || c.failure.is_some() == succeeded
                {
                    return Err(invalid("invalid interaction terminal or unclosed output"));
                }
                self.terminal = Some(c.clone());
                let mut state = self.queue.state.lock().unwrap();
                state.ended = true;
                state.items.clear();
                state.bytes = 0;
                drop(state);
                self.queue.reader.wake();
                self.queue.writer.wake();
                Ok(())
            }
            _ => Err(invalid("duplicate admission response")),
        }
    }
}
impl Stream for InteractionOutput {
    type Item = Result<wire::InteractResponse, ClientError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        self.queue.output.register(cx.waker());
        if self.queue.state.lock().unwrap().cancelled {
            self.done = true;
            self.terminal = None;
            self.source = Box::pin(futures_util::stream::empty());
            return Poll::Ready(Some(Err(invalid("interaction cancelled"))));
        }
        match self.source.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(response))) => match self.validate(&response) {
                Ok(()) => Poll::Ready(Some(Ok(response))),
                Err(error) => {
                    self.done = true;
                    self.terminal = None;
                    self.queue.cancel();
                    self.source = Box::pin(futures_util::stream::empty());
                    Poll::Ready(Some(Err(error)))
                }
            },
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                self.terminal = None;
                self.queue.cancel();
                self.source = Box::pin(futures_util::stream::empty());
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.done = true;
                if self.terminal.is_none() && !self.rejected {
                    self.queue.cancel();
                    Poll::Ready(Some(Err(invalid("interaction EOF before terminal"))))
                } else {
                    Poll::Ready(None)
                }
            }
        }
    }
}
impl Drop for InteractionOutput {
    fn drop(&mut self) {
        self.queue.cancel();
    }
}

/// Validate complete inputs before the browser/native Run upload begins.
///
/// # Errors
/// Rejects invalid limits, missing admission data, excessive body/input sizes, duplicate
/// or empty ports, unsupported content/formats, and incomplete PCM sample frames.
pub fn validate_complete_interaction(
    request: &wire::RunInteractionRequest,
    limits: &InteractionLimits,
) -> Result<(), ClientError> {
    limits.validate()?;
    if request.open.is_none()
        || request.encoded_len() > limits.complete_body_bytes
        || request.inputs.len() > limits.streams
    {
        return Err(invalid(
            "missing open or complete interaction body limit exceeded",
        ));
    }
    let mut ports = std::collections::BTreeSet::new();
    for input in &request.inputs {
        if input.port_name.is_empty() || !ports.insert(&input.port_name) {
            return Err(invalid("invalid or duplicate complete input port"));
        }
        match &input.content {
            Some(wire::interaction_input::Content::Audio(a)) => {
                let format = a
                    .format
                    .as_ref()
                    .ok_or_else(|| invalid("missing audio format"))?;
                validate_format(wire::InteractionStreamFormat {
                    content: Some(interaction_stream_format::Content::Audio(*format)),
                })?;
                if a.data.len() % (format.channels as usize * 2) != 0 {
                    return Err(invalid("incomplete audio sample frame"));
                }
            }
            Some(wire::interaction_input::Content::Text(text))
                if text.len() <= limits.text_bytes => {}
            _ => {
                return Err(invalid(
                    "invalid complete input content or text limit exceeded",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(test, all(target_arch = "wasm32", feature = "wasm")))]
pub(crate) fn browser_audio_run_request(
    routine: String,
    trigger: String,
    input: String,
    pcm: &[u8],
) -> wire::RunInteractionRequest {
    wire::RunInteractionRequest {
        open: Some(wire::OpenInteraction {
            routine_name: routine,
            trigger_name: trigger,
            source_session_id: None,
            params: vec![],
            request_id: None,
        }),
        inputs: vec![wire::InteractionInput {
            port_name: input,
            content: Some(wire::interaction_input::Content::Audio(
                wire::InteractionAudioInput {
                    format: Some(wire::InteractionAudioFormat {
                        encoding: 1,
                        sample_rate_hz: 24000,
                        channels: 1,
                    }),
                    data: pcm.to_vec(),
                },
            )),
        }],
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn browser_audio_request_omits_unsupported_source_session_on_the_wire() {
        let request = browser_audio_run_request(
            "transcribe".into(),
            "dictate".into(),
            "audio".into(),
            &[1, 0, 2, 0],
        );
        validate_complete_interaction(&request, &InteractionLimits::default()).unwrap();
        let encoded = request.encode_to_vec();
        let decoded = wire::RunInteractionRequest::decode(encoded.as_slice()).unwrap();
        let open = decoded.open.as_ref().unwrap();
        assert_eq!(open.routine_name, "transcribe");
        assert_eq!(open.trigger_name, "dictate");
        // Server prepare rejects every Some source_session_id, including an empty one.
        assert_eq!(open.source_session_id, None);
        assert!(open.params.is_empty());
        assert_eq!(open.request_id, None);
        assert_eq!(decoded.inputs.len(), 1);
        assert_eq!(decoded.inputs[0].port_name, "audio");
        let Some(wire::interaction_input::Content::Audio(audio)) = &decoded.inputs[0].content
        else {
            panic!("browser Run must contain binary audio");
        };
        assert_eq!(audio.data, [1, 0, 2, 0]);
        assert_eq!(
            audio.format,
            Some(wire::InteractionAudioFormat {
                encoding: 1,
                sample_rate_hz: 24000,
                channels: 1,
            })
        );
        let limits = InteractionLimits {
            complete_body_bytes: encoded.len() - 1,
            ..InteractionLimits::default()
        };
        assert!(validate_complete_interaction(&request, &limits).is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn browser_audio_request_passes_real_server_source_session_guard() {
        use wire::{
            interact_response::Event,
            routine_interaction_service_client::RoutineInteractionServiceClient,
        };
        let server = waltr_server::test_support::setup_server();
        let channel = tonic::transport::Endpoint::from_shared(server.base_url())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = RoutineInteractionServiceClient::new(channel);
        let request = browser_audio_run_request(
            "browser-admission-missing-routine".into(),
            "dictate".into(),
            "audio".into(),
            &[0, 0],
        );
        for source in [
            None,
            Some(String::new()),
            Some("local-composer-session".into()),
        ] {
            let mut candidate = request.clone();
            candidate.open.as_mut().unwrap().source_session_id = source.clone();
            let mut responses = client.run(candidate).await.unwrap().into_inner();
            let Event::ValidationFailure(failure) =
                responses.message().await.unwrap().unwrap().event.unwrap()
            else {
                panic!(
                    "expected pre-execution validation failure for the intentionally missing routine"
                );
            };
            let expected = if source.is_some() {
                "source sessions are not supported by interaction execution"
            } else {
                "interaction routine was not found"
            };
            assert_eq!(failure.message, expected);
            assert!(responses.message().await.unwrap().is_none());
        }
    }

    #[test]
    fn committed_prefix_stops_at_first_draft_in_first_appearance_order() {
        let mut text = InteractionText::default();
        let limits = InteractionLimits::default();
        let mut update = |id: &str, value: &str, operation, committed| {
            text.update(
                &wire::TextUpdate {
                    segment_id: id.into(),
                    text: value.into(),
                    operation,
                    committed,
                },
                &limits,
            )
            .unwrap();
            text.committed_prefix()
        };
        assert_eq!(update("z", "draft", 2, false), "");
        assert_eq!(update("a", "後", 2, true), "");
        assert_eq!(update("z", "你", 2, false), "");
        assert_eq!(update("z", "好", 1, true), "你好後");
        assert_eq!(update("empty", "", 2, true), "你好後");
        assert!(
            text.update(
                &wire::TextUpdate {
                    segment_id: "empty".into(),
                    text: "rewrite".into(),
                    operation: 2,
                    committed: true
                },
                &limits
            )
            .is_err()
        );
    }

    #[test]
    fn output_failure_uses_only_validated_terminal_state() {
        use InteractionOutputFailure as F;
        for (status, expected) in [
            (wire::RoutineInvocationStatus::Failed, F::RemoteFailed),
            (
                wire::RoutineInvocationStatus::Interrupted,
                F::RemoteInterrupted,
            ),
            (wire::RoutineInvocationStatus::Succeeded, F::Protocol),
        ] {
            let mut output = InteractionOutput::new(
                Box::pin(futures_util::stream::empty()),
                InteractionQueue::new(),
                InteractionLimits::default(),
            );
            assert_eq!(output.output_failure(), F::Protocol);
            output
                .validate(&wire::InteractResponse {
                    event: Some(interact_response::Event::Accepted(
                        wire::InteractionAccepted {
                            invocation_id: "PRIVATE_INVOCATION_SENTINEL".into(),
                            ..Default::default()
                        },
                    )),
                })
                .unwrap();
            output
                .validate(&wire::InteractResponse {
                    event: Some(interact_response::Event::Completed(
                        wire::InteractionCompleted {
                            status: status as i32,
                            failure: (status != wire::RoutineInvocationStatus::Succeeded).then(
                                || wire::RoutineFailure {
                                    code: "PRIVATE_CODE_SENTINEL".into(),
                                    message: "PRIVATE_MESSAGE_SENTINEL".into(),
                                    ..Default::default()
                                },
                            ),
                            ..Default::default()
                        },
                    )),
                })
                .unwrap();
            output.done = true;
            assert!(output.final_text("missing").is_err());
            assert_eq!(output.output_failure(), expected);
        }
        let mut output = InteractionOutput::new(
            Box::pin(futures_util::stream::empty()),
            InteractionQueue::new(),
            InteractionLimits::default(),
        );
        output
            .validate(&wire::InteractResponse {
                event: Some(interact_response::Event::ValidationFailure(
                    Default::default(),
                )),
            })
            .unwrap();
        assert_eq!(output.output_failure(), F::Rejected);
    }

    #[test]
    fn preview_is_read_only_and_does_not_imply_final_success() {
        let mut output = InteractionOutput::new(
            Box::pin(futures_util::stream::empty()),
            InteractionQueue::new(),
            InteractionLimits::default(),
        );
        assert_eq!(output.preview_text("text"), None);
        let stream = PortStream {
            port: "text".into(),
            format: wire::InteractionStreamFormat {
                content: Some(interaction_stream_format::Content::Text(
                    wire::InteractionTextFormat::default(),
                )),
            },
            text: InteractionText::default(),
            closed: None,
        };
        output.streams.entries.insert("one".into(), stream.clone());
        assert_eq!(output.preview_text("text"), Some(String::new()));
        assert_eq!(output.live_text("text").unwrap(), Some(String::new()));
        output
            .streams
            .entries
            .get_mut("one")
            .unwrap()
            .text
            .update(
                &wire::TextUpdate {
                    segment_id: "draft".into(),
                    text: "interim".into(),
                    operation: 2,
                    committed: false,
                },
                &InteractionLimits::default(),
            )
            .unwrap();
        assert_eq!(
            output.live_text("text").unwrap().as_deref(),
            Some("interim")
        );
        output.streams.entries.get_mut("one").unwrap().closed = Some(3);
        assert!(output.live_text("text").is_err());
        output.streams.entries.get_mut("one").unwrap().closed = None;
        assert_eq!(
            output.committed_prefix("text").unwrap(),
            Some(String::new())
        );
        assert_eq!(output.committed_prefix("other").unwrap(), None);
        assert_eq!(output.preview_text("other"), None);
        assert!(output.final_text("text").is_err());
        assert!(!output.succeeded());
        output.streams.entries.insert("two".into(), stream);
        assert_eq!(output.preview_text("text"), None);
        assert!(output.committed_prefix("text").is_err());
        assert!(output.live_text("text").is_err());
        output.streams.entries.remove("two");
        output.cancel();
        assert!(output.live_text("text").is_err());
    }
}
