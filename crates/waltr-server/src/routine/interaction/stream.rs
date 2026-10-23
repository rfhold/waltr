use super::Error;
use crate::resource_catalog::routine::{InteractionLimits, RoutineStreamFormat};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// One revision of a named segment. First appearance determines segment order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Text {
    pub segment: String,
    pub text: String,
    pub replace: bool,
    pub committed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Chunk {
    Audio(Vec<u8>),
    Text(Text),
}
impl Chunk {
    pub(super) fn bytes(&self) -> usize {
        match self {
            Self::Audio(v) => v.len(),
            Self::Text(v) => v.text.len().saturating_add(v.segment.len()),
        }
    }
}
#[derive(Debug, Clone)]
pub(super) enum Frame {
    Chunk(Chunk),
    Close,
    Failed(Error),
}
impl Frame {
    fn bytes(&self) -> usize {
        match self {
            Self::Chunk(v) => v.bytes().max(1),
            Self::Close | Self::Failed(_) => 1,
        }
    }
}
/// A replacement cancels under the same lock used by the final synchronous enqueue.
#[derive(Clone)]
pub(super) struct Fence {
    pub(super) token: CancellationToken,
    pub(super) gate: Arc<Mutex<()>>,
}
struct Queued {
    frame: Frame,
    fence: Option<Fence>,
    _bytes: OwnedSemaphorePermit,
}
#[derive(Clone)]
pub(crate) struct Sender {
    tx: mpsc::Sender<Queued>,
    bytes: Arc<Semaphore>,
    max: usize,
    queue_wait: std::time::Duration,
    consumer: Option<CancellationToken>,
}
pub(crate) struct Receiver {
    rx: mpsc::Receiver<Queued>,
    detachment: Option<CancellationToken>,
}
pub(super) fn edge(limits: &InteractionLimits) -> (Sender, Receiver) {
    let (tx, rx) = mpsc::channel(limits.queued_chunks);
    (
        Sender {
            tx,
            bytes: Arc::new(Semaphore::new(limits.edge_bytes)),
            max: limits.chunk_bytes,
            queue_wait: std::time::Duration::from_secs(limits.queue_wait_seconds),
            consumer: None,
        },
        Receiver {
            rx,
            detachment: None,
        },
    )
}
impl Sender {
    pub(super) fn for_consumer(mut self, token: CancellationToken) -> Self {
        self.consumer = Some(token);
        self
    }
    pub(super) async fn send(&self, frame: Frame, cancel: &CancellationToken) -> Result<(), Error> {
        self.send_fenced(frame, cancel, None).await
    }
    pub(super) async fn send_fenced(
        &self,
        frame: Frame,
        cancel: &CancellationToken,
        fence: Option<&Fence>,
    ) -> Result<(), Error> {
        let bytes = frame.bytes();
        if bytes > self.max {
            return Err(Error::new(
                "resource_exhausted",
                "stream chunk exceeds byte limit",
            ));
        }
        let Some((slot, permit)) = capacity(
            &self.tx,
            &self.bytes,
            bytes,
            cancel,
            self.consumer.as_ref(),
            self.queue_wait,
        )
        .await?
        else {
            return Ok(());
        };
        let _publication = fence.map(|f| f.gate.lock().expect("publication gate"));
        if cancel.is_cancelled() || fence.is_some_and(|f| f.token.is_cancelled()) {
            return Err(Error::cancelled());
        }
        if self
            .consumer
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Ok(());
        }
        slot.send(Queued {
            frame,
            fence: fence.cloned(),
            _bytes: permit,
        });
        Ok(())
    }
}
/// Acquire byte credit before a channel slot, under one absolute enqueue deadline.
/// Cancellation, detachment, timeout and dropped futures release every temporary reservation.
pub(super) async fn capacity<'a, T>(
    tx: &'a mpsc::Sender<T>,
    bytes: &Arc<Semaphore>,
    size: usize,
    cancel: &CancellationToken,
    consumer: Option<&CancellationToken>,
    queue_wait: std::time::Duration,
) -> Result<Option<(mpsc::Permit<'a, T>, OwnedSemaphorePermit)>, Error> {
    let size = u32::try_from(size)
        .map_err(|_| Error::new("resource_exhausted", "chunk size is not representable"))?;
    let deadline = tokio::time::Instant::now()
        .checked_add(queue_wait)
        .ok_or_else(|| Error::new("resource_exhausted", "queue wait is not representable"))?;
    let acquire = async {
        let permit = bytes
            .clone()
            .acquire_many_owned(size)
            .await
            .map_err(|_| Error::new("stream_closed", "stream byte budget closed"))?;
        let slot = tx
            .reserve()
            .await
            .map_err(|_| Error::new("stream_closed", "stream consumer closed"))?;
        Ok(Some((slot, permit)))
    };
    tokio::select! { biased;
        () = cancel.cancelled() => Err(Error::cancelled()),
        () = async { if let Some(token) = consumer { token.cancelled().await; } else { std::future::pending::<()>().await; } } => Ok(None),
        () = tokio::time::sleep_until(deadline) => Err(Error::new("resource_exhausted", "bounded stream queue wait expired")),
        result = acquire => result,
    }
}

impl Receiver {
    pub(super) fn with_detachment(mut self, token: CancellationToken) -> Self {
        self.detachment = Some(token);
        self
    }
    pub(super) fn detach(self) {
        if let Some(token) = &self.detachment {
            token.cancel();
        }
    }
    pub(super) async fn recv(&mut self, cancel: &CancellationToken) -> Result<Frame, Error> {
        let (frame, fence) = self.recv_fenced(cancel).await?;
        if fence.is_some_and(|f| f.token.is_cancelled()) {
            return Err(Error::new(
                "task_interrupted",
                "stream generation was replaced",
            ));
        }
        match frame {
            Frame::Failed(error) => Err(error),
            frame => Ok(frame),
        }
    }
    pub(super) async fn recv_fenced(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<(Frame, Option<Fence>), Error> {
        tokio::select! { biased;
            () = cancel.cancelled() => Err(Error::cancelled()),
            value = self.rx.recv() => value.map(|v| (v.frame, v.fence)).ok_or_else(|| Error::new("stream_failed", "producer ended without successful close")),
        }
    }
}

/// Bounded revision state: replacements cannot mutate committed text.
pub(super) struct State {
    format: RoutineStreamFormat,
    segments: BTreeMap<String, (String, bool)>,
    order: Vec<String>,
    bytes: usize,
    max: usize,
    closed: bool,
}
impl State {
    pub(super) fn new(format: RoutineStreamFormat, max: usize) -> Self {
        Self {
            format,
            segments: BTreeMap::new(),
            order: Vec::new(),
            bytes: 0,
            max,
            closed: false,
        }
    }
    pub(super) fn accept(&mut self, chunk: &Chunk) -> Result<(), Error> {
        if self.closed {
            return Err(Error::new("invalid_stream", "chunk after stream close"));
        }
        match (&self.format, chunk) {
            (RoutineStreamFormat::Audio { channels, .. }, Chunk::Audio(bytes)) => {
                if bytes.is_empty() || bytes.len() % (2 * *channels as usize) != 0 {
                    return Err(Error::new(
                        "invalid_format",
                        "PCM chunk is not frame aligned",
                    ));
                }
            }
            (RoutineStreamFormat::Text, Chunk::Text(text)) => {
                if text.segment.is_empty() || text.segment.len() > 128 {
                    return Err(Error::new("invalid_stream", "invalid segment identity"));
                }
                let old = self.segments.get(&text.segment);
                if old.is_some_and(|v| v.1) {
                    return Err(Error::new(
                        "invalid_stream",
                        "revision after segment commitment",
                    ));
                }
                let old_len = old.map_or(0, |v| v.0.len());
                let new_len = if text.replace {
                    text.text.len()
                } else {
                    old_len.saturating_add(text.text.len())
                };
                let bytes = self
                    .bytes
                    .saturating_sub(old_len)
                    .saturating_add(new_len)
                    .saturating_add(if old.is_none() { text.segment.len() } else { 0 });
                if bytes > self.max {
                    return Err(Error::new(
                        "resource_exhausted",
                        "text revision state exceeds byte limit",
                    ));
                }
                if old.is_none() {
                    self.order.push(text.segment.clone());
                }
                let entry = self.segments.entry(text.segment.clone()).or_default();
                if text.replace {
                    entry.0.clone_from(&text.text);
                } else {
                    entry.0.push_str(&text.text);
                }
                entry.1 = text.committed;
                self.bytes = bytes;
            }
            _ => {
                return Err(Error::new(
                    "invalid_format",
                    "chunk does not match stream format",
                ));
            }
        }
        Ok(())
    }
    pub(super) fn committed_at(&self, index: usize) -> Option<&str> {
        self.segments
            .get(self.order.get(index)?)
            .filter(|v| v.1)
            .map(|v| v.0.as_str())
    }
    pub(super) fn close(&mut self) -> Result<(), Error> {
        if self.closed || self.segments.values().any(|v| !v.1) {
            return Err(Error::new(
                "invalid_stream",
                "duplicate close or uncommitted text at close",
            ));
        }
        self.closed = true;
        Ok(())
    }
    pub(super) fn collect(&self, separator: &str, max: usize) -> Result<String, Error> {
        if !self.closed {
            return Err(Error::new(
                "invalid_stream",
                "collector requires successful close",
            ));
        }
        let size = self
            .order
            .iter()
            .map(|key| self.segments[key].0.len())
            .sum::<usize>()
            .saturating_add(
                separator
                    .len()
                    .saturating_mul(self.order.len().saturating_sub(1)),
            );
        if size > max {
            return Err(Error::new(
                "resource_exhausted",
                "collector exceeds byte limit",
            ));
        }
        Ok(self
            .order
            .iter()
            .map(|key| self.segments[key].0.as_str())
            .collect::<Vec<_>>()
            .join(separator))
    }
}
