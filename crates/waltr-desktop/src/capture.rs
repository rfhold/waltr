//! Owned blocking capture bridge. Retain and await `CaptureWorker::join` after cancellation.
//! Native initialization and teardown have no portable deadline. Dropping the future is not shutdown.
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::{collections::VecDeque, sync::Mutex};
use tokio::sync::Notify;
use waltr_audio_io::{Capture, CaptureOutput, Terminal};

/// Maximum queued PCM duration, independent of the RPC admission deadline.
pub const BACKLOG_SECONDS: usize = 30;

struct Lane {
    bytes: VecDeque<u8>,
    closed: bool,
    receiver_closed: bool,
    cancelled: bool,
}
struct PcmQueue {
    lane: Mutex<Lane>,
    ready: Notify,
    capacity: usize,
    chunk: usize,
}
impl PcmQueue {
    fn send(&self, frame: &[u8]) -> Result<(), Error> {
        let mut lane = self
            .lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lane.cancelled {
            return Err(Error::Cancelled);
        }
        if lane.receiver_closed {
            return Err(Error::ReceiverClosed);
        }
        if frame.len() > self.capacity - lane.bytes.len() {
            return Err(Error::Overflow);
        }
        lane.bytes.extend(frame);
        drop(lane);
        self.ready.notify_one();
        Ok(())
    }
    fn close(&self) {
        self.lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.ready.notify_one();
    }
}

/// Byte-bounded PCM FIFO. Frame boundaries may coalesce; sample order is unchanged.
/// A single ring avoids per-frame metadata growth for two-byte callback fragments.
pub struct Frames(Arc<PcmQueue>);
impl std::fmt::Debug for Frames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frames").finish_non_exhaustive()
    }
}
impl Frames {
    /// Read the next frame, preserving source order.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut lane = self
                    .0
                    .lane
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !lane.bytes.is_empty() {
                    let count = lane.bytes.len().min(self.0.chunk);
                    return Some(lane.bytes.drain(..count).collect());
                }
                if lane.closed {
                    return None;
                }
            }
            ready.await;
        }
    }
}
impl Drop for Frames {
    fn drop(&mut self) {
        let mut lane = self
            .0
            .lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lane.receiver_closed = true;
        lane.bytes.clear();
    }
}

/// Static, sample-free capture errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Unsupported configuration, before device access.
    Unsupported,
    /// Actual input format exceeds the one-second native/transient audio reserve.
    PreacceptBudgetExceeded,
    /// Backend initialization or streaming failure.
    Backend,
    /// Native callback exhausted its bounded queue.
    CallbackOverflow,
    /// Native callback had no free pool block.
    CallbackPoolExhausted,
    /// Native raw callback queue was saturated.
    CallbackQueueFull,
    /// Native sample conversion rejected input.
    Conversion,
    /// Native duration or per-call resource bound was exceeded.
    ResourceLimit,
    /// Native worker panicked or could not be joined.
    Worker,
    /// Host PCM queue exhausted its byte credit.
    Overflow,
    /// Source frame exceeds the configured chunk bound or is not PCM16 aligned.
    MalformedFrame,
    /// Host PCM receiver closed unexpectedly.
    ReceiverClosed,
    /// Explicit cancellation.
    Cancelled,
}
/// Injectable blocking source. Construction, polling, and destruction occur on one blocking worker.
pub trait Source: Send {
    /// Read one ready PCM chunk.
    fn frame(&mut self) -> Option<Vec<u8>>;
    /// Read independent terminal state.
    fn terminal(&self) -> Terminal;
    /// Request normal flush.
    fn stop(&self);
    /// Discard and join native work before return.
    fn shutdown(self: Box<Self>);
}
/// Injectable device factory; never invoked on the local actor thread.
pub trait Factory: Send + Sync + 'static {
    /// Open a source on the blocking worker.
    /// # Errors
    /// Returns a bounded backend or configuration failure.
    fn open(&self, config: waltr_audio_io::Config) -> Result<Box<dyn Source>, Error>;
}
/// Production CPAL factory. This value itself does not access devices.
#[derive(Debug)]
pub struct Native;
struct NativeSource {
    capture: Capture,
    output: CaptureOutput,
}
impl Source for NativeSource {
    fn frame(&mut self) -> Option<Vec<u8>> {
        self.output.try_frame()
    }
    fn terminal(&self) -> Terminal {
        self.output.terminal()
    }
    fn stop(&self) {
        self.capture.control().stop();
    }
    fn shutdown(self: Box<Self>) {
        let _result = self.capture.shutdown();
    }
}
impl Factory for Native {
    fn open(&self, config: waltr_audio_io::Config) -> Result<Box<dyn Source>, Error> {
        let (capture, output) = Capture::start(config).map_err(native_error)?;
        Ok(Box::new(NativeSource { capture, output }))
    }
}
fn native_error(error: waltr_audio_io::Error) -> Error {
    use waltr_audio_io::Error as E;
    match error {
        E::InvalidConfig | E::UnsupportedFormat | E::UnsupportedDeviceSelection => {
            Error::Unsupported
        }
        E::PreacceptBudgetExceeded => Error::PreacceptBudgetExceeded,
        E::CallbackOverflow => Error::CallbackOverflow,
        E::CallbackPoolExhausted => Error::CallbackPoolExhausted,
        E::CallbackQueueFull => Error::CallbackQueueFull,
        E::NonFiniteSample | E::MisalignedInput => Error::Conversion,
        E::Limit => Error::ResourceLimit,
        E::Closed => Error::Cancelled,
        E::WorkerPanicked => Error::Worker,
        E::Backend(_) | E::DeviceNotFound | E::AmbiguousDevice => Error::Backend,
    }
}
/// Validate the native subset without device enumeration or permission prompts.
/// # Errors
/// Rejects stable IDs, stereo, and rates above 48 kHz without fallback.
pub fn native_config(audio: &crate::config::Audio) -> Result<waltr_audio_io::Config, Error> {
    if !(8_000..=48_000).contains(&audio.sample_rate_hz) || audio.channels != 1 {
        return Err(Error::Unsupported);
    }
    let device = match &audio.device {
        crate::config::DeviceSelector::Default => waltr_audio_io::DeviceSelection::Default,
        crate::config::DeviceSelector::Name(name) => {
            waltr_audio_io::DeviceSelection::Name(name.clone())
        }
        crate::config::DeviceSelector::Id(_) => return Err(Error::Unsupported),
    };
    let mut config = waltr_audio_io::Config {
        device,
        target_rate: audio.sample_rate_hz,
        target_channels: audio.channels,
        ..Default::default()
    };
    // Actual-format native preflight includes raw/history/transients, before stream construction.
    // The separate 30-second host ring is added to this one-second native reserve.
    // Include one worker-pending chunk and one consumer frame in the native reserve.
    // RPC PCM is empty before Accepted; its post-admission queue is budgeted separately.
    config.limits.raw_queue_items = 8;
    config.limits.preaccept_external_chunks = Some(2);
    let budget = audio.sample_rate_hz as usize * 2;
    config.limits.output_queue_bytes = budget / 4;
    config.limits.output_queue_items = 4;
    config.limits.max_chunk_bytes = (budget / 32).max(2) / 2 * 2;
    config.validate().map_err(|_| Error::Unsupported)?;
    Ok(config)
}
/// Owned bridge. The host must cancel then await join, including during device initialization.
pub struct CaptureWorker {
    control: Arc<AtomicU8>,
    /// Finite PCM lane; never expose its contents in status snapshots.
    pub frames: Frames,
    worker: tokio::task::JoinHandle<Result<(), Error>>,
    result: Arc<std::sync::Mutex<Option<Result<(), Error>>>>,
    joined: bool,
    started: Arc<Mutex<Option<std::time::Instant>>>,
}
impl std::fmt::Debug for CaptureWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureWorker")
            .field("joined", &self.joined)
            .field("finished", &self.finished())
            .finish_non_exhaustive()
    }
}
impl CaptureWorker {
    /// Start promptly without blocking the caller. No hardware access occurs before worker execution.
    pub fn start(factory: Arc<dyn Factory>, config: waltr_audio_io::Config) -> Self {
        Self::start_observed(factory, config, crate::host::Observer::default())
    }
    /// Start with a bounded observer. Ready means factory open returned, not a callback timestamp.
    pub fn start_observed(
        factory: Arc<dyn Factory>,
        config: waltr_audio_io::Config,
        observer: crate::host::Observer,
    ) -> Self {
        let control = Arc::new(AtomicU8::new(0));
        let state = control.clone();
        // Bound setup even for configurations that the worker subsequently rejects.
        let bytes = (config.target_rate as usize).clamp(8_000, 48_000) * 2 * BACKLOG_SECONDS;
        let tx = Arc::new(PcmQueue {
            lane: Mutex::new(Lane {
                bytes: VecDeque::with_capacity(bytes),
                closed: false,
                receiver_closed: false,
                cancelled: false,
            }),
            ready: Notify::new(),
            capacity: bytes,
            chunk: config.limits.max_chunk_bytes.clamp(2, 256 * 1024) / 2 * 2,
        });
        let frames = tx.clone();
        let result = Arc::new(std::sync::Mutex::new(None));
        let terminal = result.clone();
        let started = Arc::new(Mutex::new(None));
        let ready = started.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let run = || {
                config.validate().map_err(|_| Error::Unsupported)?;
                let max_chunk = config.limits.max_chunk_bytes;
                let mut source = factory.open(config)?;
                *ready
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(std::time::Instant::now());
                observer.emit(crate::host::Diagnostic::Stage(
                    crate::host::Stage::CaptureReady,
                ));
                let send = |frame: Vec<u8>| {
                    if frame.len() > max_chunk || !frame.len().is_multiple_of(2) {
                        return Err(Error::MalformedFrame);
                    }
                    if frame.is_empty() {
                        return Ok(());
                    }
                    tx.send(&frame)
                };
                let result = (|| {
                    loop {
                        match state.load(Ordering::Acquire) {
                            2 => return Err(Error::Cancelled),
                            1 => source.stop(),
                            _ => {}
                        }
                        while let Some(frame) = source.frame() {
                            match state.load(Ordering::Acquire) {
                                2 => return Err(Error::Cancelled),
                                1 => source.stop(),
                                _ => {}
                            }
                            send(frame)?;
                        }
                        match source.terminal() {
                            Terminal::Running => {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Terminal::Completed => {
                                while let Some(frame) = source.frame() {
                                    if state.load(Ordering::Acquire) == 2 {
                                        return Err(Error::Cancelled);
                                    }
                                    send(frame)?;
                                }
                                return Ok(());
                            }
                            Terminal::Cancelled => return Err(Error::Cancelled),
                            Terminal::Failed(error) => return Err(native_error(error)),
                        }
                    }
                })();
                source.shutdown();
                result
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
                .unwrap_or(Err(Error::Worker));
            *terminal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
            tx.close();
            outcome
        });
        Self {
            control,
            frames: Frames(frames),
            worker,
            result,
            joined: false,
            started,
        }
    }
    /// Request flush, including if initialization has not yet returned.
    pub fn stop(&self) {
        let _previous = self
            .control
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
    /// Invalidate capture, including pending initialization.
    pub fn cancel(&self) {
        self.control.store(2, Ordering::Release);
        let mut lane = self
            .frames
            .0
            .lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lane.bytes.clear();
        lane.cancelled = true;
        lane.closed = true;
        drop(lane);
        self.frames.0.ready.notify_one();
    }
    /// Await all native teardown. OS calls have no promised hard deadline.
    /// # Errors
    /// Returns bounded failure without samples or device names.
    pub async fn join(&mut self) -> Result<(), Error> {
        if self.joined {
            return self.check_failure();
        }
        let result = (&mut self.worker).await.unwrap_or(Err(Error::Worker));
        self.joined = true;
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        result
    }
    /// Whether native startup/teardown has returned.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.worker.is_finished()
    }
    /// Whether shutdown was acknowledged.
    #[must_use]
    pub const fn joined(&self) -> bool {
        self.joined
    }
    /// Inspect the sample-free failure lane without joining.
    /// # Errors
    /// Returns the worker's terminal failure, if present.
    pub fn check_failure(&self) -> Result<(), Error> {
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(Ok(()))
    }
    pub(crate) fn started_lane(&self) -> Arc<Mutex<Option<std::time::Instant>>> {
        self.started.clone()
    }
    pub(crate) fn stop_lane(&self) -> Arc<AtomicU8> {
        self.control.clone()
    }
    pub(crate) fn failure_lane(&self) -> Arc<std::sync::Mutex<Option<Result<(), Error>>>> {
        self.result.clone()
    }
}
impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    struct ClosedSource;
    impl Source for ClosedSource {
        fn frame(&mut self) -> Option<Vec<u8>> {
            Some(vec![0, 0])
        }
        fn terminal(&self) -> Terminal {
            Terminal::Running
        }
        fn stop(&self) {}
        fn shutdown(self: Box<Self>) {}
    }
    struct Deferred(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);
    impl Factory for Deferred {
        fn open(&self, _: waltr_audio_io::Config) -> Result<Box<dyn Source>, Error> {
            self.0.lock().unwrap().recv().unwrap();
            Ok(Box::new(ClosedSource))
        }
    }
    #[tokio::test]
    async fn receiver_closure_is_not_byte_exhaustion() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut worker = CaptureWorker::start(
            Arc::new(Deferred(std::sync::Mutex::new(rx))),
            waltr_audio_io::Config::default(),
        );
        worker.frames.0.lane.lock().unwrap().receiver_closed = true;
        tx.send(()).unwrap();
        assert_eq!(worker.join().await, Err(Error::ReceiverClosed));
    }
}
