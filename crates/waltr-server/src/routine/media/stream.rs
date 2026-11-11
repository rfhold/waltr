//! Bounded typed edges. Close is explicit; dropping either side is never successful EOF.
use super::{Error, MediaFuture, Producer};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Admission direction, scoped to one invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Caller-produced input.
    Input,
    /// Host-produced output.
    Output,
}
/// A stream's immutable concrete media format; no implicit conversion occurs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Interleaved signed 16-bit little-endian samples with no container header.
    Pcm16Le {
        /// Samples per second per channel, from 8 kHz through 48 kHz.
        rate: u32,
        /// Interleaved channel count, from one through eight.
        channels: u16,
    },
    /// Ordered, individually committed text revisions.
    Text,
}
impl Format {
    /// Reject unsupported rates or channel counts before opening a stream.
    pub fn validate(self) -> Result<(), Error> {
        match self {
            Self::Text => Ok(()),
            Self::Pcm16Le {
                rate: 8000..=48000,
                channels: 1..=8,
            } => Ok(()),
            _ => Err(Error::Invalid),
        }
    }
}
/// Append or replace a stable segment; commitment permanently freezes its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    /// Stable opaque ID, at most 128 UTF-8 bytes.
    pub segment: String,
    /// Revision payload, never silently truncated.
    pub text: String,
    /// Replace existing text rather than append to it.
    pub replace: bool,
    /// Freeze the complete segment after this update.
    pub committed: bool,
}
/// One caller-owned payload awaiting admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chunk {
    /// Frame-aligned raw PCM bytes.
    Audio(Vec<u8>),
    /// A bounded text update.
    Text(Text),
}
impl Chunk {
    /// Payload admission cost, including the text segment ID.
    pub fn bytes(&self) -> usize {
        match self {
            Self::Audio(bytes) => bytes.len(),
            Self::Text(t) => t.segment.len().saturating_add(t.text.len()),
        }
    }
}
/// Unforgeable invocation, stream, and direction identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamKey {
    invocation: Uuid,
    stream: Uuid,
    direction: Direction,
}

/// Explicit standalone bounds. Defaults are also maximum admitted byte/count limits.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum payload bytes per write: 64 KiB.
    pub chunk_bytes: usize,
    /// Per-edge queued plus checked-out payload bytes: 1 MiB.
    pub edge_bytes: usize,
    /// Per-edge queued plus checked-out packets: 16.
    pub edge_chunks: usize,
    /// Finite admission wait, default 30 seconds.
    pub queue_wait: Duration,
    /// Finite consumer wait for the next frame, default 30 seconds.
    pub idle: Duration,
    /// Aggregate admitted PCM duration, default 65 minutes.
    pub input_duration: Duration,
    /// Absolute lifetime from invocation construction, default 90 minutes.
    pub total_duration: Duration,
    /// Aggregate input payload bytes, default 256 MiB.
    pub input_bytes: usize,
    /// Aggregate output payload bytes, default 256 MiB.
    pub output_bytes: usize,
    /// Lifetime stream opens per direction, default four; closing does not replenish.
    pub streams_per_direction: usize,
    /// Maximum fan-out branches, default four.
    pub tee_branches: usize,
    /// Count and retained bytes are independent; no silent segment eviction.
    pub segments: usize,
    /// Retained text and duplicated segment IDs per state, default 1 MiB, maximum 2 MiB.
    pub text_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            chunk_bytes: 65536,
            edge_bytes: 1024 * 1024,
            edge_chunks: 16,
            queue_wait: Duration::from_secs(30),
            idle: Duration::from_secs(30),
            input_duration: Duration::from_secs(65 * 60),
            total_duration: Duration::from_secs(90 * 60),
            input_bytes: 256 * 1024 * 1024,
            output_bytes: 256 * 1024 * 1024,
            streams_per_direction: 4,
            tee_branches: 4,
            segments: 4096,
            text_bytes: 1024 * 1024,
        }
    }
}
impl Limits {
    /// Validate finite waits and nonzero budgets within implementation ceilings.
    pub fn validate(&self) -> Result<(), Error> {
        if self.chunk_bytes == 0
            || self.chunk_bytes > 65536
            || self.edge_bytes == 0
            || self.edge_bytes > 1024 * 1024
            || self.edge_chunks == 0
            || self.edge_chunks > 16
            || self.chunk_bytes > self.edge_bytes
            || self.queue_wait.is_zero()
            || self.idle.is_zero()
            || self.input_duration.is_zero()
            || self.input_duration > Duration::from_secs(3900)
            || self.total_duration < self.input_duration
            || self.total_duration > Duration::from_secs(5400)
            || self.input_bytes == 0
            || self.input_bytes > 256 * 1024 * 1024
            || self.output_bytes == 0
            || self.output_bytes > 256 * 1024 * 1024
            || self.streams_per_direction == 0
            || self.streams_per_direction > 4
            || self.tee_branches == 0
            || self.tee_branches > 4
            || self.segments == 0
            || self.segments > 4096
            || self.text_bytes == 0
            || self.text_bytes > 2 * 1024 * 1024
            || std::time::Instant::now().checked_add(self.idle).is_none()
            || std::time::Instant::now()
                .checked_add(self.queue_wait)
                .is_none()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Explicit hierarchical retention pools; hosts share principal/process pools.
/// No principal map or implicit global allocation exists here.
#[derive(Clone, Debug)]
pub struct Pool {
    semaphore: Arc<Semaphore>,
    capacity: usize,
}
impl Pool {
    /// Construct an explicit payload-credit pool.
    pub fn new(bytes: usize) -> Result<Self, Error> {
        if bytes == 0 || bytes > u32::MAX as usize || bytes > Semaphore::MAX_PERMITS {
            return Err(Error::Invalid);
        }
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(bytes)),
            capacity: bytes,
        })
    }
    /// Current charged retention, not process RSS.
    pub fn retained(&self) -> usize {
        self.capacity - self.semaphore.available_permits()
    }
    /// Default 16 MiB principal pool; share one instance across that principal.
    pub fn principal() -> Self {
        Self::new(16 * 1024 * 1024).expect("finite principal pool")
    }
    /// Default 64 MiB process pool; share one instance across all principals.
    pub fn process() -> Self {
        Self::new(64 * 1024 * 1024).expect("finite process pool")
    }
}
#[derive(Debug)]
struct Credit {
    _permits: Vec<OwnedSemaphorePermit>,
}
#[derive(Clone, Debug)]
struct Budget {
    pools: Vec<Pool>,
}
impl Budget {
    fn reserve(&self, bytes: usize) -> Result<Credit, Error> {
        let bytes = u32::try_from(bytes.max(1)).map_err(|_| Error::Limit)?;
        let mut permits = Vec::with_capacity(self.pools.len());
        for pool in &self.pools {
            permits.push(
                pool.semaphore
                    .clone()
                    .try_acquire_many_owned(bytes)
                    .map_err(|_| Error::Limit)?,
            );
        }
        Ok(Credit { _permits: permits })
    }
}
/// Payload credits remain held until this packet is dropped, not merely dequeued.
pub struct Packet {
    chunk: Chunk,
    _credit: Credit,
    _edge: Option<OwnedSemaphorePermit>,
    _item: Option<OwnedSemaphorePermit>,
}
impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Packet")
            .field("bytes", &self.chunk.bytes())
            .finish_non_exhaustive()
    }
}
impl Packet {
    /// Borrow the admitted payload without surrendering its retention credits.
    pub fn chunk(&self) -> &Chunk {
        &self.chunk
    }
}
enum Frame {
    Data(Packet),
    Close,
    Failed(Error),
}
struct Queued {
    frame: Frame,
    edge: OwnedSemaphorePermit,
    item: OwnedSemaphorePermit,
}
#[derive(Default, Debug)]
struct Totals {
    opened: [usize; 2],
    bytes: [usize; 2],
    audio_frames: [Option<(u32, u64)>; 4],
}
#[derive(Debug)]
struct Shared {
    trace: tracing::Span,
    id: Uuid,
    limits: Limits,
    totals: Mutex<Totals>,
    budget: Budget,
    speech_workspace: super::snapshot::WorkspacePool,
    cancel: CancellationToken,
    started: tokio::time::Instant,
}
/// Concrete resource owner with an 8 MiB invocation payload-retention pool.
#[derive(Clone, Debug)]
pub struct Streams {
    shared: Arc<Shared>,
    retention: Pool,
}
/// Invocation-bound resource ownership. Hosts bind their principal's shared pool
/// before exposing this object; keys cannot be constructed or rebound by callers.
pub trait StreamResources: Send + Sync {
    /// Open one lifetime-counted edge with explicit consumer-drop semantics.
    fn open(
        &self,
        direction: Direction,
        format: Format,
        detachable: bool,
    ) -> Result<(Writer, Reader), Error>;
    /// The invocation token used to wake all blocked readers and writers.
    fn cancellation(&self) -> CancellationToken;
}
impl StreamResources for Streams {
    fn open(
        &self,
        direction: Direction,
        format: Format,
        detachable: bool,
    ) -> Result<(Writer, Reader), Error> {
        self.open(direction, format, detachable)
    }
    fn cancellation(&self) -> CancellationToken {
        self.cancellation()
    }
}
impl Streams {
    /// Bind shared principal/process pools before exposing invocation resources.
    pub fn new(
        limits: Limits,
        principal: Pool,
        process: Pool,
        cancel: CancellationToken,
    ) -> Result<Self, Error> {
        limits.validate()?;
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        // Copy context only: a tracing parent would retain the invocation.
        let parent = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .clone();
        let trace = tracing::info_span!(target: "waltr_server::routine::telemetry", parent: None, "routine.output_accounting", output_bytes_admitted = 0_u64);
        if parent.is_valid() {
            let _ =
                trace.set_parent(opentelemetry::Context::new().with_remote_span_context(parent));
        }
        let retention = Pool::new(8 * 1024 * 1024)?;
        Ok(Self {
            shared: Arc::new(Shared {
                trace,
                id: Uuid::new_v4(),
                limits,
                totals: Mutex::new(Totals::default()),
                budget: Budget {
                    pools: vec![retention.clone(), principal, process],
                },
                speech_workspace: super::snapshot::WorkspacePool::invocation(),
                cancel,
                started: tokio::time::Instant::now(),
            }),
            retention,
        })
    }
    /// Current charged invocation payload retention.
    pub fn retained_bytes(&self) -> usize {
        self.retention.retained()
    }
    /// Clone the invocation's cancellation token.
    pub fn cancellation(&self) -> CancellationToken {
        self.shared.cancel.clone()
    }
    /// Absolute deadline; downstream work cannot extend it.
    pub fn deadline(&self) -> tokio::time::Instant {
        self.shared.started + self.shared.limits.total_duration
    }
    /// Immutable effective limits.
    pub fn limits(&self) -> &Limits {
        &self.shared.limits
    }
    /// Reject a transport reader from a different invocation before consuming it.
    pub fn owns_reader(&self, reader: &Reader) -> bool {
        self.shared.id == reader.key.invocation
    }
    /// Reject a transport writer from a different invocation before publishing to it.
    pub fn owns_writer(&self, writer: &Writer) -> bool {
        self.shared.id == writer.key.invocation
    }
    /// Open a typed edge; a detachable edge explicitly permits consumer abandonment.
    pub fn open(
        &self,
        direction: Direction,
        format: Format,
        detachable: bool,
    ) -> Result<(Writer, Reader), Error> {
        format.validate()?;
        let index = index(direction);
        let mut totals = self.shared.totals.lock().map_err(|_| Error::Worker)?;
        if totals.opened[index] >= self.shared.limits.streams_per_direction {
            return Err(Error::Limit);
        }
        totals.opened[index] += 1;
        drop(totals);
        let key = StreamKey {
            invocation: self.shared.id,
            stream: Uuid::new_v4(),
            direction,
        };
        let (tx, rx) = mpsc::channel(self.shared.limits.edge_chunks);
        let dropped = CancellationToken::new();
        let aggregate = Arc::new(Mutex::new((usize::MAX, 0usize)));
        Ok((
            Writer {
                aggregate: aggregate.clone(),
                key,
                shared: self.shared.clone(),
                tx,
                edge: Arc::new(Semaphore::new(self.shared.limits.edge_bytes)),
                items: Arc::new(Semaphore::new(self.shared.limits.edge_chunks)),
                format,
                dropped: dropped.clone(),
                detachable,
                closed: false,
                state: TextState::new(
                    format,
                    self.shared.limits.text_bytes,
                    self.shared.limits.segments,
                    self.shared.budget.clone(),
                ),
            },
            Reader {
                aggregate,
                key,
                shared: self.shared.clone(),
                rx,
                dropped,
                closed: false,
                format,
                failure: None,
            },
        ))
    }
}
fn index(direction: Direction) -> usize {
    match direction {
        Direction::Input => 0,
        Direction::Output => 1,
    }
}
fn restrict_edge(aggregate: &Mutex<(usize, usize)>, limit: usize) -> Result<(), Error> {
    let mut aggregate = aggregate.lock().map_err(|_| Error::Worker)?;
    if limit == 0 || aggregate.1 != 0 || limit > aggregate.0 {
        return Err(Error::Invalid);
    }
    aggregate.0 = limit;
    Ok(())
}

/// Object-safe, Send write boundary with explicit close.
pub trait TypedWriter: Send {
    /// The stream's invocation-bound identity.
    fn key(&self) -> StreamKey;
    /// Admit a payload while preserving byte and packet credits through consumption.
    fn write<'a>(&'a mut self, key: StreamKey, chunk: Chunk) -> MediaFuture<'a, ()>;
    /// Close exactly once after all text has been committed.
    fn close(&mut self, key: StreamKey) -> MediaFuture<'_, ()>;
}
/// Object-safe, Send read boundary; dropping is not successful EOF.
pub trait TypedReader: Send {
    /// The stream's invocation-bound identity.
    fn key(&self) -> StreamKey;
    /// None only after an explicit successful close, never channel EOF.
    fn read(&mut self) -> MediaFuture<'_, Option<Packet>>;
}
/// Exclusive typed producer; dropping without close is a broken producer.
pub struct Writer {
    aggregate: Arc<Mutex<(usize, usize)>>,
    key: StreamKey,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Queued>,
    edge: Arc<Semaphore>,
    items: Arc<Semaphore>,
    format: Format,
    dropped: CancellationToken,
    detachable: bool,
    closed: bool,
    state: TextState,
}
impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("key", &self.key)
            .field("format", &self.format)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
struct AdmissionGuard(Option<CancellationToken>);
impl AdmissionGuard {
    fn completed(&mut self) {
        self.0 = None;
    }
}
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}
impl Writer {
    /// Tighten the semantic payload aggregate before transport startup.
    pub fn restrict_bytes(&self, limit: usize) -> Result<(), Error> {
        restrict_edge(&self.aggregate, limit)
    }
    /// The stream's invocation-bound identity.
    pub fn key(&self) -> StreamKey {
        self.key
    }
    /// The immutable concrete format.
    pub fn format(&self) -> Format {
        self.format
    }
    /// The immutable admission direction.
    pub fn direction(&self) -> Direction {
        self.key.direction
    }
    fn check(&self, key: StreamKey) -> Result<(), Error> {
        if key != self.key {
            return Err(Error::Identity);
        }
        if self.closed {
            return Err(Error::Invalid);
        }
        if self.shared.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(())
    }
    async fn send(&self, frame: Frame, size: usize) -> Result<(), Error> {
        if self.detachable && self.dropped.is_cancelled() {
            return Ok(());
        }
        let wait = async {
            let permit = self
                .edge
                .clone()
                .acquire_many_owned(size.max(1) as u32)
                .await
                .map_err(|_| Error::Worker)?;
            let item = self
                .items
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Worker)?;
            let slot = self
                .tx
                .reserve()
                .await
                .map_err(|_| Error::ConsumerDropped)?;
            slot.send(Queued {
                frame,
                edge: permit,
                item,
            });
            Ok(())
        };
        tokio::select! { biased;
            () = self.shared.cancel.cancelled() => Err(Error::Cancelled),
            () = self.dropped.cancelled() => if self.detachable { Ok(()) } else { Err(Error::ConsumerDropped) },
            () = tokio::time::sleep_until(self.phase_deadline()) => Err(Error::Deadline),
            result = tokio::time::timeout(self.shared.limits.queue_wait, wait) => result.map_err(|_| Error::Deadline)?,
        }
    }
    fn phase_deadline(&self) -> tokio::time::Instant {
        self.shared.started
            + match self.key.direction {
                Direction::Input => self.shared.limits.input_duration,
                Direction::Output => self.shared.limits.total_duration,
            }
    }
    /// Validate identity and admit a chunk without silent truncation or conversion.
    /// Dropping a pending admitted write cancels the invocation; aggregate admission
    /// counters do not roll back, and the stream cannot subsequently close successfully.
    pub async fn write(&mut self, key: StreamKey, chunk: Chunk) -> Result<(), Error> {
        self.check(key)?;
        let result = self.write_validated(chunk).await;
        if result.is_err() {
            self.closed = true;
            self.shared.cancel.cancel();
        }
        result
    }
    async fn write_validated(&mut self, chunk: Chunk) -> Result<(), Error> {
        let bytes = chunk.bytes();
        if bytes == 0 || bytes > self.shared.limits.chunk_bytes {
            return Err(Error::Limit);
        }
        let credit = self.shared.budget.reserve(bytes)?;
        self.write_admitted(chunk, credit).await
    }
    async fn copy_packet(&mut self, packet: &Packet) -> Result<(), Error> {
        self.check(self.key)?;
        let bytes = packet.chunk().bytes();
        if bytes == 0 || bytes > self.shared.limits.chunk_bytes {
            return Err(Error::Limit);
        }
        // Host-owned tee copies reserve before allocation, unlike caller-owned writes.
        let credit = self.shared.budget.reserve(bytes)?;
        let result = self.write_admitted(packet.chunk().clone(), credit).await;
        if result.is_err() {
            self.closed = true;
            self.shared.cancel.cancel();
        }
        result
    }
    async fn write_admitted(&mut self, chunk: Chunk, credit: Credit) -> Result<(), Error> {
        // Admission counters are monotonic. Abandonment is terminal, never a retryable
        // publication gap; keep this guard across every state mutation and await.
        let mut admission = AdmissionGuard(Some(self.shared.cancel.clone()));
        let bytes = chunk.bytes();
        {
            let mut aggregate = self.aggregate.lock().map_err(|_| Error::Worker)?;
            let bytes = match &chunk {
                Chunk::Text(text) => text.text.len(),
                _ => bytes,
            };
            aggregate.1 = aggregate
                .1
                .checked_add(bytes)
                .filter(|bytes| *bytes <= aggregate.0)
                .ok_or(Error::Limit)?;
        }
        self.state.accept(&chunk)?;
        {
            let mut totals = self.shared.totals.lock().map_err(|_| Error::Worker)?;
            let index = index(self.key.direction);
            let max = if index == 0 {
                self.shared.limits.input_bytes
            } else {
                self.shared.limits.output_bytes
            };
            let new = totals.bytes[index]
                .checked_add(bytes)
                .filter(|n| *n <= max)
                .ok_or(Error::Limit)?;
            if let (Direction::Input, Format::Pcm16Le { rate, channels }) =
                (self.key.direction, self.format)
            {
                let frames = bytes / (2 * usize::from(channels));
                let mut candidate = totals.audio_frames;
                let slot = candidate
                    .iter_mut()
                    .find(|slot| slot.is_none_or(|(existing, _)| existing == rate))
                    .ok_or(Error::Limit)?;
                *slot = Some((rate, slot.map_or(0, |(_, frames)| frames) + frames as u64));
                // Four finite rates fit exactly in u128; do not round each chunk's duration.
                let (mut numerator, mut denominator) = (0u128, 1u128);
                for (rate, frames) in candidate.iter().flatten() {
                    numerator = numerator * u128::from(*rate)
                        + u128::from(*frames) * 1_000_000_000 * denominator;
                    denominator *= u128::from(*rate);
                }
                if numerator > self.shared.limits.input_duration.as_nanos() * denominator {
                    return Err(Error::Limit);
                }
                totals.audio_frames = candidate;
            }
            totals.bytes[index] = new;
            if index == 1 {
                self.shared
                    .trace
                    .record("output_bytes_admitted", new as u64);
            }
        }
        let result = self
            .send(
                Frame::Data(Packet {
                    chunk,
                    _credit: credit,
                    _edge: None,
                    _item: None,
                }),
                bytes,
            )
            .await;
        if result.is_err() {
            self.closed = true;
            self.shared.cancel.cancel();
        }
        if result.is_ok() {
            admission.completed();
        }
        result
    }
    /// Publish successful EOF exactly once, rejecting missing text commitment.
    /// Dropping a pending close cancels the invocation rather than implying EOF.
    pub async fn close(&mut self, key: StreamKey) -> Result<(), Error> {
        self.check(key)?;
        if self.detachable && self.dropped.is_cancelled() {
            self.closed = true;
            return Ok(());
        }
        let mut admission = AdmissionGuard(Some(self.shared.cancel.clone()));
        if let Err(error) = self.state.close() {
            self.closed = true;
            self.shared.cancel.cancel();
            return Err(error);
        }
        self.closed = true;
        let result = self.send(Frame::Close, 1).await;
        if result.is_err() {
            self.shared.cancel.cancel();
        }
        if result.is_ok() {
            admission.completed();
        }
        result
    }
    /// Publish an explicit terminal failure rather than successful EOF.
    pub async fn fail(&mut self, error: Error) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Invalid);
        }
        let mut admission = AdmissionGuard(Some(self.shared.cancel.clone()));
        self.closed = true;
        let result = self.send(Frame::Failed(error), 1).await;
        if result.is_err() {
            self.shared.cancel.cancel();
        }
        if result.is_ok() {
            admission.completed();
        }
        result
    }
}
impl TypedWriter for Writer {
    fn key(&self) -> StreamKey {
        self.key
    }
    fn write<'a>(&'a mut self, key: StreamKey, chunk: Chunk) -> MediaFuture<'a, ()> {
        Box::pin(self.write(key, chunk))
    }
    fn close(&mut self, key: StreamKey) -> MediaFuture<'_, ()> {
        Box::pin(self.close(key))
    }
}
/// Exclusive consumer; packets retain credits while checked out.
pub struct Reader {
    aggregate: Arc<Mutex<(usize, usize)>>,
    key: StreamKey,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<Queued>,
    dropped: CancellationToken,
    closed: bool,
    format: Format,
    failure: Option<Error>,
}
impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("key", &self.key)
            .field("format", &self.format)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl Reader {
    /// Tighten the semantic payload aggregate before transport startup.
    pub fn restrict_bytes(&self, limit: usize) -> Result<(), Error> {
        restrict_edge(&self.aggregate, limit)
    }
    pub(super) fn invocation(&self) -> Uuid {
        self.shared.id
    }
    pub(super) fn speech_workspace(&self) -> super::snapshot::WorkspacePool {
        self.shared.speech_workspace.clone()
    }
    pub(super) fn snapshot_output(&self) -> Result<OutputQuota, Error> {
        let mut totals = self.shared.totals.lock().map_err(|_| Error::Worker)?;
        if totals.opened[1] >= self.shared.limits.streams_per_direction {
            return Err(Error::Limit);
        }
        totals.opened[1] += 1;
        Ok(OutputQuota(self.shared.clone()))
    }
    pub(super) fn queue_wait(&self) -> Duration {
        self.shared.limits.queue_wait
    }
    /// The stream's invocation-bound identity.
    pub fn key(&self) -> StreamKey {
        self.key
    }
    /// The immutable concrete format.
    pub fn format(&self) -> Format {
        self.format
    }
    /// The immutable admission direction.
    pub fn direction(&self) -> Direction {
        self.key.direction
    }
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.shared.cancel.clone()
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.shared.started + self.shared.limits.total_duration
    }
    pub(crate) fn matches(&self, output: &Writer) -> bool {
        self.key.invocation == output.key.invocation
    }
    pub(crate) fn text_state(&self) -> TextState {
        TextState::new(
            Format::Text,
            self.shared.limits.text_bytes,
            self.shared.limits.segments,
            self.shared.budget.clone(),
        )
    }
    pub(crate) fn workspace(&self) -> Result<impl Send + use<>, Error> {
        self.shared.budget.reserve(4 * 1024 * 1024)
    }
    pub(crate) fn idle_timeout(&self) -> Duration {
        self.shared.limits.idle
    }
    /// Return None only after explicit close; terminal failure remains repeatable.
    pub async fn read(&mut self) -> Result<Option<Packet>, Error> {
        self.read_with_idle(true).await
    }
    /// RPC output may be deferred until input and helper work finish.
    pub(in crate::routine) async fn read_rpc_output(&mut self) -> Result<Option<Packet>, Error> {
        if self.key.direction != Direction::Output {
            return Err(Error::Invalid);
        }
        self.read_with_idle(false).await
    }
    async fn read_with_idle(&mut self, idle: bool) -> Result<Option<Packet>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.read_next(idle).await;
        if let Err(error) = result {
            self.failure = Some(error);
            self.shared.cancel.cancel();
        }
        result
    }
    async fn read_next(&mut self, idle: bool) -> Result<Option<Packet>, Error> {
        if self.closed {
            return Ok(None);
        }
        let received = tokio::select! { biased;
            () = self.shared.cancel.cancelled() => return Err(Error::Cancelled),
            () = tokio::time::sleep_until(self.shared.started + self.shared.limits.total_duration) => return Err(Error::Deadline),
            result = async {
                if idle {
                    tokio::time::timeout(self.shared.limits.idle, self.rx.recv())
                        .await.map_err(|_| Error::Deadline)
                } else {
                    Ok(self.rx.recv().await)
                }
            } => result?,
        };
        let queued = received.ok_or(Error::BrokenProducer)?;
        match queued.frame {
            Frame::Data(mut packet) => {
                packet._edge = Some(queued.edge);
                packet._item = Some(queued.item);
                Ok(Some(packet))
            }
            Frame::Close => {
                self.closed = true;
                Ok(None)
            }
            Frame::Failed(error) => Err(error),
        }
    }
}

pub(super) struct OutputQuota(Arc<Shared>);
impl OutputQuota {
    pub(super) fn admit(&self, bytes: usize) -> Result<(), Error> {
        let mut totals = self.0.totals.lock().map_err(|_| Error::Worker)?;
        totals.bytes[1] = totals.bytes[1]
            .checked_add(bytes)
            .filter(|n| *n <= self.0.limits.output_bytes)
            .ok_or(Error::Limit)?;
        self.0
            .trace
            .record("output_bytes_admitted", totals.bytes[1] as u64);
        Ok(())
    }
}
impl TypedReader for Reader {
    fn key(&self) -> StreamKey {
        self.key
    }
    fn read(&mut self) -> MediaFuture<'_, Option<Packet>> {
        Box::pin(self.read())
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.dropped.cancel();
    }
}

struct Segment {
    text: String,
    committed: bool,
    _credit: Credit,
}
/// Host-owned collected text. Not a JSON value or automatically a helper prompt.
/// Host-owned bounded text reference, never a large JSON string.
pub struct CollectedText {
    state: TextState,
}
impl std::fmt::Debug for CollectedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectedText")
            .field("bytes", &self.state.bytes)
            .finish_non_exhaustive()
    }
}
impl CollectedText {
    /// Borrow committed segments in first-seen order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.state
            .order
            .iter()
            .map(|id| self.state.segments[id].text.as_str())
    }
    /// Retained payload cost, including the state's duplicated segment IDs.
    pub fn bytes(&self) -> usize {
        self.state.bytes
    }
}
pub(crate) struct TextState {
    format: Format,
    segments: BTreeMap<String, Segment>,
    order: Vec<String>,
    bytes: usize,
    max: usize,
    max_segments: usize,
    budget: Budget,
    closed: bool,
}
impl TextState {
    fn new(format: Format, max: usize, max_segments: usize, budget: Budget) -> Self {
        Self {
            format,
            segments: BTreeMap::new(),
            order: Vec::new(),
            bytes: 0,
            max,
            max_segments,
            budget,
            closed: false,
        }
    }
    pub(crate) fn accept(&mut self, chunk: &Chunk) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Invalid);
        }
        match (self.format, chunk) {
            (Format::Pcm16Le { channels, .. }, Chunk::Audio(bytes)) => {
                if bytes.is_empty() || bytes.len() % (2 * usize::from(channels)) != 0 {
                    return Err(Error::Invalid);
                }
            }
            (Format::Text, Chunk::Text(text)) => {
                if text.segment.is_empty() || text.segment.len() > 128 {
                    return Err(Error::Invalid);
                }
                let old = self.segments.get(&text.segment);
                if old.is_some_and(|s| s.committed) {
                    return Err(Error::Uncommitted);
                }
                if old.is_none() && self.segments.len() >= self.max_segments {
                    return Err(Error::Limit);
                }
                let old_len = old.map_or(0, |s| s.text.len());
                let new_len = if text.replace {
                    text.text.len()
                } else {
                    old_len.checked_add(text.text.len()).ok_or(Error::Limit)?
                };
                let id_bytes = if old.is_none() {
                    text.segment.len() * 2
                } else {
                    0
                };
                let total = self
                    .bytes
                    .checked_sub(old_len)
                    .and_then(|n| n.checked_add(new_len))
                    .and_then(|n| n.checked_add(id_bytes))
                    .filter(|n| *n <= self.max)
                    .ok_or(Error::Limit)?;
                // Reserve the replacement before releasing the old allocation: peak copies count.
                let credit = self.budget.reserve(new_len + text.segment.len() * 2)?;
                if old.is_none() {
                    self.order.push(text.segment.clone());
                }
                let value = if text.replace {
                    text.text.clone()
                } else {
                    let mut value = String::with_capacity(new_len);
                    if let Some(old) = self.segments.get(&text.segment) {
                        value.push_str(&old.text);
                    }
                    value.push_str(&text.text);
                    value
                };
                self.segments.insert(
                    text.segment.clone(),
                    Segment {
                        text: value,
                        committed: text.committed,
                        _credit: credit,
                    },
                );
                self.bytes = total;
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
    pub(crate) fn committed_at(&self, at: usize) -> Option<&str> {
        self.segments
            .get(self.order.get(at)?)
            .filter(|s| s.committed)
            .map(|s| s.text.as_str())
    }
    pub(crate) fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Invalid);
        }
        if self.segments.values().any(|s| !s.committed) {
            return Err(Error::Uncommitted);
        }
        self.closed = true;
        Ok(())
    }
}
/// Collect committed text under an explicit cap, such as E5 1 MiB or E6 256 KiB.
pub async fn collect(mut input: Reader, cap: usize) -> Result<CollectedText, Error> {
    if cap == 0 || cap > input.shared.limits.text_bytes {
        return Err(Error::Invalid);
    }
    let cancel = input.cancellation();
    let result = async {
        let mut state = TextState::new(
            Format::Text,
            cap,
            input.shared.limits.segments,
            input.shared.budget.clone(),
        );
        while let Some(packet) = input.read().await? {
            state.accept(packet.chunk())?;
        }
        state.close()?;
        Ok(CollectedText { state })
    }
    .await;
    if result.is_err() {
        cancel.cancel();
    }
    result
}
/// Explicit bounded fan-out. All branches are allocated in advance. A required
/// branch stalls the producer; dropping a detachable branch releases its edge.
pub fn tee(mut input: Reader, mut outputs: Vec<Writer>) -> Result<Producer, Error> {
    if outputs.is_empty() || outputs.len() > input.shared.limits.tee_branches {
        return Err(Error::Limit);
    }
    if outputs
        .iter()
        .any(|output| !input.matches(output) || input.format() != output.format())
    {
        return Err(Error::Invalid);
    }
    let cancel = input.shared.cancel.clone();
    let owner_cancel = cancel.clone();
    Ok(Producer::spawn(cancel, async move {
        let result = async {
            while let Some(packet) = input.read().await? {
                for output in &mut outputs {
                    if output.detachable && output.dropped.is_cancelled() {
                        continue;
                    }
                    output.copy_packet(&packet).await?;
                }
            }
            for output in &mut outputs {
                output.close(output.key()).await?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            owner_cancel.cancel();
        }
        result
    }))
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    fn streams() -> Streams {
        Streams::new(
            Limits {
                edge_chunks: 1,
                ..Limits::default()
            },
            Pool::principal(),
            Pool::process(),
            CancellationToken::new(),
        )
        .unwrap()
    }
    fn text(id: &str) -> Chunk {
        Chunk::Text(Text {
            segment: id.into(),
            text: "x".into(),
            replace: true,
            committed: true,
        })
    }
    #[tokio::test]
    async fn abandoned_committed_write_is_terminal_with_queued_or_checked_out_credit() {
        for checked_out in [false, true] {
            let streams = streams();
            let (mut writer, mut reader) =
                streams.open(Direction::Input, Format::Text, false).unwrap();
            let key = writer.key();
            writer.write(key, text("a")).await.unwrap();
            let packet = if checked_out {
                reader.read().await.unwrap()
            } else {
                None
            };
            assert_eq!(streams.retained_bytes(), 5);
            let mut pending = Box::pin(writer.write(key, text("b")));
            assert!(futures_util::poll!(&mut pending).is_pending());
            assert_eq!(streams.retained_bytes(), 10);
            assert_eq!(streams.shared.totals.lock().unwrap().bytes[0], 4);
            drop(pending);
            assert!(streams.cancellation().is_cancelled());
            assert_eq!(streams.retained_bytes(), 8);
            assert_eq!(writer.items.available_permits(), 0);
            assert_eq!(
                writer.edge.available_permits(),
                streams.limits().edge_bytes - 2
            );
            assert_eq!(writer.close(key).await, Err(Error::Cancelled));
            assert_eq!(writer.write(key, text("b")).await, Err(Error::Cancelled));
            assert_eq!(streams.shared.totals.lock().unwrap().bytes[0], 4);
            assert!(matches!(reader.read().await, Err(Error::Cancelled)));
            assert!(matches!(reader.read().await, Err(Error::Cancelled)));
            drop(packet);
            drop(reader);
            drop(writer);
            assert_eq!(streams.retained_bytes(), 0);
        }
    }
    #[tokio::test]
    async fn abandoned_tee_copy_and_pending_close_are_terminal() {
        let format = Format::Pcm16Le {
            rate: 24000,
            channels: 1,
        };
        let streams = streams();
        assert_eq!(streams.shared.totals.lock().unwrap().bytes[1], 0);
        let (mut input, mut source) = streams.open(Direction::Input, format, false).unwrap();
        input
            .write(input.key(), Chunk::Audio(vec![0; 2]))
            .await
            .unwrap();
        let packet = source.read().await.unwrap().unwrap();
        let (mut writer, mut reader) = streams.open(Direction::Output, format, false).unwrap();
        writer
            .write(writer.key(), Chunk::Audio(vec![0; 2]))
            .await
            .unwrap();
        let mut pending = Box::pin(writer.copy_packet(&packet));
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert_eq!(streams.retained_bytes(), 6);
        drop(pending);
        assert_eq!(streams.retained_bytes(), 4);
        assert_eq!(streams.shared.totals.lock().unwrap().bytes, [2, 4]);
        // The abandoned copy was admitted, not delivered to the reader.
        assert!(matches!(reader.read().await, Err(Error::Cancelled)));
        drop(packet);
        drop(reader);
        drop(writer);
        drop(input);
        drop(source);
        assert_eq!(streams.retained_bytes(), 0);

        let streams = self::streams();
        let (mut writer, mut reader) = streams.open(Direction::Input, format, false).unwrap();
        writer
            .write(writer.key(), Chunk::Audio(vec![0; 2]))
            .await
            .unwrap();
        let mut pending = Box::pin(writer.close(writer.key()));
        assert!(futures_util::poll!(&mut pending).is_pending());
        drop(pending);
        assert!(streams.cancellation().is_cancelled());
        assert_eq!(streams.shared.totals.lock().unwrap().bytes[0], 2);
        assert_eq!(streams.retained_bytes(), 2);
        assert!(matches!(reader.read().await, Err(Error::Cancelled)));
        drop(reader);
        drop(writer);
        assert_eq!(streams.retained_bytes(), 0);
    }
}
