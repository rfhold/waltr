//! Shared microphone admission, runtime fences, and desktop audio holds.
use crate::audio_permissions::{AudioPermissions, PermissionResult};
use crate::native_audio_telemetry as telemetry;
use crate::native_audio_telemetry::Operation;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
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
    pub hold_feedback: Option<Arc<HoldFeedback>>,
    pub credentials: Option<waltr_client::NativeCredentialProvider>,
    pub(super) catalog: Option<catalog::Catalog>,
    pub(super) pinned: Option<catalog::Pin>,
    scoped_view: bool,
    shortcut_catalogs: BTreeMap<String, ShortcutRoute>,
    shortcut_pin: Option<(String, catalog::Pin, CancellationToken)>,
}

struct ShortcutRoute {
    catalog: catalog::Catalog,
    transport: waltr_client::NativeGrpcTransport,
    revision: u64,
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
pub(crate) struct HoldFeedback {
    diagnostics: Operation,
    // Hold through publication to order rejection against terminal success.
    rejected: Mutex<bool>,
}
impl HoldFeedback {
    pub(crate) fn reject(&self, generation: u64, activity: &crate::activity::ActivityTracker) {
        let mut rejected = self
            .rejected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *rejected = true;
        let diagnostics = Operation::new("hold");
        diagnostics.fail("capture_rejected");
        diagnostics.finish();
        activity.complete(generation, diagnostics.completion());
        drop(rejected);
    }
    pub(crate) fn complete(&self, generation: u64, activity: &crate::activity::ActivityTracker) {
        let rejected = self
            .rejected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.diagnostics.finish();
        let completion = match self.diagnostics.completion() {
            telemetry::Completion::Succeeded if *rejected => telemetry::Completion::Unchanged,
            completion => completion,
        };
        activity.complete(generation, completion);
        drop(rejected);
    }
    #[cfg(test)]
    pub(crate) fn primary(&self, result: Result<(), waltr_desktop::host::Error>) {
        self.diagnostics.primary(result);
    }
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
        let destination = binding.routine.destination.as_deref().unwrap_or("local");
        let target = match destination {
            "local" if !state.scoped_view => None,
            id => Some(
                state
                    .shortcut_catalogs
                    .get(id)
                    .ok_or("Shortcut destination is unavailable")?,
            ),
        };
        let (catalog, transport, target_credential) = if let Some(target) = target {
            let provider = target.transport.credential_provider();
            if provider.credential_revision() != target.revision || !provider.has_bearer_token() {
                return Err("Shortcut destination is not authenticated".into());
            }
            (
                &target.catalog,
                Some(target.transport.clone()),
                Some((provider, target.revision)),
            )
        } else {
            (
                state
                    .catalog
                    .as_ref()
                    .ok_or("Routine discovery is not available")?,
                None,
                None,
            )
        };
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
            mode: binding.mode,
            max_capture_seconds: binding.max_capture_seconds,
            routine: binding.routine.clone(),
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
            playback: binding
                .playback
                .as_ref()
                .map(|_| waltr_desktop::config::PlaybackSpec {}),
        });
        let repeated = binding.mode == waltr_desktop::config::Mode::Hold
            && binding.playback.is_some()
            && binding.resolved.as_ref().is_some_and(|invocation| {
                invocation
                    .limits
                    .definition
                    .capabilities
                    .iter()
                    .any(|c| c == "media.inputs")
                    && invocation.limits.definition.ports.iter().any(|port| {
                        port.name == binding.routine.output
                            && port.direction == waltr_client::RoutineDirection::Output
                            && port.media == waltr_client::RoutineMedia::Audio
                            && !port.formats.is_empty()
                            && port
                                .formats
                                .iter()
                                .all(|format| format == "pcm16le-24000-mono")
                    })
            });
        let (hold, control) = if binding.mode == waltr_desktop::config::Mode::Toggle {
            waltr_desktop::host::Hold::press_toggle(&self.gate)
        } else if repeated {
            waltr_desktop::host::Hold::press_repeated(&self.gate)
        } else {
            waltr_desktop::host::Hold::press(&self.gate)
        }
        .map_err(|_| {
            telemetry::admission("hold", "busy");
            "Native audio is busy"
        })?;
        telemetry::admission("hold", "admitted");
        let diagnostics = Operation::new("hold");
        let feedback = Arc::new(HoldFeedback {
            diagnostics: diagnostics.clone(),
            rejected: Mutex::new(false),
        });
        state.hold_feedback = Some(feedback.clone());
        let token = CancellationToken::new();
        state.active = Some(token.clone());
        if transport.is_some() {
            state.shortcut_pin = Some((destination.to_owned(), pinned, token.clone()));
            state.pinned = None;
        } else {
            state.shortcut_pin = None;
            state.pinned = Some(pinned);
        }
        state.active_revision = state
            .credentials
            .as_ref()
            .map(waltr_client::NativeCredentialProvider::credential_revision);
        let result = (
            Held {
                host: self.clone(),
                hold: hold.observed(diagnostics.observer()),
                diagnostics,
                feedback,
                activity: self.activity.begin(),
                binding,
                token,
                generation: state.generation,
                transport,
                credential: target_credential.or_else(|| {
                    state
                        .credentials
                        .as_ref()
                        .map(|provider| (provider.clone(), provider.credential_revision()))
                }),
            },
            control,
        );
        drop(state);
        Ok(result)
    }
    pub(crate) fn scoped_view(&self, generation: u64, scoped: bool) {
        let mut state = self.lock();
        if state.generation == generation {
            state.scoped_view = scoped;
        }
    }

    pub(crate) fn observe_shortcut(
        &self,
        generation: u64,
        id: &str,
        observation: Option<(
            waltr_client::ClientRoutineDefinitionSnapshot,
            waltr_client::NativeGrpcTransport,
            u64,
        )>,
    ) {
        let mut state = self.lock();
        if state.generation != generation {
            return;
        }
        let next = observation.map(|(snapshot, transport, revision)| ShortcutRoute {
            catalog: catalog::Catalog::from_definitions(snapshot),
            transport,
            revision,
        });
        if let Some((destination, pin, token)) = &state.shortcut_pin
            && destination == id
            && next.as_ref().is_none_or(|next| {
                state
                    .shortcut_catalogs
                    .get(id)
                    .is_none_or(|previous| previous.revision != next.revision)
                    || !next.catalog.current(pin)
            })
        {
            token.cancel();
            state.shortcut_pin = None;
        }
        if let Some(next) = next {
            state.shortcut_catalogs.insert(id.to_owned(), next);
        } else {
            state.shortcut_catalogs.remove(id);
        }
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
    feedback: Arc<HoldFeedback>,
    hold: waltr_desktop::host::Hold,
    binding: Arc<waltr_desktop::config::Keybind>,
    pub token: CancellationToken,
    pub generation: u64,
    transport: Option<waltr_client::NativeGrpcTransport>,
    credential: Option<(waltr_client::NativeCredentialProvider, u64)>,
    activity: crate::activity::ActivityLease,
}

struct UnreleasedCapture {
    factory: Arc<dyn capture::Factory>,
    release: CancellationToken,
}

impl capture::Factory for UnreleasedCapture {
    fn open(
        &self,
        config: waltr_audio_io::Config,
    ) -> Result<Box<dyn capture::Source>, capture::Error> {
        if self.release.is_cancelled() {
            return Err(capture::Error::Cancelled);
        }
        self.factory.open(config)
    }
}

async fn wait_for_release(release: Option<&CancellationToken>) {
    if let Some(release) = release {
        release.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[allow(
    clippy::future_not_send,
    reason = "native interaction transports are LocalSet-owned"
)]
impl Held {
    pub(crate) fn destination(&self) -> Option<&str> {
        self.binding.routine.destination.as_deref()
    }

    pub(crate) fn alternate_destination(&self) -> Option<&str> {
        self.transport.as_ref()?;
        Some(self.destination().unwrap_or("local"))
    }

    #[cfg(test)]
    pub(crate) fn invocation(&self) -> &waltr_desktop::interaction::Invocation {
        self.binding.resolved.as_ref().unwrap()
    }

    async fn verify_destination(&self, transport: &waltr_client::NativeGrpcTransport) -> bool {
        use futures_util::StreamExt as _;
        use waltr_client::WaltrTransport as _;

        let current = || {
            !self.token.is_cancelled()
                && self
                    .credential
                    .as_ref()
                    .is_some_and(|(provider, revision)| {
                        provider.credential_revision() == *revision && provider.has_bearer_token()
                    })
        };
        let outcome = tokio::select! {
            biased;
            () = self.token.cancelled() => "cancelled",
            // A cold relay connection and the destination's signing-key fetch
            // both happen before it can return the first snapshot.
            result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                let Some(invocation) = &self.binding.resolved else { return "definition_mismatch"; };
                if !current() { return "invalidated"; }
                let Ok(mut stream) = transport.watch_manual_routines("native-shortcut-check".into()).await else { return "watch_open_failed"; };
                let snapshot = match stream.next().await {
                    Some(Ok(snapshot)) => snapshot,
                    Some(Err(_)) => return "watch_failed",
                    None => return "empty_watch",
                };
                let catalog = catalog::Catalog::from_definitions(snapshot);
                if !current() { "invalidated" }
                else if catalog.current(&catalog.pin(invocation.clone(), false)) { "ready" }
                else { "definition_mismatch" }
            }) => result.unwrap_or("deadline"),
        };
        self.diagnostics.destination_preflight(outcome);
        outcome == "ready"
    }

    pub(crate) async fn run_routed(
        self,
        active_transport: &waltr_client::NativeGrpcTransport,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
    ) {
        let transport = self
            .transport
            .clone()
            .unwrap_or_else(|| active_transport.clone());
        let preflight = self.alternate_destination().map(|_| &transport);
        self.run_with_checks(
            &transport,
            permissions,
            factory,
            &waltr_desktop::typing::NativeFactory,
            preflight,
        )
        .await;
    }

    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) async fn run_with_typing<T: RoutineInteractionTransport>(
        self,
        transport: &T,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
        typing: &dyn waltr_desktop::typing::Factory,
    ) {
        self.run_with_checks(transport, permissions, factory, typing, None)
            .await;
    }

    async fn run_with_checks<T: RoutineInteractionTransport>(
        self,
        transport: &T,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
        typing: &dyn waltr_desktop::typing::Factory,
        preflight: Option<&waltr_client::NativeGrpcTransport>,
    ) {
        let diagnostics = self.diagnostics.clone();
        let feedback = self.feedback.clone();
        let activity = self.host.activity.clone();
        let generation = self.generation;
        self.run_inner(transport, permissions, factory, typing, preflight)
            .instrument(diagnostics.span())
            .await;
        feedback.complete(generation, &activity);
    }

    async fn run_inner<T: RoutineInteractionTransport>(
        mut self,
        transport: &T,
        permissions: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
        typing: &dyn waltr_desktop::typing::Factory,
        preflight: Option<&waltr_client::NativeGrpcTransport>,
    ) {
        let release = (preflight.is_some() && self.binding.mode == waltr_desktop::config::Mode::Hold)
            .then(|| self.hold.one_shot_release())
            .flatten();
        if let Some(transport) = preflight {
            let ready = tokio::select! {
                biased;
                () = wait_for_release(release.as_ref()) => {
                    self.diagnostics.fail("cancelled");
                    return;
                }
                ready = self.verify_destination(transport) => ready,
            };
            if !ready {
                if self.token.is_cancelled() {
                    self.diagnostics.fail("invalidated");
                } else {
                    self.diagnostics.fail("destination_unavailable");
                    self.activity.fail();
                }
                return;
            }
        }
        let grant = tokio::select! { biased;
            () = self.token.cancelled() => PermissionResult::Cancelled,
            () = wait_for_release(release.as_ref()) => {
                self.diagnostics.fail("cancelled");
                return;
            }
            grant = permissions.request(self.generation, u64::MAX) => grant,
        };
        if release.as_ref().is_some_and(CancellationToken::is_cancelled) {
            self.diagnostics.fail("cancelled");
            return;
        }
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
            let factory: Arc<dyn capture::Factory> = match &release {
                Some(release) => Arc::new(UnreleasedCapture {
                    factory,
                    release: release.clone(),
                }),
                None => factory,
            };
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            let playback = self.binding.playback.as_ref().map(|_| {
                crate::native_media::hold_playback::Sink::new(
                    self.binding.routine.output.clone(),
                    crate::native_media::hold_playback::Fence {
                        host: self.host.clone(),
                        generation: self.generation,
                        generation_cancel: self.token.clone(),
                        cancel: self.hold.cancellation(),
                        revoked: CancellationToken::new(),
                        credential: self.credential.clone(),
                    },
                )
            });
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            let playback = playback
                .as_ref()
                .map(|s| s as &dyn waltr_desktop::media::OutputSink);
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            let playback = None;
            let result = self
                .hold
                .run_guarded_with_outputs(
                    &self.binding,
                    transport,
                    factory,
                    &self.token,
                    &valid,
                    Some(typing),
                    playback,
                )
                .await;
            let result = match result {
                Err(waltr_desktop::host::Error::Capture(capture::Error::Cancelled))
                    if release.as_ref().is_some_and(CancellationToken::is_cancelled) =>
                {
                    Err(waltr_desktop::host::Error::Cancelled)
                }
                result => result,
            };
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
