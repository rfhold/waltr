//! Host-owned authoritative text snapshots. Revisions replace, never replay, prior
//! state; readers borrow <=64 KiB UTF-8 chunks without copying the whole note.
//! The separate workspace is reserved before worker creation and retained by all
//! checked-out snapshots. It is not RSS: tonic buffers and allocator metadata are excluded.
use super::{Error, stream::Reader};
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MIB: usize = 1024 * 1024;
/// Shared additional speech-workspace pool, separate from stream payload pools.
#[derive(Clone, Debug)]
pub struct WorkspacePool {
    semaphore: Arc<Semaphore>,
    capacity: usize,
}
impl WorkspacePool {
    pub(super) fn invocation() -> Self {
        Self::new(16 * MIB)
    }
    /// Share this 32 MiB pool across every invocation of one principal.
    pub fn principal() -> Self {
        Self::new(32 * MIB)
    }
    /// Share this 64 MiB pool across every principal in the process.
    pub fn process() -> Self {
        Self::new(64 * MIB)
    }
    fn new(capacity: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }
    /// Reserved workspace bytes, not actual RSS.
    pub fn retained(&self) -> usize {
        self.capacity - self.semaphore.available_permits()
    }
}
/// One invocation's additional 16 MiB workspace. Reconstructed handles share the
/// same invocation credits; host-provided principal/process pools must also be shared.
#[derive(Clone, Debug)]
pub struct Workspace {
    invocation: Uuid,
    pools: [WorkspacePool; 3],
}
impl Workspace {
    /// Bind shared pools to the input invocation, without allocating media storage.
    pub fn new(input: &Reader, principal: WorkspacePool, process: WorkspacePool) -> Self {
        Self {
            invocation: input.invocation(),
            pools: [input.speech_workspace(), principal, process],
        }
    }
    pub(super) fn reserve(&self, input: &Reader) -> Result<Arc<Lease>, Error> {
        if self.invocation != input.invocation() {
            return Err(Error::Identity);
        }
        let mut permits = Vec::with_capacity(3);
        for pool in &self.pools {
            permits.push(
                pool.semaphore
                    .clone()
                    .try_acquire_many_owned((16 * MIB) as u32)
                    .map_err(|_| Error::Limit)?,
            );
        }
        Ok(Arc::new(Lease {
            _permits: permits,
            slots: Arc::new(Semaphore::new(3)),
        }))
    }
    /// Invocation reservation bytes (at most 16 MiB).
    pub fn retained(&self) -> usize {
        self.pools[0].retained()
    }
}
#[derive(Debug)]
pub(super) struct Lease {
    _permits: Vec<OwnedSemaphorePermit>,
    slots: Arc<Semaphore>,
}
#[derive(Debug)]
struct Data {
    text: String,
    _slot: OwnedSemaphorePermit,
    _lease: Arc<Lease>,
}
/// A pinned replacement revision. Clones share its charged text, not a String copy.
#[derive(Clone)]
pub struct Revision {
    data: Arc<Data>,
    generation: u64,
    committed: bool,
}
impl std::fmt::Debug for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Revision")
            .field("generation", &self.generation)
            .field("committed", &self.committed)
            .field("bytes", &self.bytes())
            .finish()
    }
}
impl Revision {
    /// Monotonic revision identity; the commit shares the latest generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// True only after explicit input close, provider EOF and owned worker joins.
    pub fn committed(&self) -> bool {
        self.committed
    }
    /// Formatted text bytes; <=1 MiB, including speaker labels.
    pub fn bytes(&self) -> usize {
        self.data.text.len()
    }
    /// Borrow bounded UTF-8 chunks from this pinned revision, without JSON or replay.
    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        let mut rest = self.data.text.as_str();
        std::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let mut end = rest.len().min(65536);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, tail) = rest.split_at(end);
            rest = tail;
            Some(chunk)
        })
    }
}
enum Frame {
    Revision(Revision),
    Close,
}
/// Required consumer of authoritative revisions; dropping it cancels the invocation.
pub struct SnapshotReader {
    receiver: mpsc::Receiver<Frame>,
    cancel: CancellationToken,
    deadline: tokio::time::Instant,
    closed: bool,
}
impl std::fmt::Debug for SnapshotReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotReader")
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl SnapshotReader {
    /// Receive a replacement reference. Only an explicit joined close yields EOF.
    /// Abandoning a pending receive is harmless; dropping the reader is terminal.
    /// Word attribution can arrive only after silent provider finalization, so
    /// revision waits use cancellation and the original absolute deadline, not idle.
    pub async fn next_revision(&mut self) -> Result<Option<Revision>, Error> {
        if self.closed {
            return Ok(None);
        }
        let result = tokio::select! { biased;
            () = self.cancel.cancelled() => Err(Error::Cancelled),
            value = tokio::time::timeout_at(self.deadline, self.receiver.recv()) => match value {
                Ok(Some(Frame::Revision(revision))) => Ok(Some(revision)),
                Ok(Some(Frame::Close)) => { self.closed = true; Ok(None) },
                Ok(None) => Err(Error::BrokenProducer),
                Err(_) => Err(Error::Deadline),
            }
        };
        if result.is_err() {
            self.cancel.cancel();
        }
        result
    }
    /// Keep only the latest reference and require exactly one final commit. The
    /// configurable note cap is checked before any whole-note copy (none is made).
    pub async fn collect(mut self, cap: usize) -> Result<Revision, Error> {
        if cap == 0 || cap > MIB {
            self.cancel.cancel();
            return Err(Error::Invalid);
        }
        let mut latest = None;
        let mut committed = false;
        while let Some(revision) = self.next_revision().await? {
            if committed || revision.bytes() > cap {
                self.cancel.cancel();
                return Err(Error::Limit);
            }
            // Walking the real bounded reader chunks checks the same path a helper
            // will consume, retaining only one authoritative reference.
            if revision.chunks().map(str::len).sum::<usize>() != revision.bytes() {
                self.cancel.cancel();
                return Err(Error::Invalid);
            }
            committed = revision.committed();
            latest = Some(revision);
        }
        if !committed {
            return Err(Error::Uncommitted);
        }
        latest.ok_or(Error::Uncommitted)
    }
}
impl Drop for SnapshotReader {
    fn drop(&mut self) {
        if !self.closed {
            self.cancel.cancel();
        }
    }
}
pub(super) struct Publisher {
    sender: mpsc::Sender<Frame>,
    cancel: CancellationToken,
    wait: Duration,
    deadline: tokio::time::Instant,
    lease: Arc<Lease>,
    generation: u64,
    quota: super::stream::OutputQuota,
    closed: bool,
}
pub(super) fn channel(
    input: &Reader,
    lease: Arc<Lease>,
) -> Result<(Publisher, SnapshotReader), Error> {
    let quota = input.snapshot_output()?;
    let (sender, receiver) = mpsc::channel(1);
    let cancel = input.cancellation();
    let deadline = input.deadline();
    Ok((
        Publisher {
            sender,
            cancel: cancel.clone(),
            wait: input.queue_wait(),
            deadline,
            lease,
            generation: 0,
            quota,
            closed: false,
        },
        SnapshotReader {
            receiver,
            cancel,
            deadline,
            closed: false,
        },
    ))
}
struct Pending(CancellationToken, bool);
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.1 {
            self.0.cancel();
        }
    }
}
impl Publisher {
    pub(super) async fn format(
        &mut self,
        alternative: waltr_audio::compact_stt::Alternative<'_>,
    ) -> Result<Option<Revision>, Error> {
        let mut words = alternative.words().peekable();
        if words.peek().is_none() {
            return Ok(None);
        }
        let mut guard = Pending(self.cancel.clone(), false);
        let slot = tokio::select! { biased;
            () = self.cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout_at(self.deadline.min(tokio::time::Instant::now() + self.wait), self.lease.slots.clone().acquire_owned()) => result.map_err(|_| Error::Deadline)?.map_err(|_| Error::Limit)?,
        };
        let mut text = String::with_capacity(MIB);
        let mut previous = None;
        for (i, word) in words.enumerate() {
            if i % 4096 == 0
                && (self.cancel.is_cancelled() || tokio::time::Instant::now() >= self.deadline)
            {
                return Err(Error::Cancelled);
            }
            let speaker = if word.speaker().trim().is_empty() {
                "unknown"
            } else {
                word.speaker()
            };
            if speaker.chars().any(char::is_control) {
                return Err(Error::Invalid);
            }
            let extra = word.word().len()
                + 1
                + if previous != Some(speaker) {
                    speaker.len() + 1 + usize::from(!text.is_empty())
                } else {
                    0
                };
            if text.len().checked_add(extra).is_none_or(|n| n > MIB) {
                return Err(Error::Limit);
            }
            if previous != Some(speaker) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(speaker);
                text.push(':');
                previous = Some(speaker);
            }
            text.push(' ');
            text.push_str(word.word());
        }
        self.generation = self.generation.checked_add(1).ok_or(Error::Limit)?;
        let revision = Revision {
            data: Arc::new(Data {
                text,
                _slot: slot,
                _lease: self.lease.clone(),
            }),
            generation: self.generation,
            committed: false,
        };
        guard.1 = true;
        Ok(Some(revision))
    }
    pub(super) async fn publish(&mut self, revision: Revision) -> Result<(), Error> {
        let mut guard = Pending(self.cancel.clone(), false);
        // The final commit republishes only identity/state, not another text copy.
        self.quota.admit(if revision.committed {
            0
        } else {
            revision.bytes()
        })?;
        tokio::select! { biased;
            () = self.cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout_at(self.deadline.min(tokio::time::Instant::now() + self.wait), self.sender.send(Frame::Revision(revision))) => result.map_err(|_| Error::Deadline)?.map_err(|_| Error::ConsumerDropped)?,
        }
        guard.1 = true;
        Ok(())
    }
    pub(super) async fn finish(&mut self, mut revision: Revision) -> Result<(), Error> {
        revision.committed = true;
        self.publish(revision).await?;
        let mut guard = Pending(self.cancel.clone(), false);
        tokio::select! { biased;
            () = self.cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout_at(self.deadline.min(tokio::time::Instant::now() + self.wait), self.sender.send(Frame::Close)) => result.map_err(|_| Error::Deadline)?.map_err(|_| Error::ConsumerDropped)?,
        }
        self.closed = true;
        guard.1 = true;
        Ok(())
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        if !self.closed {
            self.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::media::stream::{Direction, Format, Limits, Pool, Streams};

    #[tokio::test]
    async fn reconstructed_workspace_cannot_replenish_invocation_credits() {
        let streams = Streams::new(
            Limits::default(),
            Pool::principal(),
            Pool::process(),
            CancellationToken::new(),
        )
        .unwrap();
        let (_writer, input) = streams.open(Direction::Input, Format::Text, false).unwrap();
        let original = Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
        let lease = original.reserve(&input).unwrap();
        let principal = WorkspacePool::principal();
        let process = WorkspacePool::process();
        let reconstructed = Workspace::new(&input, principal.clone(), process.clone());
        assert!(matches!(reconstructed.reserve(&input), Err(Error::Limit)));
        assert_eq!(reconstructed.retained(), 16 * MIB);
        assert_eq!(principal.retained(), 0);
        assert_eq!(process.retained(), 0);
        drop(lease);
        let recovered = reconstructed.reserve(&input).unwrap();
        assert_eq!(original.retained(), 16 * MIB);
        drop(recovered);
        assert_eq!(original.retained(), 0);
        assert_eq!(principal.retained(), 0);
        assert_eq!(process.retained(), 0);
    }

    #[tokio::test]
    async fn abandoned_snapshot_publication_cancels_and_preserves_shared_output_quota() {
        let streams = Streams::new(
            Limits {
                output_bytes: 4,
                ..Limits::default()
            },
            Pool::principal(),
            Pool::process(),
            CancellationToken::new(),
        )
        .unwrap();
        let (_writer, input) = streams.open(Direction::Input, Format::Text, false).unwrap();
        let workspace =
            Workspace::new(&input, WorkspacePool::principal(), WorkspacePool::process());
        let lease = workspace.reserve(&input).unwrap();
        let (mut publication, mut reader) = channel(&input, lease.clone()).unwrap();
        let make = |generation| Revision {
            data: Arc::new(Data {
                text: "aa".into(),
                _slot: lease.slots.clone().try_acquire_owned().unwrap(),
                _lease: lease.clone(),
            }),
            generation,
            committed: false,
        };
        publication.publish(make(1)).await.unwrap();
        let mut pending = Box::pin(publication.publish(make(2)));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        drop(pending);
        assert!(streams.cancellation().is_cancelled());
        assert!(reader.next_revision().await.is_err());
        // Both admitted snapshots consume the same invocation output quota, even
        // though the second publication was abandoned and can never be retried.
        assert_eq!(publication.quota.admit(1), Err(Error::Limit));
        drop(publication);
        drop(reader);
        drop(lease);
        assert_eq!(workspace.retained(), 0);
    }
}
