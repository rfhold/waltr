use crate::{Config, Converter, Error, InputFormat, Samples};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

pub const STOP: u8 = 1;
const CANCEL: u8 = 2;
pub const OVERFLOW: u8 = 3;
pub const BACKEND: u8 = 4;
pub const POOL_EXHAUSTED: u8 = 5;
pub const RAW_QUEUE_FULL: u8 = 6;

/// Terminal status has a separate lane and is never queued behind audio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminal {
    /// Capture is live, stopping, or draining.
    Running,
    /// Normal stop and flush completed; queued frames remain readable.
    Completed,
    /// Audio was discarded.
    Cancelled,
    /// Capture failed; queued audio must be discarded.
    Failed(Error),
}

/// Cloneable control lane independent of the audio reader.
#[derive(Clone, Debug)]
pub struct CaptureControl {
    pub(crate) command: Arc<AtomicU8>,
    terminal: Arc<Mutex<Terminal>>,
    owner: Arc<OnceLock<thread::Thread>>,
    #[cfg(test)]
    pub(crate) output_backpressured: Arc<std::sync::atomic::AtomicBool>,
}
impl CaptureControl {
    fn wake(&self) {
        if let Some(owner) = self.owner.get() {
            owner.unpark();
        }
    }
    /// Stop the device and flush accepted audio. Output must continue to be drained.
    pub fn stop(&self) {
        let _ignored = self
            .command
            .compare_exchange(0, STOP, Ordering::AcqRel, Ordering::Acquire);
        self.wake();
    }
    /// Discard queued audio and interrupt output backpressure. Idempotent.
    pub fn cancel(&self) {
        self.command.store(CANCEL, Ordering::Release);
        self.wake();
    }
    /// Snapshot the independent terminal lane.
    #[must_use]
    pub fn terminal(&self) -> Terminal {
        self.terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Independently pollable output. Dropping this reader cancels capture.
/// Poll terminal separately: errors cannot be hidden behind a full audio queue.
pub struct CaptureOutput {
    receiver: Receiver<Vec<u8>>,
    control: CaptureControl,
}
impl CaptureOutput {
    /// Nonblocking; `None` means no frame is ready (consult terminal to distinguish EOF).
    #[must_use]
    pub fn try_frame(&self) -> Option<Vec<u8>> {
        if self.control.command.load(Ordering::Acquire) == CANCEL
            || matches!(
                self.control.terminal(),
                Terminal::Cancelled | Terminal::Failed(_)
            )
        {
            return None;
        }
        let frame = self.receiver.try_recv().ok();
        if frame.is_some() {
            self.control.wake();
        }
        frame
    }
    /// Snapshot the independent terminal lane.
    #[must_use]
    pub fn terminal(&self) -> Terminal {
        self.control.terminal()
    }
}
impl Drop for CaptureOutput {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

/// Owns the native owner thread. Keep this alive while draining output.
///
/// Drop and shutdown cancel then JOIN, never detach. Run these operations on a host blocking
/// worker, not a UI/event thread: backend initialization/destruction has no portable time bound.
/// Library waits are notification-driven and interruptible; OS driver calls are not time-bounded.
/// Android's fixed default-input path does not access ndk-context; host permission and
/// foreground policy remain mandatory.
pub struct Capture {
    control: CaptureControl,
    worker: Option<JoinHandle<()>>,
}
impl Capture {
    #[cfg(feature = "native")]
    /// Initialize the device on its owner thread; call from a host blocking worker.
    ///
    /// # Errors
    /// Returns configuration, selection, unsupported-format, or native initialization errors.
    pub fn start(config: Config) -> Result<(Self, CaptureOutput), Error> {
        Self::spawn(config, crate::native::open)
    }
    /// Clone the independent stop/cancel controller.
    #[must_use]
    pub fn control(&self) -> CaptureControl {
        self.control.clone()
    }
    /// Cancel outstanding work, join the owner, and report its final status.
    /// For normal completion call stop, wait for Completed while draining, then drain all
    /// remaining frames before shutdown (shutdown cancels unread output).
    ///
    /// # Errors
    /// Returns `WorkerPanicked` if the owner panicked.
    pub fn shutdown(mut self) -> Result<Terminal, Error> {
        self.control.cancel();
        self.worker
            .take()
            .ok_or(Error::Closed)?
            .join()
            .map_err(|_| Error::WorkerPanicked)?;
        Ok(self.control.terminal())
    }

    pub(crate) fn spawn<B, F>(config: Config, factory: F) -> Result<(Self, CaptureOutput), Error>
    where
        B: Backend + 'static,
        F: FnOnce(&Config, Arc<AtomicU8>) -> Result<B, Error> + Send + 'static,
    {
        config.validate()?;
        let control = CaptureControl {
            command: Arc::new(AtomicU8::new(0)),
            terminal: Arc::new(Mutex::new(Terminal::Running)),
            owner: Arc::new(OnceLock::new()),
            #[cfg(test)]
            output_backpressured: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (slots, chunk_bytes) = config.output_layout();
        let (sender, receiver) = mpsc::sync_channel(slots);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_control = control.clone();
        let worker = thread::Builder::new()
            .name("waltr-audio-capture".into())
            .spawn(move || {
                let _already_set = thread_control.owner.set(thread::current());
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut backend = match factory(&config, thread_control.command.clone()) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ignored = ready_tx.send(Err(e.clone()));
                            return Err(e);
                        }
                    };
                    let converter = match backend
                        .take_converter()
                        .map_or_else(|| Converter::new(config.clone(), backend.format()), Ok)
                    {
                        Ok(c) => c,
                        Err(e) => {
                            let _ignored = ready_tx.send(Err(e.clone()));
                            return Err(e);
                        }
                    };
                    if let Err(error) = backend.start() {
                        let _ignored = ready_tx.send(Err(error.clone()));
                        return Err(error);
                    }
                    let _ignored = ready_tx.send(Ok(()));
                    run_backend(backend, converter, &sender, chunk_bytes, &thread_control)
                }))
                .unwrap_or(Err(Error::WorkerPanicked));
                let terminal = match result {
                    Ok(t) => t,
                    Err(Error::Closed) => Terminal::Cancelled,
                    Err(e) => Terminal::Failed(e),
                };
                *thread_control.terminal.lock().expect("terminal poisoned") = terminal;
            })
            .map_err(|e| Error::Backend(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok((
                Self {
                    control: control.clone(),
                    worker: Some(worker),
                },
                CaptureOutput { receiver, control },
            )),
            Ok(Err(error)) => {
                let _ignored = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ignored = worker.join();
                Err(Error::WorkerPanicked)
            }
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.control.cancel();
        if let Some(worker) = self.worker.take() {
            let _ignored = worker.join();
        }
    }
}

pub enum Block {
    F32(Vec<f32>),
    I16(Vec<i16>),
    U16(Vec<u16>),
}
impl Block {
    fn samples(&self) -> Samples<'_> {
        match self {
            Self::F32(v) => Samples::F32(v),
            Self::I16(v) => Samples::I16(v),
            Self::U16(v) => Samples::U16(v),
        }
    }
    #[cfg(feature = "native")]
    pub fn clear(&mut self) {
        match self {
            Self::F32(v) => v.clear(),
            Self::I16(v) => v.clear(),
            Self::U16(v) => v.clear(),
        }
    }
}

pub trait Backend {
    /// Native backends prepare conversion before stream construction can invoke callbacks.
    fn take_converter(&mut self) -> Option<Converter> {
        None
    }
    fn start(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn format(&self) -> InputFormat;
    fn next(&mut self) -> Option<Block>;
    fn recycle(&mut self, block: Block);
    /// Quiesce callbacks before returning; accepted blocks must remain drainable.
    fn stop(&mut self);
}

#[allow(clippy::too_many_arguments)] // Carries the owner and its output lane without moving either.
fn send_audio<B: Backend>(
    bytes: &[u8],
    sender: &SyncSender<Vec<u8>>,
    chunk: usize,
    control: &CaptureControl,
    backend: &mut B,
    stopped: &mut bool,
) -> Result<(), Error> {
    for bytes in bytes.chunks(chunk) {
        let mut frame = bytes.to_vec();
        loop {
            let command = control.command.load(Ordering::Acquire);
            if command != 0 && !*stopped {
                backend.stop();
                *stopped = true;
            }
            match control.command.load(Ordering::Acquire) {
                CANCEL => return Err(Error::Closed),
                OVERFLOW => return Err(Error::CallbackOverflow),
                POOL_EXHAUSTED => return Err(Error::CallbackPoolExhausted),
                RAW_QUEUE_FULL => return Err(Error::CallbackQueueFull),
                BACKEND => return Err(Error::Backend("native stream failed".into())),
                _ => {}
            }
            match sender.try_send(frame) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => return Err(Error::Closed),
                Err(TrySendError::Full(value)) => {
                    #[cfg(test)]
                    control.output_backpressured.store(true, Ordering::Release);
                    frame = value;
                    thread::park();
                }
            }
        }
    }
    Ok(())
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("terminal", &self.control.terminal())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for CaptureOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureOutput")
            .field("terminal", &self.control.terminal())
            .finish_non_exhaustive()
    }
}

fn run_backend<B: Backend>(
    mut backend: B,
    mut converter: Converter,
    sender: &SyncSender<Vec<u8>>,
    chunk_bytes: usize,
    control: &CaptureControl,
) -> Result<Terminal, Error> {
    let mut stopped = false;
    loop {
        let command = control.command.load(Ordering::Acquire);
        if command != 0 && !stopped {
            backend.stop();
            stopped = true;
        }
        // stop() can wait for an admitted callback that reports overflow during teardown.
        match control.command.load(Ordering::Acquire) {
            CANCEL => {
                converter.cancel();
                return Ok(Terminal::Cancelled);
            }
            OVERFLOW => return Err(Error::CallbackOverflow),
            POOL_EXHAUSTED => return Err(Error::CallbackPoolExhausted),
            RAW_QUEUE_FULL => return Err(Error::CallbackQueueFull),
            BACKEND => return Err(Error::Backend("native stream failed".into())),
            _ => {}
        }
        if let Some(block) = backend.next() {
            let wire_bytes = converter.push(block.samples());
            backend.recycle(block);
            send_audio(
                &wire_bytes?,
                sender,
                chunk_bytes,
                control,
                &mut backend,
                &mut stopped,
            )?;
        } else if stopped {
            send_audio(
                &converter.finish()?,
                sender,
                chunk_bytes,
                control,
                &mut backend,
                &mut stopped,
            )?;
            return match control.command.load(Ordering::Acquire) {
                CANCEL => Ok(Terminal::Cancelled),
                OVERFLOW => Err(Error::CallbackOverflow),
                POOL_EXHAUSTED => Err(Error::CallbackPoolExhausted),
                RAW_QUEUE_FULL => Err(Error::CallbackQueueFull),
                BACKEND => Err(Error::Backend("native stream failed".into())),
                _ => Ok(Terminal::Completed),
            };
        } else if control.command.load(Ordering::Acquire) == 0 {
            // unpark retains a token if data/control arrives between the check and park.
            thread::park();
        }
    }
}
