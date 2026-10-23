use super::*;
use interaction::{Chunk, Error, Input};
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

struct Frame {
    chunk: Chunk,
    _bytes: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
}

pub(super) struct Ingress {
    senders: BTreeMap<String, mpsc::Sender<Frame>>,
    pub tasks: JoinSet<Result<(), Error>>,
    bytes: Arc<Semaphore>,
    slots: Arc<Semaphore>,
    cancel: CancellationToken,
    failure: Arc<std::sync::Mutex<Option<Error>>>,
}

impl Ingress {
    pub fn new(
        inputs: BTreeMap<String, Input>,
        limits: &InteractionLimits,
        parent: &CancellationToken,
        complete: bool,
    ) -> Self {
        let cancel = parent.child_token();
        let mut tasks = JoinSet::new();
        let mut senders = BTreeMap::new();
        let failure = Arc::new(std::sync::Mutex::new(None));
        for (port, mut input) in inputs {
            let (tx, mut rx) = mpsc::channel::<Frame>(limits.queued_chunks);
            senders.insert(port, tx);
            let token = cancel.clone();
            let failed = failure.clone();
            tasks.spawn(async move {
                let result = async { loop {
                    tokio::select! { biased;
                        () = token.cancelled() => return Ok(()),
                        frame = rx.recv() => match frame {
                            Some(frame) => {
                                let send = async {
                                    if complete { input.send_complete(frame.chunk).await } else { input.send(frame.chunk).await }
                                };
                                tokio::select! { biased;
                                    () = token.cancelled() => return Ok(()),
                                    result = send => result?,
                                }
                                // Credits include the in-flight send, not just buffered channel entries.
                                drop((frame._bytes, frame._slot));
                            }
                            None => return tokio::select! { biased;
                                () = token.cancelled() => Ok(()),
                                result = input.close() => result,
                            },
                        }
                    }
                } }.await;
                if let Err(error) = &result { *failed.lock().expect("input driver failure") = Some(error.clone()); }
                result
            });
        }
        Self {
            senders,
            tasks,
            bytes: Arc::new(Semaphore::new(limits.edge_bytes)),
            slots: Arc::new(Semaphore::new(limits.queued_chunks)),
            cancel,
            failure,
        }
    }

    pub async fn send(
        &self,
        port: &str,
        chunk: Chunk,
        limits: &InteractionLimits,
        input_deadline: tokio::time::Instant,
    ) -> Result<(), Error> {
        let size = match &chunk {
            Chunk::Audio(data) => data.len(),
            Chunk::Text(text) => text.text.len().saturating_add(text.segment.len()),
        };
        if size > limits.chunk_bytes || size > limits.edge_bytes {
            return Err(Error::new(
                "resource_exhausted",
                "input chunk exceeds queue byte limit",
            ));
        }
        let sender = self
            .senders
            .get(port)
            .ok_or_else(|| Error::new("invalid_stream", "input already closed"))?;
        let deadline = (tokio::time::Instant::now()
            + Duration::from_secs(limits.queue_wait_seconds))
        .min(input_deadline);
        tokio::select! { biased;
            () = self.cancel.cancelled() => Ok(()),
            () = tokio::time::sleep_until(deadline) => Err(Error::new(if deadline == input_deadline { "deadline_exceeded" } else { "resource_exhausted" }, "interaction ingress queue wait expired")),
            result = async {
                let slot = self.slots.clone().acquire_owned().await.map_err(|_| Error::new("cancelled", "input queue closed"))?;
                let bytes = self.bytes.clone().acquire_many_owned(size as u32).await.map_err(|_| Error::new("cancelled", "input queue closed"))?;
                sender.try_send(Frame { chunk, _bytes: bytes, _slot: slot }).map_err(|_| self.failure.lock().expect("input driver failure").clone().unwrap_or_else(|| Error::new("stream_closed", "input dispatcher closed")))
            } => result,
        }
    }

    pub fn owns_failure(&self, error: &Error) -> bool {
        self.failure
            .lock()
            .expect("input driver failure")
            .as_ref()
            .is_some_and(|failure| failure.code == error.code && failure.message == error.message)
    }

    pub fn close(&mut self, port: &str) {
        self.senders.remove(port);
    }
    pub fn close_remaining(&mut self) {
        self.senders.clear();
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        self.senders.clear();
        while self.tasks.join_next().await.is_some() {}
    }
}
