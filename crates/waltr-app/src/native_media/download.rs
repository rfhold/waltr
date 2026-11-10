//! One retained owner holds checkout and reader authority independently of IPC waits.
use super::*;
use std::cell::Cell;

#[cfg(not(test))]
const CLEANUP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const CLEANUP_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
type Receipt = futures_util::future::Shared<futures_util::future::LocalBoxFuture<'static, bool>>;
type Permit = Option<tokio::sync::OwnedSemaphorePermit>;
struct Read {
    reply: oneshot::Sender<Result<Chunk, String>>,
    permit: Permit,
}
pub(super) struct Chunk {
    pub bytes: Vec<u8>,
    pub permit: Permit,
}
#[derive(Clone)]
pub(super) struct Owner {
    cancel: CancellationToken,
    ready: Receipt,
    done: Receipt,
    requests: tokio::sync::mpsc::Sender<Read>,
    deadline: Rc<Cell<Option<tokio::time::Instant>>>,
    fence: Fence,
    expires: std::time::Instant,
}
struct Waiter(Owner, bool);
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.1 {
            self.0.stop();
        }
    }
}
impl Owner {
    #[cfg(test)]
    pub fn request_capacity(&self) -> usize {
        self.requests.capacity()
    }
    pub fn stop(&self) {
        if self.deadline.get().is_none() {
            self.deadline
                .set(Some(tokio::time::Instant::now() + CLEANUP_WAIT));
        }
        self.cancel.cancel();
    }
    fn valid(&self) -> bool {
        !self.cancel.is_cancelled()
            && self.fence.valid()
            && std::time::Instant::now() < self.expires
    }
    pub async fn wait_closed(&self) -> bool {
        self.stop();
        if let Some(clean) = self.done.clone().now_or_never() {
            return clean;
        }
        tokio::time::timeout_at(
            self.deadline
                .get()
                .unwrap_or_else(tokio::time::Instant::now),
            self.done.clone(),
        )
        .await
        .unwrap_or(false)
    }
    pub async fn wait_ready(&self) -> Result<(), String> {
        let mut waiter = Waiter(self.clone(), true);
        loop {
            tokio::select! { biased;
                () = self.cancel.cancelled() => return Err(failure()),
                () = self.fence.cancel.cancelled() => return Err(failure()),
                () = tokio::time::sleep_until(self.expires.into()) => return Err(failure()),
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => if !self.valid() { return Err(failure()); },
                ready = self.ready.clone() => {
                    if !ready || !self.valid() { return Err(failure()); }
                    waiter.1 = false;
                    return Ok(());
                }
            }
        }
    }
    pub async fn read(&self, permit: Permit) -> Result<Chunk, String> {
        if !self.valid() {
            return Err(failure());
        }
        let mut waiter = Waiter(self.clone(), true);
        let (reply, result) = oneshot::channel();
        self.requests
            .try_send(Read { reply, permit })
            .map_err(|_| failure())?;
        tokio::pin!(result);
        loop {
            tokio::select! { biased;
                () = self.cancel.cancelled() => return Err(failure()),
                () = self.fence.cancel.cancelled() => return Err(failure()),
                () = tokio::time::sleep_until(self.expires.into()) => return Err(failure()),
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => if !self.valid() { return Err(failure()); },
                result = &mut result => {
                    let chunk = result.map_err(|_| failure())??;
                    if !self.valid() || chunk.bytes.len() > CHUNK { return Err(failure()); }
                    waiter.1 = false;
                    return Ok(chunk);
                }
            }
        }
    }
}
pub(super) struct Start {
    pub transfers: interaction::MediaTransfers,
    pub grant: interaction::BinaryOutputGrant,
    pub fence: Fence,
    pub expires: std::time::Instant,
    pub activity: crate::activity::ActivityLease,
    pub permit: Permit,
}
pub(super) fn start(start: Start, tasks: &mut tokio::task::JoinSet<()>) -> Owner {
    // Reap completed handles, including panics. Their receipts independently remain false.
    while tasks.try_join_next().is_some() {}
    let (ready_send, ready) = oneshot::channel();
    let (done_send, done) = oneshot::channel();
    let (requests, receiver) = tokio::sync::mpsc::channel(1);
    let owner = Owner {
        cancel: CancellationToken::new(),
        ready: ready.map(|r| r.unwrap_or(false)).boxed_local().shared(),
        done: done.map(|r| r.unwrap_or(false)).boxed_local().shared(),
        requests,
        deadline: Rc::new(Cell::new(None)),
        fence: start.fence.clone(),
        expires: start.expires,
    };
    let driver = owner.clone();
    tasks.spawn_local(async move {
        drive(start, driver, receiver, ready_send, done_send).await;
    });
    owner
}
async fn drive(
    start: Start,
    owner: Owner,
    mut requests: tokio::sync::mpsc::Receiver<Read>,
    ready: oneshot::Sender<bool>,
    done: oneshot::Sender<bool>,
) {
    let _permit = start.permit;
    let _activity = start.activity;
    // Deliberately do not drop this future on cancellation. A late reader must be closed.
    let Ok(mut reader) = start.transfers.read_output(start.grant).await else {
        let _sent = ready.send(false);
        let _sent = done.send(false);
        return;
    };
    let admitted = owner.valid();
    let observed = ready.send(admitted).is_ok();
    let mut pending_permit = None;
    if admitted && observed {
        loop {
            let read = tokio::select! { biased;
                () = owner.cancel.cancelled() => break,
                () = owner.fence.cancel.cancelled() => break,
                () = tokio::time::sleep_until(owner.expires.into()) => break,
                () = tokio::time::sleep(std::time::Duration::from_millis(2)) => { if !owner.valid() { break; } continue; },
                read = requests.recv() => match read { Some(read) => read, None => break },
            };
            pending_permit = read.permit;
            let result = read_chunk(&mut reader, &owner).await;
            match result {
                Ok(bytes) => {
                    if let Err(Ok(chunk)) = read.reply.send(Ok(Chunk {
                        bytes,
                        permit: pending_permit.take(),
                    })) {
                        pending_permit = chunk.permit;
                        break;
                    }
                }
                Err(error) => {
                    let _sent = read.reply.send(Err(error));
                    break;
                }
            }
        }
    }
    owner.stop();
    // The owner, not a request waiter, retains this close future and its authority.
    let clean = reader.close().await.is_ok();
    let _sent = done.send(clean);
    if !clean {
        // No automatic retries or false join claim. Keep uncertain authority and credits.
        std::future::pending::<()>().await;
        drop((reader, pending_permit));
    }
}
async fn read_chunk(
    reader: &mut interaction::BlobReader,
    owner: &Owner,
) -> Result<Vec<u8>, String> {
    loop {
        let chunk = tokio::select! { biased;
            () = owner.cancel.cancelled() => return Err(failure()),
            () = owner.fence.cancel.cancelled() => return Err(failure()),
            () = tokio::time::sleep_until(owner.expires.into()) => return Err(failure()),
            () = tokio::time::sleep(std::time::Duration::from_millis(2)) => { if !owner.valid() { return Err(failure()); } continue; },
            chunk = reader.next() => chunk.transpose().map_err(|_| failure())?,
        };
        if !owner.valid() {
            return Err(failure());
        }
        match chunk {
            Some(bytes) if bytes.is_empty() => continue,
            Some(bytes) if bytes.len() <= CHUNK => return Ok(bytes),
            None if reader.complete() => return Ok(Vec::new()),
            _ => return Err(failure()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn raw_checkout_owner_panic_is_unverified_and_actual_task_is_joined() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let audio = Arc::new(crate::native_audio::Host::new(
                    crate::activity::ActivityTracker::default(),
                ));
                {
                    let mut state = audio.lock();
                    state.bind(1);
                    state.access(true);
                }
                let credentials = NativeCredentialProvider::default();
                let fence = Fence {
                    generation: 1,
                    epoch: 0,
                    current: None,
                    audio,
                    revision: credentials.credential_revision(),
                    credentials,
                    cancel: CancellationToken::new(),
                };
                let (ready_send, ready) = oneshot::channel::<bool>();
                let (done_send, done) = oneshot::channel::<bool>();
                let (requests, _receiver) = tokio::sync::mpsc::channel(1);
                let owner = Owner {
                    cancel: CancellationToken::new(),
                    ready: ready.map(|r| r.unwrap_or(false)).boxed_local().shared(),
                    done: done.map(|r| r.unwrap_or(false)).boxed_local().shared(),
                    requests,
                    deadline: Rc::new(Cell::new(None)),
                    fence,
                    expires: std::time::Instant::now() + std::time::Duration::from_secs(60),
                };
                let credits = Arc::new(tokio::sync::Semaphore::new(1));
                let permit = credits.clone().try_acquire_owned().unwrap();
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn_local(async move {
                    let _owned = (ready_send, done_send, permit);
                    panic!("synthetic raw owner panic");
                });
                assert_eq!(tasks.len(), 1);
                assert!(owner.wait_ready().await.is_err());
                assert!(!owner.wait_closed().await);
                assert!(tasks.join_next().await.unwrap().unwrap_err().is_panic());
                assert!(tasks.is_empty());
                assert_eq!(credits.available_permits(), 1);
            })
            .await;
    }
}
