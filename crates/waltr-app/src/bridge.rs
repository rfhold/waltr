//! Rust-owned native client runtime and bounded `WebView` publication bridge.

use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use waltr_client::{
    ClientCommand, ClientCommandRequest, ClientRuntime, ClientSnapshot, ClientStateStream,
    ClientStateStreamEvent, NativeCredentialProvider, NativeGrpcTransport,
};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::Instrument as _;

use crate::{
    observability,
    server_controller::{EmbeddedServerController, ServerConnection, ServerPhase},
};

const CHANNEL_CAPACITY: usize = 16;

/// One generation-stamped item delivered to local app content.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeEvent<T> {
    /// Active runtime generation.
    pub generation: u64,
    /// Monotonic sequence within the generation.
    pub sequence: u64,
    /// Secret-free event body.
    pub payload: T,
}

/// Existing ordered events used to recover a listener sequence gap.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeRecovery<T> {
    /// Runtime generation that owns every returned event.
    pub generation: u64,
    /// Retained controls followed by the latest authoritative state as applicable.
    pub events: Vec<BridgeEvent<T>>,
}

/// Payloads emitted by the native bridge.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum BridgePayload {
    /// Shared Waltr client state.
    State { snapshot: Box<serde_json::Value> },
    /// Secret-free embedded platform startup lifecycle.
    Platform { startup: PlatformStartup },
}

/// Bounded startup states understood by the packaged frontend.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlatformStartup {
    Starting,
    NotificationPermissionRequired,
    FirstCatalogDownload,
    Ready,
    RecoverableFailure,
    Stopped,
}

/// Receiver pair enforcing initial-before-later delivery for one generation.
pub struct EventFeed<T> {
    generation: u64,
    initial: mpsc::Receiver<BridgeEvent<T>>,
    controls: mpsc::Receiver<BridgeEvent<T>>,
    updates: watch::Receiver<Option<BridgeEvent<T>>>,
    pending_control: Option<BridgeEvent<T>>,
    pending_update: Option<BridgeEvent<T>>,
}

impl<T> fmt::Debug for EventFeed<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventFeed")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<T: Clone + Send + Sync> EventFeed<T> {
    /// Return this feed's runtime generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Receive the uncoalesced initial event.
    pub async fn initial(&mut self) -> Option<BridgeEvent<T>> {
        self.initial.recv().await
    }

    /// Receive the next available item without allowing controls to overtake snapshots.
    pub async fn next_update(&mut self) -> Option<BridgeEvent<T>> {
        loop {
            if self.pending_control.is_none() {
                self.pending_control = self.controls.try_recv().ok();
            }
            if self.pending_update.is_none() && self.updates.has_changed().ok() == Some(true) {
                self.pending_update
                    .clone_from(&self.updates.borrow_and_update());
            }
            match (&self.pending_control, &self.pending_update) {
                (Some(control), Some(update)) if control.sequence < update.sequence => {
                    return self.pending_control.take();
                }
                (_, Some(_)) => return self.pending_update.take(),
                (Some(_), None) => return self.pending_control.take(),
                (None, None) => {}
            }
            tokio::select! {
                control = self.controls.recv() => self.pending_control = control,
                changed = self.updates.changed() => {
                    changed.ok()?;
                    self.pending_update.clone_from(&self.updates.borrow_and_update());
                }
            }
        }
    }
}

#[derive(Debug)]
struct RecoveryState<T> {
    generation: u64,
    latest_state: Option<BridgeEvent<T>>,
    controls: VecDeque<BridgeEvent<T>>,
}

impl<T> RecoveryState<T> {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            latest_state: None,
            controls: VecDeque::with_capacity(CHANNEL_CAPACITY),
        }
    }
}

struct Publisher<T> {
    active_generation: Arc<AtomicU64>,
    generation: u64,
    sequence: u64,
    initial_sent: bool,
    initial: mpsc::Sender<BridgeEvent<T>>,
    controls: mpsc::Sender<BridgeEvent<T>>,
    updates: watch::Sender<Option<BridgeEvent<T>>>,
    recovery: Arc<RwLock<RecoveryState<T>>>,
}

impl<T: Clone + Send + Sync> Publisher<T> {
    fn new(
        active_generation: Arc<AtomicU64>,
        generation: u64,
        recovery: Arc<RwLock<RecoveryState<T>>>,
    ) -> (Self, EventFeed<T>) {
        let (initial_sender, initial) = mpsc::channel(1);
        let (control_sender, controls) = mpsc::channel(CHANNEL_CAPACITY);
        let (updates, update_receiver) = watch::channel(None);
        (
            Self {
                active_generation,
                generation,
                sequence: 0,
                initial_sent: false,
                initial: initial_sender,
                controls: control_sender,
                updates,
                recovery,
            },
            EventFeed {
                generation,
                initial,
                controls,
                updates: update_receiver,
                pending_control: None,
                pending_update: None,
            },
        )
    }

    async fn publish_initial(&mut self, payload: T) -> bool {
        if self.initial_sent || !self.is_current() {
            return false;
        }
        let event = self.event(payload);
        self.initial_sent = true;
        self.set_latest(event.clone());
        self.initial.send(event).await.is_ok()
    }

    fn publish_state(&mut self, payload: T) -> Option<BridgeEvent<T>> {
        if !self.initial_sent || !self.is_current() {
            return None;
        }
        self.sequence = self.sequence.saturating_add(1);
        let event = self.event(payload);
        self.set_latest(event.clone());
        self.updates.send_replace(Some(event.clone()));
        Some(event)
    }

    async fn publish_control(&mut self, payload: T) -> Option<BridgeEvent<T>> {
        if !self.initial_sent || !self.is_current() {
            return None;
        }
        self.sequence = self.sequence.saturating_add(1);
        let event = self.event(payload);
        let mut recovery = self
            .recovery
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if recovery.controls.len() == CHANNEL_CAPACITY {
            drop(recovery.controls.pop_front());
        }
        recovery.controls.push_back(event.clone());
        drop(recovery);
        self.controls.send(event.clone()).await.ok()?;
        Some(event)
    }

    const fn event(&self, payload: T) -> BridgeEvent<T> {
        BridgeEvent {
            generation: self.generation,
            sequence: self.sequence,
            payload,
        }
    }

    fn is_current(&self) -> bool {
        self.active_generation.load(Ordering::Acquire) == self.generation
    }

    fn set_latest(&self, event: BridgeEvent<T>) {
        self.recovery
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest_state = Some(event);
    }
}

enum HostRequest {
    Dispatch {
        command: Box<ClientCommand>,
        reply: oneshot::Sender<Result<BridgeEvent<BridgePayload>, String>>,
    },
}

#[derive(Clone, Eq, PartialEq)]
struct RetainedAuthorizationAttempt {
    generation: u64,
    attempt_id: String,
    auth_url: String,
}

impl fmt::Debug for RetainedAuthorizationAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedAuthorizationAttempt")
            .field("generation", &self.generation)
            .field("attempt_id", &"<opaque>")
            .field("auth_url", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
struct PresentationState {
    endpoint: String,
    bearer: String,
    generation: u64,
    active_generation: Arc<AtomicU64>,
    authorization_attempt: Arc<RwLock<Option<RetainedAuthorizationAttempt>>>,
}

impl PresentationState {
    fn snapshot(&self, snapshot: &ClientSnapshot) -> serde_json::Value {
        self.retain_authorization_attempt(snapshot);
        let mut value = serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null);
        let private_auth_url = snapshot
            .auth
            .active_attempt
            .as_ref()
            .is_some_and(|attempt| auth_url_contains_endpoint(&attempt.auth_url, &self.endpoint));
        redact_snapshot_projection(&mut value, [&self.endpoint, &self.bearer], private_auth_url);
        value
    }

    fn retain_authorization_attempt(&self, snapshot: &ClientSnapshot) {
        if self.active_generation.load(Ordering::Acquire) != self.generation {
            return;
        }
        let attempt = snapshot.auth.active_attempt.as_ref().and_then(|attempt| {
            (!attempt.auth_url.is_empty() && attempt.lifecycle == "pending").then(|| {
                RetainedAuthorizationAttempt {
                    generation: self.generation,
                    attempt_id: attempt.attempt_id.clone(),
                    auth_url: attempt.auth_url.clone(),
                }
            })
        });
        *self
            .authorization_attempt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = attempt;
    }

    fn clear_authorization_attempt(&self) {
        let mut attempt = self
            .authorization_attempt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempt
            .as_ref()
            .is_some_and(|attempt| attempt.generation == self.generation)
        {
            *attempt = None;
        }
    }
}

fn auth_url_contains_endpoint(auth_url: &str, endpoint: &str) -> bool {
    auth_url.contains(endpoint)
        || url::Url::parse(auth_url)
            .ok()
            .is_some_and(|url| url.query_pairs().any(|(_, value)| value.contains(endpoint)))
}

fn redact_snapshot_projection<const N: usize>(
    value: &mut serde_json::Value,
    secrets: [&str; N],
    private_auth_url: bool,
) {
    if private_auth_url && let Some(auth_url) = value.pointer_mut("/auth/activeAttempt/authUrl") {
        *auth_url = serde_json::Value::String(String::new());
    }
    redact_value(value, secrets);
}

fn redact_value<const N: usize>(value: &mut serde_json::Value, secrets: [&str; N]) {
    match value {
        serde_json::Value::String(value) => {
            for secret in secrets {
                if !secret.is_empty() {
                    *value = value.replace(secret, "");
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_value(value, secrets);
            }
        }
        serde_json::Value::Object(values) => {
            let entries = std::mem::take(values);
            for (mut key, mut value) in entries {
                for secret in secrets {
                    if !secret.is_empty() {
                        key = key.replace(secret, "");
                    }
                }
                redact_value(&mut value, secrets);
                values.insert(key, value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

struct RunningGeneration {
    generation: u64,
    requests: mpsc::Sender<HostRequest>,
    stop: watch::Sender<bool>,
    #[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
    platform: watch::Sender<PlatformStartup>,
    recovery: Arc<RwLock<RecoveryState<BridgePayload>>>,
    thread: thread::JoinHandle<()>,
}

/// Process-level owner for the single active native client generation.
pub struct RuntimeBridge {
    active_generation: Arc<AtomicU64>,
    next_generation: AtomicU64,
    running: Mutex<Option<RunningGeneration>>,
    server: EmbeddedServerController,
    notification_permission_required: std::sync::atomic::AtomicBool,
    authorization_attempt: Arc<RwLock<Option<RetainedAuthorizationAttempt>>>,
    start_operation: Mutex<()>,
    #[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
    mobile_start_ticket: AtomicU64,
}

impl fmt::Debug for RuntimeBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeBridge")
            .field(
                "active_generation",
                &self.active_generation.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl RuntimeBridge {
    /// Create a bridge attached to the process-owned embedded server.
    #[must_use]
    pub fn new(server: EmbeddedServerController) -> Self {
        Self {
            active_generation: Arc::new(AtomicU64::new(0)),
            next_generation: AtomicU64::new(0),
            running: Mutex::new(None),
            server,
            notification_permission_required: std::sync::atomic::AtomicBool::new(false),
            authorization_attempt: Arc::new(RwLock::new(None)),
            start_operation: Mutex::new(()),
            mobile_start_ticket: AtomicU64::new(0),
        }
    }

    /// Replace any active generation and return its ordered event feed.
    #[cfg(not(target_os = "android"))]
    pub fn start(&self) -> Result<EventFeed<BridgePayload>, String> {
        let _operation = self
            .start_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.stop();
        self.start_generation()
    }

    /// Replace a mobile generation without synchronously joining its predecessor.
    #[cfg(target_os = "android")]
    pub fn start_nonblocking(&self, ticket: u64) -> Result<EventFeed<BridgePayload>, String> {
        let _operation = self
            .start_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.mobile_start_ticket.load(Ordering::Acquire) != ticket {
            return Err("native client start was cancelled".to_owned());
        }
        self.stop_nonblocking();
        if self.mobile_start_ticket.load(Ordering::Acquire) != ticket {
            return Err("native client start was cancelled".to_owned());
        }
        self.start_generation()
    }

    /// Make one Android lifecycle ticket the only ticket allowed to start a generation.
    #[cfg(any(target_os = "android", test))]
    pub fn authorize_start(&self, ticket: u64) {
        self.mobile_start_ticket.store(ticket, Ordering::Release);
    }

    /// Invalidate one in-progress Android lifecycle ticket without affecting its replacement.
    #[cfg(any(target_os = "android", test))]
    pub fn invalidate_start(&self, ticket: u64) {
        let _ = self.mobile_start_ticket.compare_exchange(
            ticket,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn start_generation(&self) -> Result<EventFeed<BridgePayload>, String> {
        let span = observability::operation_span("bridge.generation.start", "native_thread");
        let mut running_guard = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if running_guard.is_some() {
            return Err("native client runtime is already starting".to_owned());
        }
        let generation = self
            .next_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        self.active_generation.store(generation, Ordering::Release);
        let recovery = Arc::new(RwLock::new(RecoveryState::new(generation)));
        let (publisher, feed) = Publisher::new(
            Arc::clone(&self.active_generation),
            generation,
            Arc::clone(&recovery),
        );
        let (requests, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let (stop, stop_receiver) = watch::channel(false);
        let server = self.server.clone();
        let authorization_attempt = Arc::clone(&self.authorization_attempt);
        let initial = self.current_startup();
        let (platform, platform_receiver) = watch::channel(initial);
        let thread = thread::Builder::new()
            .name(format!("waltr-client-{generation}"))
            .spawn(move || {
                run_thread(
                    receiver,
                    stop_receiver,
                    platform_receiver,
                    publisher,
                    server,
                    authorization_attempt,
                    initial,
                );
            })
            .map_err(|_| {
                self.active_generation.store(0, Ordering::Release);
                "native client runtime could not start".to_owned()
            })?;
        *running_guard = Some(RunningGeneration {
            generation,
            requests,
            stop,
            platform,
            recovery,
            thread,
        });
        observability::record_outcome(&span, "ok");
        Ok(feed)
    }

    /// Dispatch one frontend command through the generation's serialized queue.
    pub async fn dispatch(
        &self,
        command: ClientCommand,
    ) -> Result<BridgeEvent<BridgePayload>, String> {
        let requests = self.requests()?;
        let (reply, result) = oneshot::channel();
        requests
            .send(HostRequest::Dispatch {
                command: Box::new(command),
                reply,
            })
            .await
            .map_err(|_| "native client runtime is stopped".to_owned())?;
        result
            .await
            .map_err(|_| "native client runtime stopped before dispatch completed".to_owned())?
    }

    /// Return retained controls and authoritative state without minting a sequence.
    pub fn recover(
        &self,
        after_sequence: Option<u64>,
    ) -> Result<BridgeRecovery<BridgePayload>, String> {
        let running_guard = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let running = running_guard
            .as_ref()
            .ok_or_else(|| "native client runtime is stopped".to_owned())?;
        let generation = running.generation;
        let recovery = Arc::clone(&running.recovery);
        drop(running_guard);
        let recovery = recovery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert_eq!(generation, recovery.generation);
        Ok(recovery_after(
            &recovery,
            self.active_generation.load(Ordering::Acquire),
            after_sequence,
        ))
    }

    /// Invalidate and synchronously dispose the active client generation.
    pub fn stop(&self) {
        let span = observability::operation_span("bridge.generation.stop", "native_thread");
        let running = self.signal_stop();
        let Some(running) = running else {
            observability::record_outcome(&span, "unchanged");
            return;
        };
        let outcome = if running.thread.join().is_ok() {
            "ok"
        } else {
            "panicked"
        };
        observability::record_outcome(&span, outcome);
    }

    /// Invalidate and signal the active generation, joining it away from a lifecycle callback.
    #[cfg(any(target_os = "android", test))]
    pub fn stop_nonblocking(&self) {
        let Some(running) = self.signal_stop() else {
            return;
        };
        drop(
            thread::Builder::new()
                .name("waltr-client-reaper".to_owned())
                .spawn(move || drop(running.thread.join())),
        );
    }

    /// Stop only the exact active generation without joining the caller.
    #[cfg(any(target_os = "android", test))]
    pub fn stop_generation_nonblocking(&self, generation: u64) -> bool {
        let running = {
            let mut running = self
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !running
                .as_ref()
                .is_some_and(|running| running.generation == generation)
            {
                return false;
            }
            running.take()
        };
        let Some(running) = running else {
            return false;
        };
        let _ = self.active_generation.compare_exchange(
            generation,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        running.stop.send_replace(true);
        self.clear_authorization_generation(generation);
        drop(
            thread::Builder::new()
                .name("waltr-client-reaper".to_owned())
                .spawn(move || drop(running.thread.join())),
        );
        true
    }

    /// Mark Android notification permission as denied without exposing platform details.
    #[cfg(any(target_os = "android", test))]
    pub fn notification_permission_required(&self, required: bool) {
        self.notification_permission_required
            .store(required, Ordering::Release);
        let startup = self.current_startup();
        if let Some(running) = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            running.platform.send_replace(startup);
        }
    }

    /// Resolve the exact current outbound authorization attempt inside Rust memory.
    pub(crate) fn authorization_attempt_url(&self, attempt_id: &str) -> Result<String, String> {
        let generation = self.active_generation.load(Ordering::Acquire);
        if generation == 0 {
            return Err("authorization attempt is unavailable".to_owned());
        }
        let auth_url = self
            .authorization_attempt
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|attempt| attempt.generation == generation && attempt.attempt_id == attempt_id)
            .map(|attempt| attempt.auth_url.clone())
            .ok_or_else(|| "authorization attempt is unavailable".to_owned())?;
        if self.active_generation.load(Ordering::Acquire) != generation {
            return Err("authorization attempt is unavailable".to_owned());
        }
        Ok(auth_url)
    }

    fn current_startup(&self) -> PlatformStartup {
        if self
            .notification_permission_required
            .load(Ordering::Acquire)
        {
            return PlatformStartup::NotificationPermissionRequired;
        }
        match self.server.phase() {
            ServerPhase::Running => PlatformStartup::Ready,
            ServerPhase::Failed => PlatformStartup::RecoverableFailure,
            ServerPhase::Starting if self.server.first_catalog_download() => {
                PlatformStartup::FirstCatalogDownload
            }
            ServerPhase::Stopped | ServerPhase::Starting | ServerPhase::Stopping => {
                PlatformStartup::Starting
            }
        }
    }

    fn requests(&self) -> Result<mpsc::Sender<HostRequest>, String> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|running| running.requests.clone())
            .ok_or_else(|| "native client runtime is stopped".to_owned())
    }

    fn signal_stop(&self) -> Option<RunningGeneration> {
        let running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(running) = &running {
            let _ = self.active_generation.compare_exchange(
                running.generation,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            running.stop.send_replace(true);
            self.clear_authorization_generation(running.generation);
        }
        running
    }

    fn clear_authorization_generation(&self, generation: u64) {
        let mut attempt = self
            .authorization_attempt
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempt
            .as_ref()
            .is_some_and(|attempt| attempt.generation == generation)
        {
            *attempt = None;
        }
    }

    /// Return the current generation for event delivery validation.
    #[must_use]
    pub fn is_current(&self, generation: u64) -> bool {
        self.active_generation.load(Ordering::Acquire) == generation
    }
}

impl Drop for RuntimeBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_thread(
    requests: mpsc::Receiver<HostRequest>,
    stop: watch::Receiver<bool>,
    platform: watch::Receiver<PlatformStartup>,
    publisher: Publisher<BridgePayload>,
    server: EmbeddedServerController,
    authorization_attempt: Arc<RwLock<Option<RetainedAuthorizationAttempt>>>,
    initial: PlatformStartup,
) {
    let runtime_span = observability::operation_span("native.runtime", "native_thread");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread Tokio runtime");
    let local = tokio::task::LocalSet::new();
    local.block_on(
        &runtime,
        run_generation(
            requests,
            stop,
            platform,
            publisher,
            server,
            authorization_attempt,
            initial,
        )
        .instrument(runtime_span.clone()),
    );
    observability::record_outcome(&runtime_span, "stopped");
}

#[allow(
    clippy::future_not_send,
    reason = "ClientStateStream stays on one LocalSet"
)]
async fn run_generation(
    mut requests: mpsc::Receiver<HostRequest>,
    mut stop: watch::Receiver<bool>,
    mut platform: watch::Receiver<PlatformStartup>,
    mut publisher: Publisher<BridgePayload>,
    server: EmbeddedServerController,
    authorization_attempt: Arc<RwLock<Option<RetainedAuthorizationAttempt>>>,
    initial: PlatformStartup,
) {
    let mut initial_sent = false;
    let mut permission_required = initial == PlatformStartup::NotificationPermissionRequired;
    if let Some(connection) = gated_connection(initial, server.connection()) {
        match run_connected_generation(
            &mut requests,
            &mut stop,
            &mut platform,
            &mut publisher,
            connection,
            Arc::clone(&authorization_attempt),
            true,
        )
        .await
        {
            ConnectedExit::Stopped => return,
            ConnectedExit::PermissionRequired => {
                initial_sent = true;
                permission_required = true;
            }
        }
    }
    if !initial_sent
        && !publisher
            .publish_initial(BridgePayload::Platform { startup: initial })
            .await
    {
        return;
    }
    let mut reported_first_catalog_download = initial == PlatformStartup::FirstCatalogDownload;
    let mut reported_failure = initial == PlatformStartup::RecoverableFailure;
    loop {
        if !permission_required && let Some(connection) = server.connection() {
            if connection.first_catalog_download && !reported_first_catalog_download {
                publisher
                    .publish_control(BridgePayload::Platform {
                        startup: PlatformStartup::FirstCatalogDownload,
                    })
                    .await;
                reported_first_catalog_download = true;
            }
            publisher
                .publish_control(BridgePayload::Platform {
                    startup: PlatformStartup::Ready,
                })
                .await;
            match run_connected_generation(
                &mut requests,
                &mut stop,
                &mut platform,
                &mut publisher,
                connection,
                Arc::clone(&authorization_attempt),
                false,
            )
            .await
            {
                ConnectedExit::Stopped => {
                    publisher.publish_state(BridgePayload::Platform {
                        startup: PlatformStartup::Stopped,
                    });
                    return;
                }
                ConnectedExit::PermissionRequired => {
                    permission_required = true;
                    continue;
                }
            }
        }
        if server.phase() == ServerPhase::Failed && !reported_failure {
            publisher.publish_state(BridgePayload::Platform {
                startup: PlatformStartup::RecoverableFailure,
            });
            reported_failure = true;
        } else if server.phase() != ServerPhase::Failed {
            reported_failure = false;
        }
        if server.first_catalog_download() && !reported_first_catalog_download {
            publisher
                .publish_control(BridgePayload::Platform {
                    startup: PlatformStartup::FirstCatalogDownload,
                })
                .await;
            reported_first_catalog_download = true;
        }
        tokio::select! {
            _ = stop.changed() => return,
            changed = platform.changed() => {
                if changed.is_err() {
                    return;
                }
                let startup = *platform.borrow_and_update();
                publisher.publish_control(BridgePayload::Platform { startup }).await;
                permission_required = startup == PlatformStartup::NotificationPermissionRequired;
            }
            request = requests.recv() => match request {
                Some(HostRequest::Dispatch { reply, .. }) => {
                    drop(reply.send(Err("embedded server is not ready".to_owned())));
                }
                None => return,
            },
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
}

fn gated_connection<T>(startup: PlatformStartup, connection: Option<T>) -> Option<T> {
    (startup != PlatformStartup::NotificationPermissionRequired)
        .then_some(connection)
        .flatten()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectedExit {
    Stopped,
    PermissionRequired,
}

#[allow(
    clippy::future_not_send,
    reason = "ClientStateStream stays on one LocalSet"
)]
async fn run_connected_generation(
    requests: &mut mpsc::Receiver<HostRequest>,
    stop: &mut watch::Receiver<bool>,
    platform: &mut watch::Receiver<PlatformStartup>,
    publisher: &mut Publisher<BridgePayload>,
    connection: ServerConnection,
    authorization_attempt: Arc<RwLock<Option<RetainedAuthorizationAttempt>>>,
    initial: bool,
) -> ConnectedExit {
    let presentation = PresentationState {
        endpoint: connection.endpoint.clone(),
        bearer: connection.bearer.clone(),
        generation: publisher.generation,
        active_generation: Arc::clone(&publisher.active_generation),
        authorization_attempt,
    };
    let credentials = NativeCredentialProvider::new();
    if credentials
        .replace_bearer_token(&connection.bearer)
        .is_err()
    {
        publisher.publish_state(BridgePayload::Platform {
            startup: PlatformStartup::RecoverableFailure,
        });
        return ConnectedExit::Stopped;
    }
    let transport =
        NativeGrpcTransport::with_credential_provider(connection.endpoint, credentials.clone());
    let runtime = ClientRuntime::with_transport(transport);
    let mut stream =
        runtime.into_state_stream(|delay_ms| tokio::time::sleep(Duration::from_millis(delay_ms)));
    let initial_payload = BridgePayload::State {
        snapshot: Box::new(presentation.snapshot(&stream.snapshot())),
    };
    if initial {
        if !publisher.publish_initial(initial_payload).await {
            let _ = credentials.clear_bearer_token();
            return ConnectedExit::Stopped;
        }
    } else {
        publisher.publish_state(initial_payload);
    }

    let exit = loop {
        tokio::select! {
            _ = stop.changed() => break ConnectedExit::Stopped,
            changed = platform.changed() => {
                if changed.is_err() {
                    break ConnectedExit::Stopped;
                }
                let startup = *platform.borrow_and_update();
                publisher.publish_control(BridgePayload::Platform { startup }).await;
                if startup == PlatformStartup::NotificationPermissionRequired {
                    break ConnectedExit::PermissionRequired;
                }
            }
            request = requests.recv() => match request {
                Some(HostRequest::Dispatch { command, reply }) => {
                    let dispatch = dispatch_command(&mut stream, *command);
                    tokio::pin!(dispatch);
                    let mut permission_interrupted = false;
                    let result = loop {
                        tokio::select! {
                            biased;
                            _ = stop.changed() => break None,
                            changed = platform.changed() => {
                                if changed.is_err() {
                                    break None;
                                }
                                let startup = *platform.borrow_and_update();
                                publisher.publish_control(BridgePayload::Platform { startup }).await;
                                if startup == PlatformStartup::NotificationPermissionRequired {
                                    permission_interrupted = true;
                                    break None;
                                }
                            }
                            result = &mut dispatch => break Some(result),
                        }
                    };
                    let Some(result) = result else {
                        drop(reply.send(Err("native client generation was stopped".to_owned())));
                        break if permission_interrupted {
                            ConnectedExit::PermissionRequired
                        } else {
                            ConnectedExit::Stopped
                        };
                    };
                    let result = result.and_then(|snapshot| {
                        publisher
                            .publish_state(BridgePayload::State {
                                snapshot: Box::new(presentation.snapshot(&snapshot)),
                            })
                            .ok_or_else(|| "native client generation was replaced".to_owned())
                    });
                    drop(reply.send(result));
                }
                None => break ConnectedExit::Stopped,
            },
            snapshot = stream.next() => {
                publisher.publish_state(BridgePayload::State {
                    snapshot: Box::new(presentation.snapshot(&snapshot)),
                });
            }
        }
    };
    let _ = credentials.clear_bearer_token();
    presentation.clear_authorization_attempt();
    exit
}

#[cfg(test)]
async fn cancel_on_stop<T>(
    stop: &mut watch::Receiver<bool>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = stop.changed() => None,
        result = work => Some(result),
    }
}

#[allow(
    clippy::future_not_send,
    reason = "ClientStateStream stays on one LocalSet"
)]
async fn dispatch_command(
    stream: &mut ClientStateStream,
    command: ClientCommand,
) -> Result<ClientSnapshot, String> {
    let (reply, result) = futures_channel::oneshot::channel();
    let event = stream
        .next_or_dispatch(async move { Some(ClientCommandRequest::with_result(command, reply)) })
        .await;
    let ClientStateStreamEvent::Snapshot(_) = event else {
        return Err("native client command queue closed".to_owned());
    };
    result
        .await
        .map_err(|_| "native client dispatch result was canceled".to_owned())?
        .map_err(|_| "native client command failed".to_owned())
}

fn recovery_after<T: Clone>(
    recovery: &RecoveryState<T>,
    authoritative_generation: u64,
    after_sequence: Option<u64>,
) -> BridgeRecovery<T> {
    if recovery.generation != authoritative_generation {
        return BridgeRecovery {
            generation: authoritative_generation,
            events: Vec::new(),
        };
    }
    let mut events = recovery
        .controls
        .iter()
        .filter(|event| after_sequence.is_none_or(|sequence| event.sequence > sequence))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(state) = &recovery.latest_state
        && after_sequence.is_none_or(|sequence| state.sequence > sequence)
        && events.iter().all(|event| event.sequence != state.sequence)
    {
        events.push(state.clone());
    }
    events.sort_unstable_by_key(|event| event.sequence);
    BridgeRecovery {
        generation: recovery.generation,
        events,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn initial_is_first_and_later_updates_coalesce_to_latest() {
        let active = Arc::new(AtomicU64::new(7));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(7)));
        let (mut publisher, mut feed) = Publisher::new(active, 7, recovery);
        assert!(publisher.publish_initial("initial").await);
        publisher.publish_state("one");
        publisher.publish_state("latest");
        assert_eq!(feed.initial().await.unwrap().payload, "initial");
        let update = feed.next_update().await.unwrap();
        assert_eq!((update.sequence, update.payload), (2, "latest"));
    }

    #[tokio::test]
    async fn controls_and_latest_state_recover_in_sequence_order() {
        let active = Arc::new(AtomicU64::new(3));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(3)));
        let (mut publisher, _feed) = Publisher::new(active, 3, Arc::clone(&recovery));
        publisher.publish_initial("starting").await;
        publisher.publish_control("control").await.unwrap();
        publisher.publish_state("state").unwrap();
        let recovered = recovery_after(&recovery.read().unwrap(), 3, Some(0));
        assert_eq!(
            recovered
                .events
                .iter()
                .map(|event| (event.sequence, event.payload))
                .collect::<Vec<_>>(),
            [(1, "control"), (2, "state")]
        );
    }

    #[test]
    fn platform_payloads_are_secret_free_fixed_values() {
        for startup in [
            PlatformStartup::Starting,
            PlatformStartup::NotificationPermissionRequired,
            PlatformStartup::FirstCatalogDownload,
            PlatformStartup::Ready,
            PlatformStartup::RecoverableFailure,
            PlatformStartup::Stopped,
        ] {
            let json = serde_json::to_string(&BridgePayload::Platform { startup }).unwrap();
            assert!(json.starts_with(r#"{"type":"platform","startup":"#));
            for forbidden in ["token", "bearer", "endpoint", "127.0.0.1"] {
                assert!(!json.contains(forbidden));
            }
        }
    }

    #[test]
    fn stale_generation_recovery_returns_no_old_events() {
        let recovery = RecoveryState {
            generation: 4,
            latest_state: Some(BridgeEvent {
                generation: 4,
                sequence: 1,
                payload: "old",
            }),
            controls: VecDeque::new(),
        };
        let recovered = recovery_after(&recovery, 5, None);
        assert_eq!(recovered.generation, 5);
        assert!(recovered.events.is_empty());
    }

    #[tokio::test]
    async fn stale_generation_feed_cannot_deliver_initial_event() {
        let active = Arc::new(AtomicU64::new(42));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(41)));
        let (mut publisher, mut feed) = Publisher::new(active, 41, recovery);
        assert!(!publisher.publish_initial("stale").await);
        drop(publisher);
        assert!(feed.initial().await.is_none());
    }

    #[test]
    #[cfg(not(target_os = "android"))]
    fn desktop_start_and_stop_remain_synchronous() {
        let bridge = RuntimeBridge::new(EmbeddedServerController::default());
        let feed = bridge.start().expect("desktop generation starts");
        let generation = feed.generation();
        assert!(bridge.is_current(generation));
        bridge.stop();
        assert!(!bridge.is_current(generation));
        assert!(bridge.running.lock().unwrap().is_none());
    }

    #[test]
    fn fresh_permission_required_generation_gates_existing_connection() {
        let connection = ServerConnection {
            generation: 4,
            endpoint: "http://127.0.0.1:43123".to_owned(),
            bearer: "local-bearer".to_owned(),
            first_catalog_download: false,
        };
        assert!(
            gated_connection(
                PlatformStartup::NotificationPermissionRequired,
                Some(connection.clone())
            )
            .is_none()
        );
        assert_eq!(
            gated_connection(PlatformStartup::Ready, Some(connection))
                .expect("ready generation may connect")
                .generation,
            4
        );
    }

    #[test]
    fn complete_state_payload_removes_endpoint_and_bearer_everywhere() {
        let endpoint = "http://127.0.0.1:43123";
        let bearer = "SUPER_SECRET_LOCAL_BEARER";
        let private_auth_url = concat!(
            "https://provider.example/authorize?client_id=waltr&redirect_uri=",
            "http%3A%2F%2F127.0.0.1%3A43123%2Foauth%2Fcallback&state=opaque"
        );
        let mut snapshot = serde_json::json!({
            "auth": {
                "activeAttempt": {
                    "attemptId": "attempt-current",
                    "authUrl": private_auth_url
                }
            },
            "config": {
                "effective": { "url": format!("{endpoint}/mcp") },
                "baseline": [{ "endpoint": endpoint }]
            },
            "operations": {
                format!("patch:{endpoint}"): {
                    "error": format!("request to {endpoint} used {bearer}")
                }
            },
            "resources": [endpoint, bearer]
        });
        assert!(auth_url_contains_endpoint(private_auth_url, endpoint));
        redact_snapshot_projection(&mut snapshot, [endpoint, bearer], true);
        let payload = BridgePayload::State {
            snapshot: Box::new(snapshot),
        };
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains(endpoint));
        assert!(!serialized.contains(bearer));
        assert!(!serialized.contains(private_auth_url));
        assert_eq!(
            payload_snapshot(&payload).pointer("/auth/activeAttempt/authUrl"),
            Some(&serde_json::Value::String(String::new()))
        );
        assert!(!serialized.to_ascii_lowercase().contains("redacted"));
    }

    #[test]
    fn raw_authorization_attempt_is_retained_and_cleared_by_snapshot_lifecycle() {
        let endpoint = "http://127.0.0.1:43123";
        let auth_url = concat!(
            "https://provider.example/authorize?redirect_uri=",
            "http%3A%2F%2F127.0.0.1%3A43123%2Foauth%2Fcallback"
        );
        let active_generation = Arc::new(AtomicU64::new(9));
        let authorization_attempt = Arc::new(RwLock::new(None));
        let presentation = PresentationState {
            endpoint: endpoint.to_owned(),
            bearer: "local-bearer".to_owned(),
            generation: 9,
            active_generation,
            authorization_attempt: Arc::clone(&authorization_attempt),
        };
        let mut snapshot = ClientRuntime::for_endpoint("http://unused.invalid").snapshot();
        snapshot.auth.active_attempt = Some(waltr_client::AuthAttemptSnapshot {
            target: waltr_client::ClientAuthTarget {
                kind: "provider".to_owned(),
                key: "provider:test".to_owned(),
            },
            attempt_id: "attempt-current".to_owned(),
            auth_url: auth_url.to_owned(),
            verification_url: String::new(),
            user_code: String::new(),
            expires_at_ms: 1,
            poll_interval_seconds: 0,
            messages: Vec::new(),
            completion_mode: "serverCallback".to_owned(),
            registration_method: String::new(),
            lifecycle: "pending".to_owned(),
            failure_reason: String::new(),
        });

        let presented = presentation.snapshot(&snapshot);
        assert_eq!(
            presented.pointer("/auth/activeAttempt/authUrl"),
            Some(&serde_json::Value::String(String::new()))
        );
        assert_eq!(
            authorization_attempt.read().unwrap().as_ref(),
            Some(&RetainedAuthorizationAttempt {
                generation: 9,
                attempt_id: "attempt-current".to_owned(),
                auth_url: auth_url.to_owned(),
            })
        );

        snapshot.auth.active_attempt.as_mut().unwrap().lifecycle = "succeeded".to_owned();
        drop(presentation.snapshot(&snapshot));
        assert!(authorization_attempt.read().unwrap().is_none());

        snapshot.auth.active_attempt = None;
        drop(presentation.snapshot(&snapshot));
        assert!(authorization_attempt.read().unwrap().is_none());
    }

    fn payload_snapshot(payload: &BridgePayload) -> &serde_json::Value {
        let BridgePayload::State { snapshot } = payload else {
            panic!("state payload");
        };
        snapshot
    }

    #[tokio::test]
    async fn pending_dispatch_is_cancelled_when_generation_stops() {
        let (stop, mut receiver) = watch::channel(false);
        stop.send_replace(true);
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            cancel_on_stop(&mut receiver, std::future::pending::<()>()),
        )
        .await
        .expect("stop selection must be prompt");
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn platform_transition_and_nonblocking_stop_reach_pending_generation() {
        let bridge = RuntimeBridge::new(EmbeddedServerController::default());
        bridge.active_generation.store(1, Ordering::Release);
        let (requests, _request_receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let (stop, mut stop_receiver) = watch::channel(false);
        let (platform, mut platform_receiver) = watch::channel(PlatformStartup::Ready);
        let recovery = Arc::new(RwLock::new(RecoveryState::new(1)));
        let (finished, finished_receiver) = std::sync::mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                drop(stop_receiver.changed().await);
                assert!(finished.send(()).is_ok());
            });
        });
        *bridge.running.lock().unwrap() = Some(RunningGeneration {
            generation: 1,
            requests,
            stop,
            platform,
            recovery,
            thread,
        });
        bridge.authorize_start(10);
        bridge.invalidate_start(9);
        assert_eq!(bridge.mobile_start_ticket.load(Ordering::Acquire), 10);
        bridge.invalidate_start(10);
        assert_eq!(bridge.mobile_start_ticket.load(Ordering::Acquire), 0);
        let auth_url = "https://provider.example/authorize?state=opaque";
        *bridge.authorization_attempt.write().unwrap() = Some(RetainedAuthorizationAttempt {
            generation: 1,
            attempt_id: "attempt-current".to_owned(),
            auth_url: auth_url.to_owned(),
        });
        assert_eq!(
            bridge.authorization_attempt_url("attempt-current").unwrap(),
            auth_url
        );
        assert_eq!(
            bridge
                .authorization_attempt_url("attempt-wrong")
                .unwrap_err(),
            "authorization attempt is unavailable"
        );
        bridge.active_generation.store(2, Ordering::Release);
        assert_eq!(
            bridge
                .authorization_attempt_url("attempt-current")
                .unwrap_err(),
            "authorization attempt is unavailable"
        );
        bridge.active_generation.store(1, Ordering::Release);

        assert_eq!(
            *platform_receiver.borrow_and_update(),
            PlatformStartup::Ready
        );
        bridge.notification_permission_required(true);
        assert!(platform_receiver.has_changed().unwrap());
        assert_eq!(
            *platform_receiver.borrow_and_update(),
            PlatformStartup::NotificationPermissionRequired
        );
        assert!(!bridge.stop_generation_nonblocking(2));
        assert!(bridge.is_current(1));

        let started = Instant::now();
        bridge.stop_nonblocking();
        assert!(started.elapsed() < Duration::from_millis(100));
        finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("pending generation receives stop");
        assert!(bridge.authorization_attempt.read().unwrap().is_none());
    }
}
