//! Shared microphone admission, runtime fences, and desktop audio holds.
use crate::audio_permissions::{AudioPermissions, PermissionResult};
use crate::native_audio_telemetry as telemetry;
use crate::native_audio_telemetry::Operation;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use waltr_client::RoutineInteractionTransport;
use waltr_desktop::{capture, host::AudioAdmission};
mod catalog;

#[derive(Default)]
pub(crate) struct State {
    pub generation: u64,
    pub allowed: bool,
    pub media_epoch: u64,
    pub picker_epoch: u64,
    pub active: Option<CancellationToken>,
    pub active_revision: Option<u64>,
    pub credentials: Option<waltr_client::NativeCredentialProvider>,
    pub(super) catalog: Option<catalog::Catalog>,
    pub(super) pinned: Option<catalog::Pin>,
}
impl State {
    pub(crate) fn bind(&mut self, generation: u64) {
        self.cancel();
        *self = Self {
            generation,
            ..Default::default()
        };
    }
    pub(crate) fn access(&mut self, allowed: bool) {
        self.allowed = allowed;
        if !allowed {
            self.media_epoch = self.media_epoch.wrapping_add(1);
            self.picker_epoch = self.picker_epoch.wrapping_add(1);
            self.cancel();
        }
    }
    pub(crate) fn credentials_changed(&self) -> bool {
        self.credentials
            .as_ref()
            .map(waltr_client::NativeCredentialProvider::credential_revision)
            != self.active_revision
    }
    pub(crate) fn cancel(&self) {
        if let Some(active) = &self.active {
            active.cancel();
        }
    }
}
pub struct Host {
    state: Mutex<State>,
    pub gate: AudioAdmission,
    pub(crate) activity: crate::activity::ActivityTracker,
}
impl Host {
    pub fn new(activity: crate::activity::ActivityTracker) -> Self {
        Self {
            state: Mutex::new(State::default()),
            gate: AudioAdmission::default(),
            activity,
        }
    }
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn hold(
        self: &Arc<Self>,
        binding: Arc<waltr_desktop::config::Keybind>,
    ) -> Result<(Held, waltr_desktop::host::HoldControl), String> {
        let mut state = self.lock();
        if !state.allowed {
            telemetry::admission("hold", "unavailable");
            return Err("Native audio is unavailable".into());
        }
        let catalog = state
            .catalog
            .as_ref()
            .ok_or("Routine discovery is not available")?;
        let invocation = catalog.resolve(
            &binding.routine.name,
            &binding.routine.trigger,
            None,
            format!("native-hold-{}", state.generation),
        )?;
        let pinned = catalog.pin(invocation.clone(), false);
        let binding = Arc::new(waltr_desktop::config::Keybind {
            resolved: Some(invocation),
            id: binding.id.clone(),
            key: binding.key.clone(),
            mode: waltr_desktop::config::Mode::Hold,
            max_capture_seconds: binding.max_capture_seconds,
            routine: waltr_desktop::config::Routine {
                name: binding.routine.name.clone(),
                trigger: binding.routine.trigger.clone(),
                input: binding.routine.input.clone(),
                output: binding.routine.output.clone(),
            },
            audio: waltr_desktop::config::Audio {
                device: binding.audio.device.clone(),
                sample_rate_hz: binding.audio.sample_rate_hz,
                channels: binding.audio.channels,
            },
            command: binding
                .command
                .as_ref()
                .map(|c| waltr_desktop::command::CommandSpec {
                    argv: c.argv.clone(),
                    stdin: c.stdin.clone(),
                    timeout_seconds: c.timeout_seconds,
                }),
            typing: binding
                .typing
                .as_ref()
                .map(|_| waltr_desktop::config::TypingSpec {}),
        });
        let (hold, control) = waltr_desktop::host::Hold::press(&self.gate).map_err(|_| {
            telemetry::admission("hold", "busy");
            "Native audio is busy"
        })?;
        telemetry::admission("hold", "admitted");
        let diagnostics = Operation::new("hold");
        let token = CancellationToken::new();
        state.active = Some(token.clone());
        state.pinned = Some(pinned);
        state.active_revision = state
            .credentials
            .as_ref()
            .map(waltr_client::NativeCredentialProvider::credential_revision);
        let result = (
            Held {
                host: self.clone(),
                hold: hold.observed(diagnostics.observer()),
                diagnostics,
                activity: self.activity.begin(),
                binding,
                token,
                generation: state.generation,
                credential: state
                    .credentials
                    .as_ref()
                    .map(|provider| (provider.clone(), provider.credential_revision())),
            },
            control,
        );
        drop(state);
        Ok(result)
    }
    pub fn observe(&self, snapshot: &waltr_client::ClientSnapshot) -> bool {
        let generation = self.lock().generation;
        self.observe_for(generation, snapshot)
    }
    pub(crate) fn observe_for(
        &self,
        generation: u64,
        snapshot: &waltr_client::ClientSnapshot,
    ) -> bool {
        let catalog = catalog::Catalog::new(snapshot);
        let mut state = self.lock();
        if state.generation != generation {
            return false;
        }
        let changed = state
            .pinned
            .as_ref()
            .is_some_and(|pin| !catalog.current(pin));
        if changed {
            state.cancel();
            state.pinned = None;
        }
        state.catalog = Some(catalog);
        changed
    }
}
pub struct Held {
    host: Arc<Host>,
    diagnostics: Operation,
    hold: waltr_desktop::host::Hold,
    binding: Arc<waltr_desktop::config::Keybind>,
    pub token: CancellationToken,
    pub generation: u64,
    credential: Option<(waltr_client::NativeCredentialProvider, u64)>,
    activity: crate::activity::ActivityLease,
}
#[allow(
    clippy::future_not_send,
    reason = "native interaction transports are LocalSet-owned"
)]
impl Held {
    pub async fn run<T: RoutineInteractionTransport>(
        self,
        transport: &T,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
    ) {
        self.run_with_typing(
            transport,
            permissions,
            factory,
            &waltr_desktop::typing::NativeFactory,
        )
        .await;
    }

    pub(crate) async fn run_with_typing<T: RoutineInteractionTransport>(
        self,
        transport: &T,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
        typing: &dyn waltr_desktop::typing::Factory,
    ) {
        let diagnostics = self.diagnostics.clone();
        self.run_inner(transport, permissions, factory, typing)
            .instrument(diagnostics.span())
            .await;
        diagnostics.finish();
    }

    async fn run_inner<T: RoutineInteractionTransport>(
        mut self,
        transport: &T,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
        typing: &dyn waltr_desktop::typing::Factory,
    ) {
        let grant = tokio::select! { biased;
            () = self.token.cancelled() => PermissionResult::Cancelled,
            grant = permissions.request(self.generation, u64::MAX) => grant,
        };
        self.diagnostics.permission(grant);
        let valid = || {
            let state = self.host.lock();
            !self.token.is_cancelled()
                && state.allowed
                && state.generation == self.generation
                && self
                    .credential
                    .as_ref()
                    .is_none_or(|(provider, revision)| provider.credential_revision() == *revision)
        };
        if grant == PermissionResult::Granted && valid() {
            let result = self
                .hold
                .run_guarded_with_typing(
                    &self.binding,
                    transport,
                    factory,
                    &self.token,
                    &valid,
                    Some(typing),
                )
                .await;
            self.diagnostics.primary(result);
            if result.is_err_and(|error| {
                !matches!(
                    error,
                    waltr_desktop::host::Error::Cancelled
                        | waltr_desktop::host::Error::Typing(
                            waltr_desktop::typing::Error::Cancelled
                        )
                )
            }) {
                self.activity.fail();
            }
        } else if matches!(
            grant,
            PermissionResult::Denied | PermissionResult::Unavailable
        ) && valid()
        {
            self.activity.fail();
        } else if !valid() {
            self.diagnostics.fail("invalidated");
        }
    }
}
