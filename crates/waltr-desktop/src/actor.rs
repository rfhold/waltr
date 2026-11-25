//! Shortcut-to-hold local actor. Each actor pins one validated config, transport/profile,
//! and access generation. Cancel and await it before replacing any of those dependencies.
use crate::{
    capture,
    config::{Config, Mode},
    host::{AudioAdmission, Error, Hold, Status},
    shortcut,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use waltr_client::RoutineInteractionTransport;

/// Caller-driven actor; does not spawn an async task or own a global runtime.
pub struct Actor<T> {
    config: Config,
    transport: T,
    factory: Arc<dyn capture::Factory>,
    gate: AudioAdmission,
    status: tokio::sync::watch::Sender<Status>,
    valid: std::rc::Rc<dyn Fn() -> bool>,
}
impl<T> std::fmt::Debug for Actor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Actor")
            .field("bindings", &self.config.keybinds.len())
            .finish_non_exhaustive()
    }
}
/// Terminal actor errors are static and bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorError {
    /// Invalid startup snapshot; no native access occurred.
    Config,
    /// Configuration requires an output capability that was not installed.
    Unsupported,
    /// Backend failure, including overflow and lost key-up protection.
    Shortcut(shortcut::Error),
    /// Profile/access/shutdown generation invalidated and all owned work joined.
    Cancelled,
}
impl<T: RoutineInteractionTransport> Actor<T> {
    /// Pin dependencies without device access. Share `gate` with future app-chat capture.
    /// # Errors
    /// Rejects invalid config, explicit destinations, and playback.
    pub fn new(
        config: Config,
        transport: T,
        factory: Arc<dyn capture::Factory>,
        gate: AudioAdmission,
    ) -> Result<(Self, tokio::sync::watch::Receiver<Status>), ActorError> {
        Self::with_guard(config, transport, factory, gate, std::rc::Rc::new(|| true))
    }
    /// Pin a synchronous identity/credential effect fence.
    /// # Errors
    /// Rejects invalid config, explicit destinations, and playback before microphone access.
    pub fn with_guard(
        config: Config,
        transport: T,
        factory: Arc<dyn capture::Factory>,
        gate: AudioAdmission,
        valid: std::rc::Rc<dyn Fn() -> bool>,
    ) -> Result<(Self, tokio::sync::watch::Receiver<Status>), ActorError> {
        config.validate().map_err(|_| ActorError::Config)?;
        if config
            .keybinds
            .iter()
            .any(|binding| binding.routine.destination.is_some() || binding.playback.is_some())
        {
            return Err(ActorError::Unsupported);
        }
        let (status, receiver) = tokio::sync::watch::channel(Status::Idle);
        Ok((
            Self {
                config,
                transport,
                factory,
                gate,
                status,
                valid,
            },
            receiver,
        ))
    }
    /// Drive shortcut events, capture, input, output, and child cleanup on one local future.
    /// Cancel `generation` then await this method on switch, access loss, or host shutdown.
    /// Backend failure also cancels and joins active work before return. Per-hold failure appears
    /// in status and never retries; a later physical press starts a new operation.
    /// # Errors
    /// Returns only after active work acknowledges owned teardown.
    #[allow(clippy::future_not_send, reason = "host transports may be Rc-owned")]
    pub async fn run(
        &self,
        events: &mut shortcut::Receiver,
        generation: &CancellationToken,
    ) -> Result<(), ActorError> {
        loop {
            let edge = tokio::select! { biased;
                () = generation.cancelled() => return Err(ActorError::Cancelled),
                event = events.next() => event.map_err(ActorError::Shortcut)?,
            };
            if !edge.pressed {
                continue;
            }
            let Some(binding) = self.config.keybinds.iter().find(|b| b.id == edge.id) else {
                continue;
            };
            let admission = match binding.mode {
                Mode::Toggle => Hold::press_toggle(&self.gate),
                Mode::Hold | Mode::PressToTalk => Hold::press(&self.gate),
            };
            let (mut hold, control) = match admission {
                Ok(hold) => hold,
                Err(Error::Busy) => {
                    self.status.send_replace(Status::Finished(Err(Error::Busy)));
                    continue;
                }
                Err(error) => {
                    self.status.send_replace(Status::Finished(Err(error)));
                    continue;
                }
            };
            hold.use_status(self.status.clone());
            let backend_generation = events.failure_token();
            let valid = || !generation.is_cancelled() && (self.valid)();
            let operation = hold.run_guarded(
                binding,
                &self.transport,
                self.factory.clone(),
                &backend_generation,
                &valid,
            );
            tokio::pin!(operation);
            loop {
                tokio::select! { biased;
                    () = generation.cancelled() => { control.cancel(); let _result = operation.await; return Err(ActorError::Cancelled); }
                    event = events.next() => {
                        match event {
                            Ok(event) if event.id == binding.id => match (binding.mode, event.pressed) {
                                (Mode::Hold, false) => control.release(),
                                (Mode::Toggle, true) => { let _ = control.finish_toggle_capture(); }
                                _ => (),
                            },
                            Ok(_) => (), // No queued captures, including other binding presses.
                            Err(error) => { control.cancel(); let _result = operation.await; return Err(ActorError::Shortcut(error)); }
                        }
                    }
                    _result = &mut operation => break,
                }
            }
        }
    }
}

impl std::fmt::Display for ActorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ActorError {}
