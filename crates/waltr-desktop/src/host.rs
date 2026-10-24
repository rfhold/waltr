//! Local-future hold pipeline. Pin a transport/profile lease at press; cancel its generation
//! token before any server/profile/access transition. Retain and await every `Hold::run` future.
use crate::{capture, config::Keybind};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use waltr_client::RoutineInteractionTransport;

/// Fixed milestones; first frame means observed by the host, not callback time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Capture worker requested.
    CaptureRequested,
    /// Factory returned a source on the blocking worker.
    CaptureReady,
    /// First frame dequeued by the host.
    FirstFrameObserved,
    /// Interaction admission requested.
    RpcOpen,
    /// Server acknowledged admission.
    Accepted,
    /// Input finish acknowledged.
    InputFinished,
    /// Output completed.
    OutputFinished,
    /// External action requested.
    Action,
    /// Owned teardown is being awaited.
    Cleanup,
}
/// Content-free observer notifications. No raw errors or configuration cross this seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    /// Clean capture ended without a nonempty PCM frame; no interaction was opened.
    NoInput,
    /// A pipeline milestone.
    Stage(Stage),
    /// Primary pipeline result, independent of cleanup.
    Primary(Result<(), Error>),
    /// Native capture cleanup result.
    CaptureCleanup(Result<(), capture::Error>),
    /// Typing cleanup result.
    TypingCleanup(Result<(), crate::typing::Error>),
    /// Interaction failure boundary (the public error remains unchanged).
    RpcFailure(RpcFailure),
}
/// Existing interaction errors do not preserve backend/protocol details at every boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcFailure {
    /// Opening the RPC failed.
    Open,
    /// The independent RPC-open/Accepted deadline elapsed.
    AcceptanceTimeout,
    /// Sending input failed.
    Input,
    /// Reading output failed.
    Output,
    /// Required terminal output was absent or invalid.
    Protocol,
    /// Validated remote terminal failure.
    RemoteFailed,
    /// Validated remote terminal interruption.
    RemoteInterrupted,
    /// Remote admission rejection.
    Rejected,
    /// Typed client access failure; no authentication details are retained.
    LoginRequired,
}
impl RpcFailure {
    /// Classify selected-output failure without retaining remote diagnostics.
    #[must_use]
    pub const fn from_output(failure: waltr_client::interaction::InteractionOutputFailure) -> Self {
        use waltr_client::interaction::InteractionOutputFailure as F;
        match failure {
            F::RemoteFailed => Self::RemoteFailed,
            F::RemoteInterrupted => Self::RemoteInterrupted,
            F::Rejected => Self::Rejected,
            F::Protocol => Self::Protocol,
        }
    }
    /// Preserve the client's only typed access category, otherwise retain the observed boundary.
    #[must_use]
    pub const fn from_client(error: &waltr_client::ClientError, boundary: Self) -> Self {
        if error.is_waltr_login_required() {
            Self::LoginRequired
        } else {
            boundary
        }
    }
}
/// Optional synchronous observer; callbacks must be bounded and nonblocking.
#[derive(Clone, Default)]
pub struct Observer(Option<Arc<dyn Fn(Diagnostic) + Send + Sync>>);
impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observer").finish_non_exhaustive()
    }
}
impl Observer {
    /// Install a content-free callback. Panics are ignored to keep diagnostics fail-open.
    pub fn new(callback: impl Fn(Diagnostic) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(callback)))
    }
    /// Notify without changing pipeline results.
    pub fn emit(&self, event: Diagnostic) {
        if let Some(callback) = &self.0 {
            let _result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event)));
        }
    }
}

/// One shared admission gate for desktop holds and future native chat capture.
#[derive(Clone, Debug)]
pub struct AudioAdmission(Arc<Semaphore>);
impl Default for AudioAdmission {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(1)))
    }
}
impl AudioAdmission {
    /// Reserve one operation without a queue. Repeats and other presses return Busy.
    /// # Errors
    /// Returns Busy while any operation owns the gate.
    pub fn reserve(&self) -> Result<OwnedSemaphorePermit, Error> {
        self.0.clone().try_acquire_owned().map_err(|_| Error::Busy)
    }
}
/// Bounded, secret-free operation outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shared microphone admission is occupied.
    Busy,
    /// Configuration exceeds the native subset.
    Unsupported,
    /// Explicit cancel, access loss, or stale generation.
    Cancelled,
    /// Capture initialization, queue credit, or teardown failed.
    Capture(capture::Error),
    /// Interaction failed, interrupted, or returned incomplete output.
    Interaction,
    /// Local command failed, including its explicit final-text bound.
    Command(crate::command::CommandError),
    /// Bounded live-text delivery failure.
    Typing(crate::typing::Error),
}
/// Independent stop/cancel lane; key-up must call release even during pending startup.
#[derive(Clone, Debug)]
pub struct HoldControl {
    release: CancellationToken,
    cancel: CancellationToken,
    status: tokio::sync::watch::Sender<Status>,
}
/// Finite status snapshot; no samples, output text, or command values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No operation owns the microphone.
    Idle,
    /// Admission reserved; capture starts before the first nonempty frame admits RPC.
    Starting,
    /// RPC accepted; audio input and output advance concurrently.
    Streaming,
    /// Local command owns its child process.
    Command,
    /// All cleanup acknowledged with a terminal result.
    Finished(Result<(), Error>),
}
impl HoldControl {
    /// Subscribe to the latest bounded status, without an event backlog.
    #[must_use]
    pub fn status(&self) -> tokio::sync::watch::Receiver<Status> {
        self.status.subscribe()
    }
    /// Normal key-up: flush input, then wait for final output.
    pub fn release(&self) {
        self.release.cancel();
    }
    /// Cancel and suppress command; host must still await the operation.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
/// One operation owns admission until capture, RPC, and command cleanup all finish.
#[derive(Debug)]
pub struct Hold {
    _permit: OwnedSemaphorePermit,
    control: HoldControl,
    observer: Observer,
}
impl Hold {
    /// Attach optional diagnostics without altering existing callers.
    #[must_use]
    pub fn observed(mut self, observer: Observer) -> Self {
        self.observer = observer;
        self
    }
    pub(crate) fn use_status(&mut self, status: tokio::sync::watch::Sender<Status>) {
        self.control.status = status;
        self.control.status.send_replace(Status::Starting);
    }
    /// Reserve admission synchronously at press. The host ignores repeats for its current binding.
    /// # Errors
    /// Returns Busy without a queued capture.
    pub fn press(gate: &AudioAdmission) -> Result<(Self, HoldControl), Error> {
        let control = HoldControl {
            release: CancellationToken::new(),
            cancel: CancellationToken::new(),
            status: tokio::sync::watch::channel(Status::Starting).0,
        };
        Ok((
            Self {
                _permit: gate.reserve()?,
                control: control.clone(),
                observer: Observer::default(),
            },
            control,
        ))
    }
    /// Execute against an already pinned transport and generation lease. No Send bound is imposed.
    /// The caller must cancel and await this future before host disposal; dropping it is not a join.
    /// # Errors
    /// Returns static errors without audio, final text, device names, or command contents.
    #[allow(clippy::future_not_send, reason = "native transports may be Rc-owned")]
    pub async fn run<T: RoutineInteractionTransport>(
        self,
        binding: &Keybind,
        transport: &T,
        factory: Arc<dyn capture::Factory>,
        generation: &CancellationToken,
    ) -> Result<(), Error> {
        self.run_guarded(binding, transport, factory, generation, &|| true)
            .await
    }
    /// Execute with a synchronous host intent fence at capture, RPC, and command admission.
    /// The guard must check the pinned identity and credentials without awaiting a monitor.
    /// # Errors
    /// Returns Cancelled when the guard rejects an effect; cleanup is still awaited.
    #[allow(clippy::future_not_send, reason = "native transports may be Rc-owned")]
    #[allow(
        clippy::too_many_arguments,
        reason = "additive guard preserves the existing run call shape"
    )]
    pub async fn run_guarded<T: RoutineInteractionTransport>(
        self,
        binding: &Keybind,
        transport: &T,
        factory: Arc<dyn capture::Factory>,
        generation: &CancellationToken,
        valid: &dyn Fn() -> bool,
    ) -> Result<(), Error> {
        self.run_guarded_with_typing(binding, transport, factory, generation, valid, None)
            .await
    }
    /// Execute with an optional local-capable typing backend. Command bindings ignore it.
    /// The backend owns no effect until admitted by the supplied intent fence.
    /// Cleanup is always awaited before this hold releases admission.
    /// # Errors
    /// Typing without an installed factory returns Unsupported, never a command fallback.
    #[allow(
        clippy::future_not_send,
        reason = "native transports and typing may be Rc-owned"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "additive backend injection preserves existing callers"
    )]
    pub async fn run_guarded_with_typing<T: RoutineInteractionTransport>(
        self,
        binding: &Keybind,
        transport: &T,
        factory: Arc<dyn capture::Factory>,
        generation: &CancellationToken,
        valid: &dyn Fn() -> bool,
        typing_factory: Option<&dyn crate::typing::Factory>,
    ) -> Result<(), Error> {
        let setup = match (&binding.command, &binding.typing) {
            (Some(_), None) => Ok(None),
            (None, Some(_)) => typing_factory
                .ok_or(Error::Unsupported)
                .and_then(|factory| factory.create().map(Some).map_err(Error::Typing)),
            _ => Err(Error::Unsupported),
        };
        let mut sink = match setup {
            Ok(sink) => sink,
            Err(error) => {
                self.control
                    .status
                    .send_replace(Status::Finished(Err(error)));
                return Err(error);
            }
        };
        let result = self
            .execute(binding, transport, factory, generation, valid, &mut sink)
            .await;
        self.observer.emit(Diagnostic::Primary(result));
        self.observer.emit(Diagnostic::Stage(Stage::Cleanup));
        let cleanup = if let Some(sink) = &mut sink {
            let cleanup = sink.close().await;
            self.observer.emit(Diagnostic::TypingCleanup(cleanup));
            cleanup.map_err(Error::Typing)
        } else {
            Ok(())
        };
        let result = result.and(cleanup);
        self.control.status.send_replace(Status::Finished(result));
        result
    }
    #[allow(clippy::future_not_send, reason = "native transports may be Rc-owned")]
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the guarded admission API"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "single cleanup scope owns capture and RPC admission"
    )]
    async fn execute<T: RoutineInteractionTransport>(
        &self,
        binding: &Keybind,
        transport: &T,
        factory: Arc<dyn capture::Factory>,
        generation: &CancellationToken,
        valid: &dyn Fn() -> bool,
        sink: &mut Option<Box<dyn crate::typing::Session>>,
    ) -> Result<(), Error> {
        let cancel = self.control.cancel.clone();
        let guard = crate::typing::Guard {
            cancel: &cancel,
            generation,
            valid,
        };
        let consumer = HoldOutput {
            status: &self.control.status,
            observer: &self.observer,
            typing: std::cell::RefCell::new((sink, crate::typing::Delivery::default(), false)),
        };
        let output = crate::interaction::run(
            transport,
            factory,
            &binding.audio,
            crate::interaction::Selection {
                routine: &binding.routine.name,
                trigger: &binding.routine.trigger,
                input: &binding.routine.input,
                output: &binding.routine.output,
            },
            &self.control.release,
            &guard,
            &self.observer,
            &consumer,
        )
        .await?;
        let Some(crate::interaction::Output { text, invocation }) = output else {
            return Ok(());
        };
        let Some(spec) = &binding.command else {
            return Ok(());
        };
        self.control.status.send_replace(Status::Command);
        self.observer.emit(Diagnostic::Stage(Stage::Action));
        guard.check().map_err(|_| Error::Cancelled)?;
        let command = crate::command::run(spec, &text, &invocation, &cancel);
        tokio::pin!(command);
        tokio::select! { biased;
            () = generation.cancelled() => {
                cancel.cancel();
                match command.await {
                    Err(error @ crate::command::CommandError::Cleanup { .. }) => Err(Error::Command(error)),
                    _ => Err(Error::Cancelled),
                }
            }
            result = &mut command => result.map(|_| ()).map_err(Error::Command),
        }
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

struct HoldOutput<'a> {
    status: &'a tokio::sync::watch::Sender<Status>,
    observer: &'a Observer,
    typing: std::cell::RefCell<(
        &'a mut Option<Box<dyn crate::typing::Session>>,
        crate::typing::Delivery,
        bool,
    )>,
}
impl crate::interaction::Consumer for HoldOutput<'_> {
    fn progress(&self, progress: crate::interaction::Progress) {
        if progress == crate::interaction::Progress::Accepted {
            self.status.send_replace(Status::Streaming);
        }
    }
    fn output<'a>(
        &'a self,
        output: &'a waltr_client::interaction::InteractionOutput,
        port: &'a str,
        final_text: Option<&'a str>,
        guard: &'a crate::typing::Guard<'_>,
    ) -> crate::typing::LocalFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut state = self.typing.borrow_mut();
            let (sink, delivery, action_observed) = &mut *state;
            let Some(sink) = sink.as_mut() else {
                return Ok(());
            };
            let prefix = match final_text {
                Some(text) => Some(text.to_owned()),
                None => output.live_text(port).map_err(|_| {
                    self.observer
                        .emit(Diagnostic::RpcFailure(RpcFailure::from_output(
                            output.output_failure(),
                        )));
                    Error::Interaction
                })?,
            };
            if let Some(prefix) = prefix {
                if !*action_observed {
                    *action_observed = true;
                    self.observer.emit(Diagnostic::Stage(Stage::Action));
                }
                delivery
                    .deliver(&prefix, sink.as_mut(), guard)
                    .await
                    .map_err(Error::Typing)?;
            }
            if final_text.is_some() {
                delivery
                    .finish(sink.as_mut(), guard)
                    .await
                    .map_err(Error::Typing)?;
            }
            Ok(())
        })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
