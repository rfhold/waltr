//! Rust-owned native client runtime and bounded `WebView` publication bridge.

use std::{
    collections::VecDeque,
    fmt,
    future::pending,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use arc_swap::ArcSwapOption;
use waltr_client::{
    ClientCommand, ClientCommandRequest, ClientRuntime, ClientSnapshot, ClientStateStream,
    ClientStateStreamEvent, NativeAccessAction, NativeAccessConfig, NativeAccessCoordinator,
    NativeAccessOutput, NativeAccessProgress, NativeAuthorizationCallback, NativeCallbackPolicy,
    NativeCredentialProvider, NativeGrpcTransport, NativeLoopbackPolicy,
};
use serde::Serialize;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::Instrument as _;

use crate::{observability, policy::external_https_url};

const ENDPOINT: &str = "https://preview-waltr.holdenitdown.net";
const NATIVE_CLIENT_ID: &str = "waltr-preview-native";
const AUDIENCE: &str = "waltr-preview-api";
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
    State { snapshot: Box<ClientSnapshot> },
    /// Native authorization lifecycle suitable for presentation.
    Access { progress: AccessProgress },
}

/// Native access presentation without credentials.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AccessProgress {
    /// Secret-free progress supplied by the native access coordinator.
    Native { progress: AccessPresentation },
    /// Logout presentation barrier emitted before the logout state snapshot.
    LoggingOut,
}

/// Tauri-safe native access progress with authorization URLs and callback state removed.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AccessPresentation {
    /// Metadata discovery is in progress.
    Discovering,
    /// Rust opened the system browser and is awaiting its callback.
    AwaitingBrowser { timeout_millis: u64 },
    /// The authorization code is being exchanged.
    Exchanging,
    /// The access token is being refreshed.
    Refreshing,
    /// Refresh completed.
    RefreshSucceeded,
    /// Credentials were installed.
    Succeeded,
    /// The flow failed with safe presentation text.
    Failed { error: String },
    /// The flow was canceled.
    Canceled,
}

impl From<NativeAccessProgress> for AccessPresentation {
    fn from(progress: NativeAccessProgress) -> Self {
        match progress {
            NativeAccessProgress::Discovering => Self::Discovering,
            NativeAccessProgress::AwaitingBrowser { timeout_millis, .. } => {
                Self::AwaitingBrowser { timeout_millis }
            }
            NativeAccessProgress::Exchanging => Self::Exchanging,
            NativeAccessProgress::Refreshing => Self::Refreshing,
            NativeAccessProgress::RefreshSucceeded => Self::RefreshSucceeded,
            NativeAccessProgress::Succeeded => Self::Succeeded,
            NativeAccessProgress::Failed { error } => Self::Failed { error },
            NativeAccessProgress::Canceled => Self::Canceled,
        }
    }
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

    /// Receive the uncoalesced initial snapshot.
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
                control = self.controls.recv() => {
                    self.pending_control = control;
                },
                changed = self.updates.changed() => {
                    changed.ok()?;
                    self.pending_update
                        .clone_from(&self.updates.borrow_and_update());
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
        self.set_latest_state(event.clone());
        self.initial.send(event).await.is_ok()
    }

    fn publish(&mut self, payload: T) -> bool {
        if !self.initial_sent || !self.is_current() {
            return false;
        }
        self.sequence = self.sequence.saturating_add(1);
        let event = self.event(payload);
        self.updates.send_replace(Some(event));
        true
    }

    fn publish_state(&mut self, payload: T) -> Option<BridgeEvent<T>> {
        if !self.initial_sent || !self.is_current() {
            return None;
        }
        self.sequence = self.sequence.saturating_add(1);
        let event = self.event(payload);
        self.set_latest_state(event.clone());
        self.updates.send_replace(Some(event.clone()));
        Some(event)
    }

    async fn publish_control(
        &mut self,
        payload: T,
        authoritative_state: bool,
    ) -> Option<BridgeEvent<T>> {
        if !self.initial_sent || !self.is_current() {
            return None;
        }
        self.sequence = self.sequence.saturating_add(1);
        let event = self.event(payload);
        self.record_control(event.clone(), authoritative_state);
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

    fn set_latest_state(&self, event: BridgeEvent<T>) {
        self.recovery
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest_state = Some(event);
    }

    fn record_control(&self, event: BridgeEvent<T>, authoritative_state: bool) {
        let mut recovery = self
            .recovery
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if authoritative_state {
            recovery.latest_state = Some(event.clone());
        }
        if recovery.controls.len() == CHANNEL_CAPACITY {
            drop(recovery.controls.pop_front());
        }
        recovery.controls.push_back(event);
    }
}

enum HostRequest {
    Dispatch {
        command: Box<ClientCommand>,
        reply: oneshot::Sender<Result<BridgeEvent<BridgePayload>, String>>,
    },
    Access {
        action: NativeAccessAction,
        reply: oneshot::Sender<Result<Option<BridgeEvent<BridgePayload>>, String>>,
    },
    Stop,
}

#[derive(Default)]
struct BrowserHandoff {
    active: Option<HandoffAttempt>,
}

struct HandoffAttempt {
    generation: u64,
    phase: HandoffPhase,
    admission: Arc<CallbackAdmission>,
}

struct CallbackAdmission {
    generation: u64,
    ready: AtomicBool,
    callback: ArcSwapOption<GenerationCallback>,
}

struct GenerationCallback {
    generation: u64,
    callback: NativeAuthorizationCallback,
}

impl CallbackAdmission {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            ready: AtomicBool::new(true),
            callback: ArcSwapOption::empty(),
        }
    }

    fn clear(&self) {
        self.ready.store(false, Ordering::Release);
        drop(self.callback.swap(None));
    }
}

#[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
#[derive(Clone, Copy, Eq, PartialEq)]
enum HandoffPhase {
    AwaitingInitialPause,
    PreservedSuspended,
    Resumed,
}

#[cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]
impl BrowserHandoff {
    fn begin(&mut self, generation: u64, admission: Arc<CallbackAdmission>) {
        self.active = Some(HandoffAttempt {
            generation,
            phase: HandoffPhase::AwaitingInitialPause,
            admission,
        });
    }

    fn suspend(&mut self, generation: u64) -> bool {
        let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.generation == generation)
        else {
            return false;
        };
        if active.phase != HandoffPhase::AwaitingInitialPause {
            return false;
        }
        active.phase = HandoffPhase::PreservedSuspended;
        active.admission.ready.store(false, Ordering::Release);
        true
    }

    fn resume(&mut self, generation: u64) -> Option<Arc<CallbackAdmission>> {
        let active = self.active.as_mut().filter(|active| {
            active.generation == generation && active.phase == HandoffPhase::PreservedSuspended
        })?;
        active.phase = HandoffPhase::Resumed;
        active.admission.ready.store(true, Ordering::Release);
        Some(Arc::clone(&active.admission))
    }

    fn finish(&mut self, generation: u64) -> Option<(bool, Arc<CallbackAdmission>)> {
        let active = self
            .active
            .take_if(|active| active.generation == generation)?;
        Some((
            active.phase == HandoffPhase::PreservedSuspended,
            active.admission,
        ))
    }

    fn clear(&mut self, generation: u64) -> Option<Arc<CallbackAdmission>> {
        self.active
            .take_if(|active| active.generation == generation)
            .map(|active| active.admission)
    }
}

type OpenUrl = Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct RunningGeneration {
    generation: u64,
    requests: mpsc::Sender<HostRequest>,
    stop: watch::Sender<bool>,
    credentials: NativeCredentialProvider,
    recovery: Arc<RwLock<RecoveryState<BridgePayload>>>,
    thread: thread::JoinHandle<()>,
}

/// Process-level owner for the single active native client generation.
pub struct RuntimeBridge {
    active_generation: Arc<AtomicU64>,
    next_generation: AtomicU64,
    running: Mutex<Option<RunningGeneration>>,
    handoff: Arc<Mutex<BrowserHandoff>>,
    callback_admission: Arc<ArcSwapOption<CallbackAdmission>>,
    callback_notify: Arc<Notify>,
    open_url: OpenUrl,
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
    /// Create a bridge whose validated authorization URLs are opened by Rust.
    #[must_use]
    pub fn new(open_url: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            active_generation: Arc::new(AtomicU64::new(0)),
            next_generation: AtomicU64::new(0),
            running: Mutex::new(None),
            handoff: Arc::new(Mutex::new(BrowserHandoff::default())),
            callback_admission: Arc::new(ArcSwapOption::empty()),
            callback_notify: Arc::new(Notify::new()),
            open_url: Arc::new(open_url),
        }
    }

    /// Replace any active generation and return its ordered event feed.
    pub fn start(&self) -> Result<EventFeed<BridgePayload>, String> {
        let span = observability::operation_span("bridge.generation.start", "native_thread");
        self.stop();
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
        let credentials = NativeCredentialProvider::new();
        let thread_credentials = credentials.clone();
        let open_url = Arc::clone(&self.open_url);
        let handoff = Arc::clone(&self.handoff);
        let callback_admission = Arc::clone(&self.callback_admission);
        let callback_notify = Arc::clone(&self.callback_notify);
        let thread = match thread::Builder::new()
            .name(format!("waltr-client-{generation}"))
            .spawn(move || {
                let thread_span =
                    observability::operation_span("bridge.generation.run", "native_thread");
                let _entered = thread_span.enter();
                run_thread(
                    receiver,
                    callback_admission,
                    callback_notify,
                    stop_receiver,
                    publisher,
                    thread_credentials,
                    open_url,
                    handoff,
                );
                observability::record_outcome(&thread_span, "stopped");
            }) {
            Ok(thread) => thread,
            Err(error) => {
                self.active_generation.store(0, Ordering::Release);
                observability::record_outcome(&span, "error");
                observability::error_event("native_thread.spawn", "native_thread", "error");
                return Err(format!("failed to start native client runtime: {error}"));
            }
        };

        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(RunningGeneration {
            generation,
            requests,
            stop,
            credentials,
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
        if is_native_access_command(&command) {
            return Err("native Waltr access commands require the dedicated access IPC".to_owned());
        }
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

    /// Queue a native access action.
    pub async fn access(
        &self,
        action: NativeAccessAction,
    ) -> Result<Option<BridgeEvent<BridgePayload>>, String> {
        let requests = self.requests()?;
        let (reply, result) = oneshot::channel();
        requests
            .send(HostRequest::Access { action, reply })
            .await
            .map_err(|_| "native client runtime is stopped".to_owned())?;
        result.await.map_err(|_| {
            "native client runtime stopped before access action completed".to_owned()
        })?
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

        let recovery_guard = recovery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert_eq!(generation, recovery_guard.generation);
        let response = recovery_after(
            &recovery_guard,
            self.active_generation.load(Ordering::Acquire),
            after_sequence,
        );
        drop(recovery_guard);
        Ok(response)
    }

    /// Invalidate and synchronously dispose the active generation.
    pub fn stop(&self) {
        let span = observability::operation_span("bridge.generation.stop", "native_thread");
        if let Some(admission) = self.callback_admission.swap(None) {
            admission.clear();
            let detailed = self
                .handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear(admission.generation);
            if let Some(detailed) = detailed {
                detailed.clear();
            }
        }
        self.active_generation.store(0, Ordering::Release);
        let running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(running) = running else {
            observability::record_outcome(&span, "unchanged");
            return;
        };
        let _ = running.credentials.clear_bearer_token();
        observability::lifecycle_event("credential.clear", "native_thread", "ok");
        running.stop.send_replace(true);
        let outcome = if running.thread.join().is_ok() {
            "ok"
        } else {
            observability::error_event("native_thread.join", "native_thread", "panicked");
            "panicked"
        };
        observability::record_outcome(&span, outcome);
    }

    fn requests(&self) -> Result<mpsc::Sender<HostRequest>, String> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|running| running.requests.clone())
            .ok_or_else(|| "native client runtime is stopped".to_owned())
    }

    /// Return the current generation for event delivery validation.
    #[must_use]
    pub fn is_current(&self, generation: u64) -> bool {
        self.active_generation.load(Ordering::Acquire) == generation
    }

    /// Nonblocking delivery of one opaque Android callback to the active attempt.
    #[cfg(any(target_os = "android", test))]
    pub fn authorization_callback(&self, callback: NativeAuthorizationCallback) -> bool {
        let Some(admission) = self.callback_admission.load_full() else {
            return false;
        };
        self.admit_callback_to(&admission, callback)
    }

    #[cfg(any(target_os = "android", test))]
    fn admit_callback_to(
        &self,
        admission: &Arc<CallbackAdmission>,
        callback: NativeAuthorizationCallback,
    ) -> bool {
        let generation = admission.generation;
        if !self.is_current(generation) {
            return false;
        }
        self.store_callback_to_admission(admission, callback)
    }

    #[cfg(any(target_os = "android", test))]
    fn store_callback_to_admission(
        &self,
        admission: &Arc<CallbackAdmission>,
        callback: NativeAuthorizationCallback,
    ) -> bool {
        let generation = admission.generation;
        admission.callback.store(Some(Arc::new(GenerationCallback {
            generation,
            callback,
        })));
        if !self.is_current_admission(admission) {
            return false;
        }
        if admission.ready.load(Ordering::Acquire) {
            self.callback_notify.notify_one();
        }
        true
    }

    /// Preserve the current generation only for an unauthenticated browser handoff.
    #[must_use]
    #[cfg(any(target_os = "android", test))]
    pub fn suspend_for_browser_handoff(&self) -> bool {
        let running_guard = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(running) = running_guard.as_ref() else {
            return false;
        };
        if !self.is_current(running.generation) || running.credentials.has_bearer_token() {
            return false;
        }
        let generation = running.generation;
        drop(running_guard);
        self.handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .suspend(generation)
    }

    /// Resume a generation retained for a browser handoff.
    #[must_use]
    #[cfg(any(target_os = "android", test))]
    pub fn resume_browser_handoff(&self) -> bool {
        let generation = self.active_generation.load(Ordering::Acquire);
        if generation == 0 {
            return false;
        }
        let admission = self
            .handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resume(generation);
        let Some(admission) = admission else {
            return false;
        };
        if !self.is_current_admission(&admission) {
            admission.clear();
            return false;
        }
        self.callback_notify.notify_one();
        true
    }

    #[cfg(any(target_os = "android", test))]
    fn is_current_admission(&self, admission: &Arc<CallbackAdmission>) -> bool {
        self.is_current(admission.generation)
            && self
                .callback_admission
                .load_full()
                .is_some_and(|current| Arc::ptr_eq(&current, admission))
    }
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

impl Drop for RuntimeBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one generation thread receives its isolated owned channels and state"
)]
fn run_thread(
    requests: mpsc::Receiver<HostRequest>,
    callback_admission: Arc<ArcSwapOption<CallbackAdmission>>,
    callback_notify: Arc<Notify>,
    stop: watch::Receiver<bool>,
    publisher: Publisher<BridgePayload>,
    credentials: NativeCredentialProvider,
    open_url: OpenUrl,
    handoff: Arc<Mutex<BrowserHandoff>>,
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
            callback_admission,
            callback_notify,
            stop,
            publisher,
            credentials,
            open_url,
            handoff,
        )
        .instrument(runtime_span.clone()),
    );
    observability::record_outcome(&runtime_span, "stopped");
}

#[allow(
    clippy::future_not_send,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the ClientStateStream is intentionally hosted by one LocalSet loop"
)]
async fn run_generation(
    mut requests: mpsc::Receiver<HostRequest>,
    callback_admission: Arc<ArcSwapOption<CallbackAdmission>>,
    callback_notify: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
    mut publisher: Publisher<BridgePayload>,
    credentials: NativeCredentialProvider,
    open_url: OpenUrl,
    handoff: Arc<Mutex<BrowserHandoff>>,
) {
    let transport = NativeGrpcTransport::with_credential_provider(ENDPOINT, credentials.clone());
    let runtime = ClientRuntime::with_transport(transport);
    let mut stream = runtime.into_state_stream(|delay_ms| {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
    });
    let external_callback = cfg!(target_os = "android");
    let mut access = Some(NativeAccessCoordinator::spawn(
        native_access_config(external_callback),
        credentials.clone(),
        CHANNEL_CAPACITY,
    ));
    let mut logout_reply = None;
    let mut deferred_teardown = false;

    if !publisher
        .publish_initial(BridgePayload::State {
            snapshot: Box::new(stream.snapshot()),
        })
        .await
    {
        stop_access(access).await;
        let _ = credentials.clear_bearer_token();
        return;
    }

    loop {
        enum Ready {
            Request(Option<HostRequest>),
            Snapshot(Box<ClientSnapshot>),
            Access(Option<NativeAccessOutput>),
            Callback,
        }

        let ready = tokio::select! {
            _ = stop.changed() => Ready::Request(Some(HostRequest::Stop)),
            request = requests.recv() => Ready::Request(request),
            snapshot = stream.next() => Ready::Snapshot(Box::new(snapshot)),
            output = next_access_output(&mut access) => Ready::Access(output),
            () = callback_notify.notified() => Ready::Callback,
        };

        match ready {
            Ready::Request(Some(HostRequest::Stop) | None) => break,
            Ready::Request(Some(HostRequest::Dispatch { command, reply })) => {
                let operation =
                    observability::operation_span("client.command.execute", "native_task");
                let result = dispatch_and_publish(
                    &mut stream,
                    &mut publisher,
                    *command,
                    None,
                    Publication::Latest,
                )
                .instrument(operation.clone())
                .await;
                observability::record_result(&operation, &result);
                drop(reply.send(result));
            }
            Ready::Request(Some(HostRequest::Access { action, reply })) => {
                let operation =
                    observability::operation_span("access.action.execute", "native_task");
                if matches!(
                    action,
                    NativeAccessAction::Cancel | NativeAccessAction::Logout
                ) {
                    deferred_teardown |=
                        finish_handoff(&handoff, &callback_admission, publisher.generation);
                }
                if matches!(action, NativeAccessAction::Logout) {
                    if logout_reply.is_some() {
                        observability::record_outcome(&operation, "rejected");
                        drop(reply.send(Err("native logout is already pending".to_owned())));
                        continue;
                    }
                    if !credentials.has_bearer_token() {
                        let result = publish_state_control(&stream, &mut publisher).await;
                        observability::record_result(&operation, &result);
                        drop(reply.send(result.map(Some)));
                        continue;
                    }
                    logout_reply = Some(reply);
                    if let Some(coordinator) = &access {
                        if coordinator.action_sender().send(action).await.is_err()
                            && let Some(reply) = logout_reply.take()
                        {
                            drop(reply.send(Err("native access coordinator stopped".to_owned())));
                            observability::record_outcome(&operation, "stopped");
                        }
                    } else if let Some(reply) = logout_reply.take() {
                        drop(reply.send(Err("native access is unavailable".to_owned())));
                        observability::record_outcome(&operation, "unavailable");
                    }
                } else if let Some(coordinator) = &access {
                    let result = coordinator
                        .action_sender()
                        .send(action)
                        .await
                        .map(|()| None)
                        .map_err(|_| "native access coordinator stopped".to_owned());
                    observability::record_result(&operation, &result);
                    drop(reply.send(result));
                } else {
                    drop(reply.send(Err("native access is unavailable".to_owned())));
                    observability::record_outcome(&operation, "unavailable");
                }
            }
            Ready::Snapshot(snapshot) => {
                publisher.publish_state(BridgePayload::State { snapshot });
            }
            Ready::Callback => {
                let callback = take_ready_callback(&callback_admission, publisher.generation);
                if publisher.is_current()
                    && let (Some(callback), Some(coordinator)) = (callback, &access)
                {
                    drop(
                        coordinator
                            .action_sender()
                            .send(NativeAccessAction::AuthorizationCallback(callback))
                            .await,
                    );
                }
            }
            Ready::Access(Some(NativeAccessOutput::Progress(progress))) => {
                let progress_class = access_progress_class(&progress);
                let progress_span = observability::operation_span("access.progress", "native_task");
                observability::record_outcome(&progress_span, access_progress_outcome(&progress));
                progress_span.in_scope(|| {
                    tracing::info!(
                        event.name = "access.progress",
                        access.stage = progress_class,
                        outcome = access_progress_outcome(&progress),
                    );
                });
                if let NativeAccessProgress::AwaitingBrowser {
                    authorization_url, ..
                } = &progress
                {
                    let opened = if let Ok(url) = external_https_url(authorization_url) {
                        open_authorization_url(
                            &handoff,
                            &callback_admission,
                            publisher.generation,
                            external_callback,
                            &open_url,
                            url.as_str(),
                        )
                    } else {
                        false
                    };
                    if !opened {
                        deferred_teardown |=
                            finish_handoff(&handoff, &callback_admission, publisher.generation);
                        if let Some(coordinator) = &access {
                            drop(
                                coordinator
                                    .action_sender()
                                    .send(NativeAccessAction::Cancel)
                                    .await,
                            );
                        }
                    }
                }
                if matches!(
                    progress,
                    NativeAccessProgress::Succeeded
                        | NativeAccessProgress::Failed { .. }
                        | NativeAccessProgress::Canceled
                ) {
                    deferred_teardown |=
                        finish_handoff(&handoff, &callback_admission, publisher.generation);
                }
                publisher.publish(BridgePayload::Access {
                    progress: AccessProgress::Native {
                        progress: progress.into(),
                    },
                });
            }
            Ready::Access(Some(NativeAccessOutput::ClientCommand {
                command,
                logout_barrier,
            })) => {
                let operation =
                    observability::operation_span("access.client_command", "native_task");
                if logout_barrier
                    && publisher
                        .publish_control(
                            BridgePayload::Access {
                                progress: AccessProgress::LoggingOut,
                            },
                            false,
                        )
                        .await
                        .is_none()
                {
                    continue;
                }
                let action_sender = access.as_ref().map(NativeAccessCoordinator::action_sender);
                let result = dispatch_and_publish(
                    &mut stream,
                    &mut publisher,
                    *command,
                    action_sender,
                    if logout_barrier {
                        Publication::Control
                    } else {
                        Publication::Latest
                    },
                )
                .instrument(operation.clone())
                .await;
                observability::record_result(&operation, &result);
                if logout_barrier && let Some(reply) = logout_reply.take() {
                    drop(reply.send(result.clone().map(Some)));
                }
            }
            Ready::Access(None) => {
                deferred_teardown |=
                    finish_handoff(&handoff, &callback_admission, publisher.generation);
                access = None;
            }
        }
        if deferred_teardown {
            let _ = publisher.active_generation.compare_exchange(
                publisher.generation,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            break;
        }
    }

    clear_handoff(&handoff, &callback_admission, publisher.generation);
    stop_access(access).await;
    let _ = credentials.clear_bearer_token();
}

const fn access_progress_class(progress: &NativeAccessProgress) -> &'static str {
    match progress {
        NativeAccessProgress::Discovering => "discovering",
        NativeAccessProgress::AwaitingBrowser { .. } => "awaiting_browser",
        NativeAccessProgress::Exchanging => "exchanging",
        NativeAccessProgress::Refreshing => "refreshing",
        NativeAccessProgress::RefreshSucceeded => "refresh_succeeded",
        NativeAccessProgress::Succeeded => "succeeded",
        NativeAccessProgress::Failed { .. } => "failed",
        NativeAccessProgress::Canceled => "canceled",
    }
}

const fn access_progress_outcome(progress: &NativeAccessProgress) -> &'static str {
    match progress {
        NativeAccessProgress::Succeeded | NativeAccessProgress::RefreshSucceeded => "ok",
        NativeAccessProgress::Failed { .. } => "error",
        NativeAccessProgress::Canceled => "canceled",
        _ => "in_progress",
    }
}

fn native_access_config(external_callback: bool) -> NativeAccessConfig {
    NativeAccessConfig {
        endpoint: ENDPOINT.to_owned(),
        client_id: NATIVE_CLIENT_ID.to_owned(),
        scopes: Vec::new(),
        audience: Some(AUDIENCE.to_owned()),
        callback: if external_callback {
            NativeCallbackPolicy::External
        } else {
            NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral)
        },
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the authorization effect receives explicit handoff state and opener inputs"
)]
fn open_authorization_url(
    handoff: &Mutex<BrowserHandoff>,
    callback_admission: &ArcSwapOption<CallbackAdmission>,
    generation: u64,
    external_callback: bool,
    open_url: &OpenUrl,
    url: &str,
) -> bool {
    if external_callback {
        let admission = Arc::new(CallbackAdmission::new(generation));
        handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin(generation, Arc::clone(&admission));
        if let Some(previous) = callback_admission.swap(Some(admission)) {
            previous.clear();
        }
    }
    open_url(url)
}

fn take_ready_callback(
    callback_admission: &ArcSwapOption<CallbackAdmission>,
    generation: u64,
) -> Option<NativeAuthorizationCallback> {
    let admission = callback_admission.load_full()?;
    if admission.generation != generation || !admission.ready.load(Ordering::Acquire) {
        return None;
    }
    let callback = admission.callback.swap(None)?;
    if callback.generation != generation {
        return None;
    }
    let current = callback_admission.load_full()?;
    Arc::ptr_eq(&current, &admission).then_some(callback.callback.clone())
}

fn finish_handoff(
    handoff: &Mutex<BrowserHandoff>,
    callback_admission: &ArcSwapOption<CallbackAdmission>,
    generation: u64,
) -> bool {
    let finished = handoff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .finish(generation);
    let Some((suspended, admission)) = finished else {
        return false;
    };
    invalidate_admission(callback_admission, &admission);
    suspended
}

fn clear_handoff(
    handoff: &Mutex<BrowserHandoff>,
    callback_admission: &ArcSwapOption<CallbackAdmission>,
    generation: u64,
) {
    let admission = handoff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear(generation);
    if let Some(admission) = admission {
        invalidate_admission(callback_admission, &admission);
    }
}

fn invalidate_admission(
    callback_admission: &ArcSwapOption<CallbackAdmission>,
    admission: &Arc<CallbackAdmission>,
) {
    drop(callback_admission.compare_and_swap(admission, None));
    admission.clear();
}

async fn next_access_output(
    access: &mut Option<NativeAccessCoordinator>,
) -> Option<NativeAccessOutput> {
    match access {
        Some(coordinator) => coordinator.next_output().await,
        None => pending().await,
    }
}

async fn stop_access(access: Option<NativeAccessCoordinator>) {
    if let Some(coordinator) = access {
        coordinator.stop().await;
    }
}

#[allow(
    clippy::future_not_send,
    reason = "the ClientStateStream is intentionally polled only on its LocalSet"
)]
async fn dispatch_and_publish(
    stream: &mut ClientStateStream,
    publisher: &mut Publisher<BridgePayload>,
    command: ClientCommand,
    access_actions: Option<mpsc::Sender<NativeAccessAction>>,
    publication: Publication,
) -> Result<BridgeEvent<BridgePayload>, String> {
    let starts_login = matches!(command, ClientCommand::StartWaltrLogin);
    let (reply, result) = futures_channel::oneshot::channel();
    let event = stream
        .next_or_dispatch(async move { Some(ClientCommandRequest::with_result(command, reply)) })
        .await;
    let ClientStateStreamEvent::Snapshot(_snapshot) = event else {
        return Err("native client command queue closed".to_owned());
    };
    let snapshot = result
        .await
        .map_err(|_| "native client dispatch result was canceled".to_owned())?
        .map_err(|error| error.to_string())?;
    if starts_login
        && let (Some(attempt_id), Some(actions)) =
            (snapshot.access.active_attempt_id.clone(), access_actions)
    {
        drop(
            actions
                .send(NativeAccessAction::AttemptStarted { attempt_id })
                .await,
        );
    }

    match publication {
        Publication::Latest => publish_snapshot(snapshot, publisher),
        Publication::Control => publish_snapshot_control(snapshot, publisher).await,
    }
}

#[derive(Clone, Copy)]
enum Publication {
    Latest,
    Control,
}

#[allow(
    clippy::future_not_send,
    reason = "the ClientStateStream is intentionally read only on its LocalSet"
)]
async fn publish_state_control(
    stream: &ClientStateStream,
    publisher: &mut Publisher<BridgePayload>,
) -> Result<BridgeEvent<BridgePayload>, String> {
    publish_snapshot_control(stream.snapshot(), publisher).await
}

async fn publish_snapshot_control(
    snapshot: ClientSnapshot,
    publisher: &mut Publisher<BridgePayload>,
) -> Result<BridgeEvent<BridgePayload>, String> {
    publisher
        .publish_control(
            BridgePayload::State {
                snapshot: Box::new(snapshot),
            },
            true,
        )
        .await
        .ok_or_else(|| "native client generation was replaced".to_owned())
}

fn publish_snapshot(
    snapshot: ClientSnapshot,
    publisher: &mut Publisher<BridgePayload>,
) -> Result<BridgeEvent<BridgePayload>, String> {
    publisher
        .publish_state(BridgePayload::State {
            snapshot: Box::new(snapshot),
        })
        .ok_or_else(|| "native client generation was replaced".to_owned())
}

const fn is_native_access_command(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::StartWaltrLogin
            | ClientCommand::CompleteWaltrLogin { .. }
            | ClientCommand::FailWaltrLogin { .. }
            | ClientCommand::CancelWaltrLogin { .. }
            | ClientCommand::ReprobeWaltrAccess
            | ClientCommand::LogoutWaltr
            | ClientCommand::WaltrAccessExpired { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_observability_classifications_are_bounded() {
        let cases = [
            (
                NativeAccessProgress::Discovering,
                "discovering",
                "in_progress",
            ),
            (
                NativeAccessProgress::AwaitingBrowser {
                    authorization_url: "SECRET_CALLBACK_URL_MARKER".to_owned(),
                    timeout_millis: 1,
                },
                "awaiting_browser",
                "in_progress",
            ),
            (
                NativeAccessProgress::Exchanging,
                "exchanging",
                "in_progress",
            ),
            (
                NativeAccessProgress::Refreshing,
                "refreshing",
                "in_progress",
            ),
            (
                NativeAccessProgress::RefreshSucceeded,
                "refresh_succeeded",
                "ok",
            ),
            (NativeAccessProgress::Succeeded, "succeeded", "ok"),
            (
                NativeAccessProgress::Failed {
                    error: "SECRET_ERROR_MARKER".to_owned(),
                },
                "failed",
                "error",
            ),
            (NativeAccessProgress::Canceled, "canceled", "canceled"),
        ];
        for (progress, stage, outcome) in cases {
            assert_eq!(access_progress_class(&progress), stage);
            assert_eq!(access_progress_outcome(&progress), outcome);
            assert!(!stage.contains("SECRET_"));
            assert!(!outcome.contains("SECRET_"));
        }
    }

    fn callback_test_bridge(generation: u64) -> (RuntimeBridge, NativeCredentialProvider) {
        let bridge = RuntimeBridge::new(|_| true);
        bridge
            .active_generation
            .store(generation, Ordering::Release);
        let (requests, _request_receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let (stop, _stop_receiver) = watch::channel(false);
        let credentials = NativeCredentialProvider::new();
        let recovery = Arc::new(RwLock::new(RecoveryState::new(generation)));
        let thread = thread::spawn(|| {});
        *bridge.running.lock().unwrap() = Some(RunningGeneration {
            generation,
            requests,
            stop,
            credentials: credentials.clone(),
            recovery,
            thread,
        });
        (bridge, credentials)
    }

    fn install_test_handoff(bridge: &RuntimeBridge, generation: u64) -> Arc<CallbackAdmission> {
        let admission = Arc::new(CallbackAdmission::new(generation));
        bridge
            .handoff
            .lock()
            .unwrap()
            .begin(generation, Arc::clone(&admission));
        if let Some(previous) = bridge.callback_admission.swap(Some(Arc::clone(&admission))) {
            previous.clear();
        }
        admission
    }

    #[tokio::test]
    async fn initial_is_first_and_later_updates_coalesce_to_latest() {
        let active = Arc::new(AtomicU64::new(7));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(7)));
        let (mut publisher, mut feed) = Publisher::new(active, 7, recovery);
        assert!(publisher.publish_initial("initial").await);
        assert!(publisher.publish("one"));
        assert!(publisher.publish("two"));
        assert!(publisher.publish("latest"));

        let initial = feed.initial().await.unwrap();
        assert_eq!(
            (initial.generation, initial.sequence, initial.payload),
            (7, 0, "initial")
        );
        let update = feed.next_update().await.unwrap();
        assert_eq!(
            (update.generation, update.sequence, update.payload),
            (7, 3, "latest")
        );
    }

    #[tokio::test]
    async fn sequences_are_monotonic_and_stale_generation_is_rejected() {
        let active = Arc::new(AtomicU64::new(3));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(3)));
        let (mut publisher, mut feed) = Publisher::new(Arc::clone(&active), 3, recovery);
        publisher.publish_initial(10_u8).await;
        let _ = feed.initial().await;
        publisher.publish(11);
        let first = feed.next_update().await.unwrap();
        publisher.publish(12);
        let second = feed.next_update().await.unwrap();
        assert_eq!((first.generation, first.sequence), (3, 1));
        assert_eq!((second.generation, second.sequence), (3, 2));

        active.store(4, Ordering::Release);
        assert!(!publisher.publish(13));
    }

    #[tokio::test]
    async fn dispose_invalidates_pending_publication() {
        let active = Arc::new(AtomicU64::new(9));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(9)));
        let (mut publisher, mut feed) = Publisher::new(Arc::clone(&active), 9, recovery);
        publisher.publish_initial("initial").await;
        let _ = feed.initial().await;

        active.store(0, Ordering::Release);
        assert!(!publisher.publish("after-dispose"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), feed.next_update())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bounded_fifo_serializes_dispatch_requests() {
        let (sender, mut receiver) = mpsc::channel(2);
        let producer = tokio::spawn(async move {
            for command in ["first", "second", "third"] {
                sender.send(command).await.unwrap();
            }
        });
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(receiver.recv().await.unwrap());
        }
        producer.await.unwrap();
        assert_eq!(seen, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn unread_coalesced_state_precedes_logout_barrier_and_cleared_state() {
        let active = Arc::new(AtomicU64::new(1));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(1)));
        let (mut publisher, mut feed) = Publisher::new(active, 1, recovery);
        publisher.publish_initial("initial").await;
        let _ = feed.initial().await;
        publisher.publish_state("ordinary-one").unwrap();
        publisher.publish_state("ordinary-latest").unwrap();
        publisher
            .publish_control("logging-out", false)
            .await
            .unwrap();
        publisher
            .publish_control("logged-out-state", true)
            .await
            .unwrap();
        let ordinary = feed.next_update().await.unwrap();
        let barrier = feed.next_update().await.unwrap();
        let state = feed.next_update().await.unwrap();
        assert_eq!(
            (ordinary.sequence, ordinary.payload),
            (2, "ordinary-latest")
        );
        assert_eq!(barrier.payload, "logging-out");
        assert_eq!(state.payload, "logged-out-state");
        assert_eq!((barrier.sequence, state.sequence), (3, 4));
    }

    #[tokio::test]
    async fn recovery_replays_controls_without_minting_past_them() {
        let active = Arc::new(AtomicU64::new(2));
        let recovery = Arc::new(RwLock::new(RecoveryState::new(2)));
        let (mut publisher, _feed) = Publisher::new(active, 2, Arc::clone(&recovery));
        publisher.publish_initial("initial").await;
        publisher.publish_state("ordinary-latest").unwrap();
        publisher
            .publish_control("logging-out", false)
            .await
            .unwrap();
        publisher
            .publish_control("logged-out-state", true)
            .await
            .unwrap();
        let sequence = publisher.sequence;

        let recovered = recovery_after(&recovery.read().unwrap(), 2, Some(0));
        assert_eq!(recovered.generation, 2);
        assert_eq!(
            recovered
                .events
                .iter()
                .map(|event| (event.sequence, event.payload))
                .collect::<Vec<_>>(),
            [(2, "logging-out"), (3, "logged-out-state")]
        );
        assert_eq!(publisher.sequence, sequence);

        let replaced = recovery_after(&recovery.read().unwrap(), 3, Some(0));
        assert_eq!(replaced.generation, 3);
        assert!(replaced.events.is_empty());
    }

    #[test]
    fn serialized_access_output_contains_no_secret_fields() {
        let progress = NativeAccessProgress::AwaitingBrowser {
            authorization_url: concat!(
                "https://id.example/authorize?state=STATE_MARKER&nonce=NONCE_MARKER",
                "&code_verifier=VERIFIER_MARKER&access_token=ACCESS_MARKER",
                "&refresh_token=REFRESH_MARKER"
            )
            .to_owned(),
            timeout_millis: 1_000,
        };
        let event = BridgeEvent {
            generation: 1,
            sequence: 2,
            payload: BridgePayload::Access {
                progress: AccessProgress::Native {
                    progress: progress.into(),
                },
            },
        };
        let serialized = serde_json::to_string(&event).unwrap().to_ascii_lowercase();
        for forbidden in [
            "state_marker",
            "nonce_marker",
            "verifier_marker",
            "access_marker",
            "refresh_marker",
            "access_token",
            "accesstoken",
            "refresh_token",
            "refreshtoken",
            "code_verifier",
            "codeverifier",
            "client_secret",
        ] {
            assert!(!serialized.contains(forbidden), "{forbidden}: {serialized}");
        }
    }

    #[test]
    fn callback_policy_is_external_only_for_android() {
        assert_eq!(
            native_access_config(true).callback,
            NativeCallbackPolicy::External
        );
        assert_eq!(
            native_access_config(false).callback,
            NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral)
        );
    }

    #[tokio::test]
    async fn callback_admission_is_lock_free_latest_and_has_no_lost_wakeup() {
        let invalid =
            "dev.waltr.app:/oauth2redirect?code=INVALID_CODE_MARKER&state=WRONG_STATE_MARKER";
        let valid =
            "dev.waltr.app:/oauth2redirect?code=VALID_CODE_MARKER&state=VALID_STATE_MARKER";
        let (bridge, _credentials) = callback_test_bridge(7);
        assert!(
            !bridge.authorization_callback(NativeAuthorizationCallback::new(invalid.to_owned()))
        );

        install_test_handoff(&bridge, 7);
        let running = bridge.running.lock().unwrap();
        assert!(
            bridge.authorization_callback(NativeAuthorizationCallback::new(invalid.to_owned()))
        );
        drop(running);
        let handoff = bridge.handoff.lock().unwrap();
        assert!(bridge.authorization_callback(NativeAuthorizationCallback::new(valid.to_owned())));
        drop(handoff);

        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            bridge.callback_notify.notified(),
        )
        .await
        .unwrap();
        let callback = take_ready_callback(&bridge.callback_admission, 7).unwrap();
        assert_eq!(callback, NativeAuthorizationCallback::new(valid.to_owned()));
        let debug = format!("{callback:?}").to_ascii_lowercase();
        assert_eq!(debug, "nativeauthorizationcallback([redacted])");
        assert!(!debug.contains("valid_code_marker"));
        assert!(!debug.contains("valid_state_marker"));

        bridge.active_generation.store(8, Ordering::Release);
        assert!(!bridge.authorization_callback(NativeAuthorizationCallback::new(valid.to_owned())));
        bridge.stop();
        assert!(!bridge.authorization_callback(NativeAuthorizationCallback::new(valid.to_owned())));
    }

    #[test]
    fn retired_generation_cannot_displace_replacement_or_survive_stop() {
        let valid =
            "dev.waltr.app:/oauth2redirect?code=VALID_CODE_MARKER&state=VALID_STATE_MARKER";
        let stale =
            "dev.waltr.app:/oauth2redirect?code=STALE_CODE_MARKER&state=STALE_STATE_MARKER";
        let (bridge, _credentials) = callback_test_bridge(7);
        let old = install_test_handoff(&bridge, 7);

        bridge.stop();
        assert!(old.callback.load_full().is_none());
        bridge.active_generation.store(8, Ordering::Release);
        let current = install_test_handoff(&bridge, 8);
        assert!(bridge.authorization_callback(NativeAuthorizationCallback::new(valid.to_owned())));
        assert!(
            !bridge.store_callback_to_admission(
                &old,
                NativeAuthorizationCallback::new(stale.to_owned())
            )
        );
        assert_eq!(
            take_ready_callback(&bridge.callback_admission, 8),
            Some(NativeAuthorizationCallback::new(valid.to_owned()))
        );
        assert!(take_ready_callback(&bridge.callback_admission, 7).is_none());
        assert!(Arc::ptr_eq(
            &bridge.callback_admission.load_full().unwrap(),
            &current
        ));
        old.clear();
        assert!(bridge.authorization_callback(NativeAuthorizationCallback::new(valid.to_owned())));
        assert!(current.callback.load_full().is_some());
        bridge.stop();
        assert!(current.callback.load_full().is_none());
        assert!(bridge.callback_admission.load_full().is_none());
    }

    #[test]
    fn browser_handoff_precedes_open_and_fences_suspension() {
        let handoff = Arc::new(Mutex::new(BrowserHandoff::default()));
        let callback_admission = ArcSwapOption::empty();
        let opener_handoff = Arc::clone(&handoff);
        let opener: OpenUrl = Arc::new(move |_| {
            opener_handoff
                .lock()
                .unwrap()
                .active
                .as_ref()
                .is_some_and(|active| active.generation == 4)
        });
        assert!(open_authorization_url(
            &handoff,
            &callback_admission,
            4,
            true,
            &opener,
            "https://id.example/authorize"
        ));
        assert!(handoff.lock().unwrap().suspend(4));
        let admission = callback_admission.load_full().unwrap();
        admission.callback.store(Some(Arc::new(GenerationCallback {
            generation: 4,
            callback: NativeAuthorizationCallback::new(
                "dev.waltr.app:/oauth2redirect?code=held&state=held".to_owned(),
            ),
        })));
        assert!(take_ready_callback(&callback_admission, 4).is_none());
        assert!(handoff.lock().unwrap().resume(4).is_some());
        assert!(take_ready_callback(&callback_admission, 4).is_some());
        assert!(!handoff.lock().unwrap().suspend(4));
        assert!(
            handoff
                .lock()
                .unwrap()
                .finish(4)
                .is_some_and(|value| !value.0)
        );

        let admission = Arc::new(CallbackAdmission::new(5));
        handoff.lock().unwrap().begin(5, admission);
        assert!(handoff.lock().unwrap().suspend(5));
        assert!(
            handoff
                .lock()
                .unwrap()
                .finish(5)
                .is_some_and(|value| value.0)
        );
        assert!(handoff.lock().unwrap().resume(5).is_none());
        assert!(!handoff.lock().unwrap().suspend(5));
    }

    #[tokio::test]
    async fn suspended_callback_releases_on_resume_without_lost_notification() {
        let (bridge, _credentials) = callback_test_bridge(8);
        let admission = install_test_handoff(&bridge, 8);
        assert!(bridge.suspend_for_browser_handoff());
        assert!(
            bridge.authorization_callback(NativeAuthorizationCallback::new(
                "dev.waltr.app:/oauth2redirect?code=held&state=resume".to_owned()
            ))
        );
        assert!(take_ready_callback(&bridge.callback_admission, 8).is_none());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                bridge.callback_notify.notified(),
            )
            .await
            .is_err()
        );
        assert!(bridge.resume_browser_handoff());
        tokio::time::timeout(
            std::time::Duration::from_millis(10),
            bridge.callback_notify.notified(),
        )
        .await
        .unwrap();
        assert!(take_ready_callback(&bridge.callback_admission, 8).is_some());
        assert!(admission.callback.load_full().is_none());
        assert!(!bridge.suspend_for_browser_handoff());
    }

    #[test]
    fn mobile_suspend_preserves_only_pending_unauthenticated_handoff() {
        let (bridge, credentials) = callback_test_bridge(11);
        assert!(!bridge.suspend_for_browser_handoff());

        install_test_handoff(&bridge, 11);
        assert!(bridge.suspend_for_browser_handoff());
        assert!(bridge.is_current(11));
        assert!(bridge.resume_browser_handoff());
        assert!(bridge.is_current(11));

        finish_handoff(&bridge.handoff, &bridge.callback_admission, 11);
        credentials.replace_bearer_token("TOKEN_MARKER").unwrap();
        install_test_handoff(&bridge, 11);
        assert!(!bridge.suspend_for_browser_handoff());
        bridge.stop();
        assert!(!credentials.has_bearer_token());
        assert!(!bridge.is_current(11));
    }

    #[test]
    fn lifecycle_stop_clears_credentials_and_resume_uses_a_fresh_generation() {
        let bridge = RuntimeBridge::new(|_| true);
        let first = bridge.start().unwrap();
        let first_generation = first.generation();
        let credentials = bridge
            .running
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .credentials
            .clone();
        credentials.replace_bearer_token("secret-marker").unwrap();
        assert!(credentials.has_bearer_token());

        bridge.stop();
        assert!(!credentials.has_bearer_token());
        assert!(!bridge.is_current(first_generation));

        let resumed = bridge.start().unwrap();
        assert!(resumed.generation() > first_generation);
        bridge.stop();
    }
}
