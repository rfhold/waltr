//! Chat dictation identity, status publication, IPC, and capture/RPC consumer.
use crate::audio_permissions::{AudioPermissions, PermissionResult};
use crate::native_audio_telemetry::{self as telemetry, Operation};
use waltr_client::RoutineInteractionTransport;
use waltr_desktop::capture;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

const MAX_ID: u64 = 9_007_199_254_740_991;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    routine: String,
    trigger: String,
    input: String,
    output: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    generation: u64,
    operation_id: u64,
    phase: &'static str,
    preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_text: Option<String>,
}
struct Record {
    status: Status,
    cancel: CancellationToken,
    finish: CancellationToken,
}
#[derive(Default)]
struct State {
    highwater: u64,
    latest: Option<Record>,
}
pub struct Host {
    // Lock the shared audio fence before chat state for combined transactions.
    state: Mutex<State>,
    pub(crate) audio: Arc<crate::native_audio::Host>,
}
impl Default for Host {
    fn default() -> Self {
        Self::new(Arc::new(crate::native_audio::Host::new(
            crate::activity::ActivityTracker::default(),
        )))
    }
}
impl Host {
    pub fn new(audio: Arc<crate::native_audio::Host>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            audio,
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn bind(&self, generation: u64) {
        let mut audio = self.audio.lock();
        let mut state = self.lock();
        audio.bind(generation);
        *state = State::default();
    }
    pub fn credentials(&self, credentials: waltr_client::NativeCredentialProvider) {
        self.audio.lock().credentials = Some(credentials);
    }
    pub fn access(&self, allowed: bool) {
        let mut audio = self.audio.lock();
        let mut state = self.lock();
        audio.access(allowed);
        if !allowed && let Some(record) = &mut state.latest {
            record.cancel.cancel();
            record.status.phase = "cancelled";
            record.status.preview.clear();
            record.status.final_text = None;
        }
    }
    pub fn check_credentials(&self) {
        let audio = self.audio.lock();
        let mut state = self.lock();
        if audio.credentials_changed() {
            audio.cancel();
            if let Some(record) = &mut state.latest {
                record.cancel.cancel();
                record.status.phase = "cancelled";
                record.status.preview.clear();
                record.status.final_text = None;
            }
        }
    }
    pub fn cancel_all(&self) {
        let audio = self.audio.lock();
        let mut state = self.lock();
        audio.cancel();
        if let Some(record) = &mut state.latest {
            record.cancel.cancel();
            record.status.phase = "cancelled";
            record.status.preview.clear();
            record.status.final_text = None;
        }
    }
    #[allow(
        clippy::too_many_lines,
        reason = "one admission scope retains identity and microphone ownership"
    )]
    pub fn start(
        self: &Arc<Self>,
        generation: u64,
        operation: u64,
        selection: Selection,
        session: &str,
    ) -> Result<(Status, Option<Job>), String> {
        validate_identity(generation, operation)?;
        if [
            selection.routine.as_str(),
            selection.trigger.as_str(),
            selection.input.as_str(),
            selection.output.as_str(),
            session,
        ]
        .iter()
        .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err("Invalid dictation selection".into());
        }
        let mut audio = self.audio.lock();
        let mut state = self.lock();
        if audio.generation != generation {
            telemetry::admission("chat", "stale");
            return Err("Native client generation was replaced".into());
        }
        if let Some(record) = &state.latest {
            if operation == record.status.operation_id {
                telemetry::admission("chat", "duplicate");
                return Ok((record.status.clone(), None));
            }
            if operation < record.status.operation_id {
                telemetry::admission("chat", "stale");
                return Err("Stale dictation operation".into());
            }
        }
        if operation <= state.highwater {
            telemetry::admission("chat", "stale");
            return Err("Stale dictation operation".into());
        }
        state.highwater = operation;
        if !audio.allowed {
            telemetry::admission("chat", "unavailable");
            return Err("Native dictation is unavailable".into());
        }
        let permit = self.audio.gate.reserve().map_err(|_| {
            telemetry::admission("chat", "busy");
            "Native audio is busy"
        })?;
        telemetry::admission("chat", "admitted");
        let cancel = CancellationToken::new();
        let finish = CancellationToken::new();
        let status = Status {
            generation,
            operation_id: operation,
            phase: "starting",
            preview: String::new(),
            final_text: None,
        };
        audio.active = Some(cancel.clone());
        audio.active_revision = audio
            .credentials
            .as_ref()
            .map(waltr_client::NativeCredentialProvider::credential_revision);
        state.latest = Some(Record {
            status: status.clone(),
            cancel: cancel.clone(),
            finish: finish.clone(),
        });
        let result = (
            status,
            Some(Job {
                host: self.clone(),
                diagnostics: Operation::new("chat"),
                generation,
                operation,
                selection,
                cancel,
                finish,
                credential: audio
                    .credentials
                    .as_ref()
                    .map(|provider| (provider.clone(), provider.credential_revision())),
                _permit: permit,
                activity: self.audio.activity.begin(),
            }),
        );
        drop(state);
        Ok(result)
    }
    pub fn control(&self, generation: u64, operation: u64, action: &str) -> Result<Status, String> {
        validate_identity(generation, operation)?;
        let audio = self.audio.lock();
        let mut state = self.lock();
        if audio.generation != generation {
            return Err("Native client generation was replaced".into());
        }
        if action == "cancel" && operation > state.highwater {
            state.highwater = operation;
            state.latest = Some(Record {
                status: Status {
                    generation,
                    operation_id: operation,
                    phase: "cancelled",
                    preview: String::new(),
                    final_text: None,
                },
                cancel: CancellationToken::new(),
                finish: CancellationToken::new(),
            });
        }
        let record = state
            .latest
            .as_mut()
            .filter(|r| r.status.operation_id == operation)
            .ok_or("Stale dictation operation")?;
        if action == "cancel" {
            record.cancel.cancel();
            record.status.phase = "cancelled";
            record.status.preview.clear();
            record.status.final_text = None;
        }
        if action == "finish" {
            record.finish.cancel();
        }
        let status = record.status.clone();
        drop(state);
        Ok(status)
    }
    fn publish(&self, generation: u64, operation: u64, phase: &'static str, text: Option<String>) {
        let audio = self.audio.lock();
        let mut state = self.lock();
        if audio.generation != generation {
            return;
        }
        if let Some(record) = state
            .latest
            .as_mut()
            .filter(|r| r.status.operation_id == operation && !r.cancel.is_cancelled())
        {
            record.status.phase = phase;
            if matches!(phase, "error" | "cancelled" | "succeeded") {
                record.status.preview.clear();
            }
            record.status.final_text = text;
        }
    }
}
fn validate_identity(generation: u64, operation: u64) -> Result<(), String> {
    if generation == 0 || operation == 0 || generation > MAX_ID || operation > MAX_ID {
        return Err("Invalid dictation identity".into());
    }
    Ok(())
}
pub struct Job {
    host: Arc<Host>,
    diagnostics: Operation,
    generation: u64,
    operation: u64,
    selection: Selection,
    cancel: CancellationToken,
    finish: CancellationToken,
    credential: Option<(waltr_client::NativeCredentialProvider, u64)>,
    activity: crate::activity::ActivityLease,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
#[allow(
    clippy::future_not_send,
    reason = "native interaction transports are LocalSet-owned"
)]
impl Job {
    fn valid(&self) -> bool {
        let audio = self.host.audio.lock();
        let state = self.host.lock();
        !self.cancel.is_cancelled()
            && audio.allowed
            && audio.generation == self.generation
            && state
                .latest
                .as_ref()
                .is_some_and(|r| r.status.operation_id == self.operation)
            && self
                .credential
                .as_ref()
                .is_none_or(|(provider, revision)| provider.credential_revision() == *revision)
    }
    fn preview(&self, text: String) {
        let audio = self.host.audio.lock();
        let mut state = self.host.lock();
        if !audio.allowed
            || audio.generation != self.generation
            || self.cancel.is_cancelled()
            || self
                .credential
                .as_ref()
                .is_some_and(|(provider, revision)| provider.credential_revision() != *revision)
        {
            return;
        }
        if let Some(record) = state.latest.as_mut().filter(|record| {
            record.status.generation == self.generation
                && record.status.operation_id == self.operation
                && !record.cancel.is_cancelled()
                && matches!(record.status.phase, "starting" | "recording" | "processing")
        }) {
            record.status.preview = text;
        }
    }
    fn publish_current(&self, phase: &'static str, text: Option<String>) {
        let audio = self.host.audio.lock();
        let mut state = self.host.lock();
        if !audio.allowed
            || audio.generation != self.generation
            || self.cancel.is_cancelled()
            || self
                .credential
                .as_ref()
                .is_some_and(|(provider, revision)| provider.credential_revision() != *revision)
        {
            return;
        }
        if let Some(record) = state.latest.as_mut().filter(|record| {
            record.status.operation_id == self.operation
                && !record.cancel.is_cancelled()
                && matches!(record.status.phase, "starting" | "recording" | "processing")
        }) {
            record.status.phase = phase;
            if phase == "succeeded" {
                record.status.preview.clear();
            }
            record.status.final_text = text;
        }
    }
    pub async fn run<T: RoutineInteractionTransport>(
        self,
        transport: &T,
        permission: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
    ) {
        let diagnostics = self.diagnostics.clone();
        self.run_inner(transport, permission, factory)
            .instrument(diagnostics.span())
            .await;
        diagnostics.finish();
    }
    async fn run_inner<T: RoutineInteractionTransport>(
        mut self,
        transport: &T,
        permission: &AudioPermissions,
        factory: Arc<dyn capture::Factory>,
    ) {
        let grant = tokio::select! { biased; () = self.cancel.cancelled() => PermissionResult::Cancelled, grant = permission.request(self.generation, self.operation) => grant };
        self.diagnostics.permission(grant);
        if grant != PermissionResult::Granted || self.cancel.is_cancelled() {
            if self.cancel.is_cancelled() {
                self.diagnostics.fail("cancelled");
            }
            if matches!(
                grant,
                PermissionResult::Denied | PermissionResult::Unavailable
            ) && self.valid()
            {
                self.activity.fail();
            }
            self.host
                .publish(self.generation, self.operation, "error", None);
            return;
        }
        // Permission is only intent: the identity/access fence is checked again before capture.
        let current = self.valid();
        if !current {
            self.diagnostics.fail("invalidated");
            self.host
                .publish(self.generation, self.operation, "error", None);
            return;
        }
        let audio = waltr_desktop::config::Audio {
            device: waltr_desktop::config::DeviceSelector::Default,
            sample_rate_hz: 24_000,
            channels: 1,
        };
        let guard = waltr_desktop::typing::Guard {
            cancel: &self.cancel,
            generation: &self.cancel,
            valid: &|| self.valid(),
        };
        let cleanup_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let failed = cleanup_failed.clone();
        let diagnostics = self.diagnostics.observer();
        let observer = waltr_desktop::host::Observer::new(move |event| {
            if matches!(event, waltr_desktop::host::Diagnostic::CaptureCleanup(Err(error)) if error != capture::Error::Cancelled)
            {
                failed.store(true, std::sync::atomic::Ordering::Release);
            }
            diagnostics.emit(event);
        });
        let result = waltr_desktop::interaction::run(
            transport,
            factory,
            &audio,
            waltr_desktop::interaction::Selection {
                routine: &self.selection.routine,
                trigger: &self.selection.trigger,
                input: &self.selection.input,
                output: &self.selection.output,
            },
            &self.finish,
            &guard,
            &observer,
            &self,
        )
        .await;
        if cleanup_failed.load(std::sync::atomic::Ordering::Acquire)
            || (result.is_err() && self.valid())
        {
            self.activity.fail();
        }
        match result {
            Ok(output) if self.valid() => {
                self.publish_current("succeeded", output.map(|output| output.text));
            }
            _ => {
                if !self.valid() {
                    self.diagnostics.fail("invalidated");
                }
                self.host
                    .publish(self.generation, self.operation, "error", None);
            }
        }
    }
}
impl waltr_desktop::interaction::Consumer for Job {
    fn progress(&self, progress: waltr_desktop::interaction::Progress) {
        if self.valid() {
            let phase = match progress {
                waltr_desktop::interaction::Progress::Accepted => "recording",
                waltr_desktop::interaction::Progress::InputClosed => "processing",
            };
            self.publish_current(phase, None);
        }
    }
    fn output<'a>(
        &'a self,
        output: &'a waltr_client::interaction::InteractionOutput,
        port: &'a str,
        final_text: Option<&'a str>,
        guard: &'a waltr_desktop::typing::Guard<'_>,
    ) -> waltr_desktop::typing::LocalFuture<'a, Result<(), waltr_desktop::host::Error>> {
        Box::pin(async move {
            guard
                .check()
                .map_err(|_| waltr_desktop::host::Error::Cancelled)?;
            if final_text.is_none() {
                self.preview(output.preview_text(port).unwrap_or_default());
            }
            Ok(())
        })
    }
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command extraction owns State and IPC strings"
)]
pub fn dictation_start(
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    generation: u64,
    operation_id: u64,
    selection: Selection,
    session_id: String,
) -> Result<Status, String> {
    bridge.start_dictation(generation, operation_id, selection, &session_id)
}
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command extraction owns State"
)]
pub fn dictation_status(
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    generation: u64,
    operation_id: u64,
) -> Result<Status, String> {
    bridge.dictation_control(generation, operation_id, "status")
}
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command extraction owns State"
)]
pub fn dictation_finish(
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    generation: u64,
    operation_id: u64,
) -> Result<Status, String> {
    bridge.dictation_control(generation, operation_id, "finish")
}
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command extraction owns State"
)]
pub fn dictation_cancel(
    bridge: tauri::State<'_, Arc<crate::bridge::RuntimeBridge>>,
    generation: u64,
    operation_id: u64,
) -> Result<Status, String> {
    bridge.dictation_control(generation, operation_id, "cancel")
}

#[cfg(test)]
#[path = "dictation_tests.rs"]
mod tests;
