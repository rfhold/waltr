//! Shared microphone admission, runtime fences, and desktop audio holds.
use crate::audio_permissions::{AudioPermissions, PermissionResult};
use crate::native_audio_telemetry as telemetry;
use crate::native_audio_telemetry::Operation;
use waltr_client::RoutineInteractionTransport;
use waltr_desktop::{capture, host::AudioAdmission};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

#[derive(Default)]
pub(crate) struct State {
    pub generation: u64,
    pub allowed: bool,
    pub active: Option<CancellationToken>,
    pub active_revision: Option<u64>,
    pub credentials: Option<waltr_client::NativeCredentialProvider>,
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
        let (hold, control) = waltr_desktop::host::Hold::press(&self.gate).map_err(|_| {
            telemetry::admission("hold", "busy");
            "Native audio is busy"
        })?;
        telemetry::admission("hold", "admitted");
        let diagnostics = Operation::new("hold");
        let token = CancellationToken::new();
        state.active = Some(token.clone());
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
                        | waltr_desktop::host::Error::Typing(waltr_desktop::typing::Error::Cancelled)
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
