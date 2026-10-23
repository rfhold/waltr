//! Host-owned microphone authorization only; never opens a capture device.
//!
//! A grant is not capture authority. Callers must revalidate their generation and
//! operation after awaiting it and immediately before starting capture.
use std::{
    fmt,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::oneshot;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[path = "audio_permissions_macos.rs"]
mod macos;

/// Sanitized authorization outcome. Restricted and unknown OS states fail closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionResult {
    /// Audio access was authorized; caller intent still needs revalidation.
    Granted,
    /// Access was denied or restricted.
    Denied,
    /// The intent, generation, or foreground epoch is no longer current.
    Cancelled,
    /// Another OS request still owns the single dialog slot.
    Busy,
    /// This platform or its permission bridge is unavailable.
    Unavailable,
}

/// Injectable nonblocking platform boundary. Implementations must not capture audio.
pub trait PermissionBackend: Send + Sync {
    /// Dispatch one audio-only request; complete the owned reply on any queue.
    fn request(&self, ticket: u64, reply: PermissionReply);
}

/// Owned, thread-safe completion token with no runtime, thread, or capture handles.
#[derive(Clone)]
pub struct PermissionReply {
    state: Weak<Mutex<Inner>>,
    ticket: u64,
}
impl fmt::Debug for PermissionReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PermissionReply(<redacted>)")
    }
}
impl PermissionReply {
    /// Whether this exact request still has a live foreground waiter.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.state.upgrade().is_some_and(|state| {
            let inner = lock(&state);
            !inner.disposed
                && inner.foreground
                && inner.dialog == Some(self.ticket)
                && inner.pending.as_ref().is_some_and(|pending| {
                    pending.generation == inner.generation && pending.epoch == inner.epoch
                })
        })
    }
    /// Consume only the exact dialog identity; duplicate and late replies are inert.
    pub fn complete(&self, result: PermissionResult) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut inner = lock(&state);
        if inner.dialog != Some(self.ticket) {
            return;
        }
        inner.dialog = None;
        if let Some(pending) = inner.pending.take() {
            let current = !inner.disposed
                && inner.foreground
                && pending.generation == inner.generation
                && pending.epoch == inner.epoch;
            let _sent = pending.sender.send(if current {
                result
            } else {
                PermissionResult::Cancelled
            });
        }
    }
}
struct Pending {
    operation: u64,
    generation: u64,
    epoch: u64,
    sender: oneshot::Sender<PermissionResult>,
}
struct Inner {
    generation: u64,
    epoch: u64,
    next_ticket: u64,
    foreground: bool,
    disposed: bool,
    dialog: Option<u64>,
    pending: Option<Pending>,
}
fn lock(state: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn cancel(inner: &mut Inner) {
    if let Some(pending) = inner.pending.take() {
        let _sent = pending.sender.send(PermissionResult::Cancelled);
    }
}
struct CancelOnDrop(PermissionReply);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(state) = self.0.state.upgrade() {
            let mut inner = lock(&state);
            if inner.dialog == Some(self.0.ticket) {
                cancel(&mut inner);
            }
        }
    }
}

/// One host-owned permission gate. Dropping it resolves outstanding waiters.
pub struct AudioPermissions {
    state: Arc<Mutex<Inner>>,
    backend: Arc<dyn PermissionBackend>,
}
impl fmt::Debug for AudioPermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AudioPermissions(<redacted>)")
    }
}
impl AudioPermissions {
    /// Construct an injected gate without touching platform permission APIs.
    pub fn new(generation: u64, foreground: bool, backend: Arc<dyn PermissionBackend>) -> Self {
        Self {
            state: Arc::new(Mutex::new(Inner {
                generation,
                foreground,
                epoch: 0,
                next_ticket: 0,
                disposed: false,
                dialog: None,
                pending: None,
            })),
            backend,
        }
    }

    /// Request for an expected generation and caller-owned operation identity.
    ///
    /// The operation is opaque and never logged. Dropping this future
    /// cancels its waiter, but preserves the OS dialog slot until its callback.
    pub async fn request(&self, expected_generation: u64, operation: u64) -> PermissionResult {
        let (reply, receiver) = {
            let mut inner = lock(&self.state);
            if inner.disposed || !inner.foreground || inner.generation != expected_generation {
                return PermissionResult::Cancelled;
            }
            if inner.dialog.is_some() {
                return PermissionResult::Busy;
            }
            let Some(ticket) = inner
                .next_ticket
                .checked_add(1)
                .filter(|ticket| i64::try_from(*ticket).is_ok())
            else {
                return PermissionResult::Unavailable;
            };
            inner.next_ticket = ticket;
            let (sender, receiver) = oneshot::channel();
            inner.dialog = Some(ticket);
            inner.pending = Some(Pending {
                operation,
                generation: inner.generation,
                epoch: inner.epoch,
                sender,
            });
            drop(inner);
            (
                PermissionReply {
                    state: Arc::downgrade(&self.state),
                    ticket,
                },
                receiver,
            )
        };
        let _cancellation = CancelOnDrop(reply.clone());
        self.backend.request(reply.ticket, reply);
        receiver.await.unwrap_or(PermissionResult::Cancelled)
    }

    /// Cancel only the exact caller operation, preserving any outstanding OS UI.
    pub fn cancel_request(&self, generation: u64, operation: u64) {
        let mut inner = lock(&self.state);
        if inner.pending.as_ref().is_some_and(|pending| {
            pending.generation == generation && pending.operation == operation
        }) {
            cancel(&mut inner);
        }
    }

    /// Cancel current intent and replace its generation, retaining any OS dialog.
    pub fn invalidate(&self, generation: u64) {
        let mut inner = lock(&self.state);
        cancel(&mut inner);
        inner.generation = generation;
    }
    /// Fail closed immediately on backgrounding, including permission-dialog pause.
    pub fn pause(&self) {
        let mut inner = lock(&self.state);
        inner.foreground = false;
        cancel(&mut inner);
        if let Some(epoch) = inner.epoch.checked_add(1) {
            inner.epoch = epoch;
        } else {
            inner.disposed = true;
        }
    }
    /// Restore foreground eligibility, never restart an old request or capture.
    pub fn resume(&self) {
        lock(&self.state).foreground = true;
    }
    /// Permanently close this gate without waiting for OS UI or joining threads.
    pub fn dispose(&self) {
        let mut inner = lock(&self.state);
        inner.disposed = true;
        cancel(&mut inner);
        drop(inner);
    }
}
impl Drop for AudioPermissions {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// Build the process host adapter without requesting access or opening a device.
#[must_use]
pub fn host(app: &tauri::AppHandle, generation: u64) -> AudioPermissions {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        AudioPermissions::new(
            generation,
            true,
            Arc::new(macos::MacPermission(app.clone())),
        )
    }
    #[cfg(target_os = "android")]
    {
        let _ = app;
        let mut host = ANDROID_HOST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = AudioPermissions::new(generation, host.foreground, Arc::new(AndroidPermission));
        // Only one process host can own the Activity's unticketed OS dialog.
        if host.installed {
            gate.dispose();
        } else {
            host.installed = true;
            host.state = Arc::downgrade(&gate.state);
        }
        gate
    }
    #[cfg(not(any(
        target_os = "android",
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    {
        let _ = app;
        AudioPermissions::new(generation, true, Arc::new(OtherPermission))
    }
}
#[cfg(not(any(
    target_os = "android",
    all(target_os = "macos", target_arch = "aarch64")
)))]
struct OtherPermission;
#[cfg(not(any(
    target_os = "android",
    all(target_os = "macos", target_arch = "aarch64")
)))]
impl PermissionBackend for OtherPermission {
    fn request(&self, _: u64, reply: PermissionReply) {
        // Linux has no application-level OS microphone authorization prompt.
        reply.complete(if cfg!(target_os = "linux") {
            PermissionResult::Granted
        } else {
            PermissionResult::Unavailable
        });
    }
}
#[cfg(target_os = "android")]
static ANDROID_HOST: Mutex<AndroidHost> = Mutex::new(AndroidHost {
    installed: false,
    foreground: false,
    state: Weak::new(),
});
#[cfg(target_os = "android")]
struct AndroidHost {
    installed: bool,
    foreground: bool,
    state: Weak<Mutex<Inner>>,
}
#[cfg(target_os = "android")]
fn android_state() -> Weak<Mutex<Inner>> {
    ANDROID_HOST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .state
        .clone()
}
#[cfg(target_os = "android")]
struct AndroidPermission;
#[cfg(target_os = "android")]
impl PermissionBackend for AndroidPermission {
    fn request(&self, ticket: u64, reply: PermissionReply) {
        tauri::wry::prelude::dispatch(move |env, activity, _| {
            if !reply.is_current() {
                reply.complete(PermissionResult::Cancelled);
                return;
            }
            if env
                .call_method(
                    activity,
                    "requestRecordAudioPermission",
                    "(J)V",
                    &[(ticket as i64).into()],
                )
                .is_err()
            {
                reply.complete(PermissionResult::Unavailable);
            }
        });
    }
}
#[cfg(target_os = "android")]
pub(crate) fn android_result(ticket: i64, granted: bool) {
    if let Ok(ticket) = u64::try_from(ticket) {
        PermissionReply {
            state: android_state(),
            ticket,
        }
        .complete(if granted {
            PermissionResult::Granted
        } else {
            PermissionResult::Denied
        });
    }
}
#[cfg(target_os = "android")]
pub(crate) fn android_current(ticket: i64) -> bool {
    u64::try_from(ticket).ok().is_some_and(|ticket| {
        PermissionReply {
            state: android_state(),
            ticket,
        }
        .is_current()
    })
}
#[cfg(target_os = "android")]
pub(crate) fn android_foreground(foreground: bool) {
    let mut host = ANDROID_HOST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    host.foreground = foreground;
    if let Some(state) = host.state.upgrade() {
        let mut inner = lock(&state);
        inner.foreground = foreground;
        if !foreground {
            cancel(&mut inner);
            if let Some(epoch) = inner.epoch.checked_add(1) {
                inner.epoch = epoch;
            } else {
                inner.disposed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    #[derive(Default)]
    struct Fake(Mutex<Vec<PermissionReply>>);
    impl PermissionBackend for Fake {
        fn request(&self, _: u64, reply: PermissionReply) {
            self.0.lock().unwrap().push(reply);
        }
    }
    impl Fake {
        fn reply(&self, index: usize) -> PermissionReply {
            self.0.lock().unwrap()[index].clone()
        }
        fn calls(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }
    fn pending(future: Pin<&mut impl Future<Output = PermissionResult>>) {
        assert!(matches!(
            future.poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
    }
    fn fixture() -> (AudioPermissions, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        (AudioPermissions::new(7, true, fake.clone()), fake)
    }
    #[tokio::test]
    async fn injected_grant_denial_and_unavailable_never_capture() {
        let (gate, fake) = fixture();
        for (index, result) in [
            PermissionResult::Granted,
            PermissionResult::Denied,
            PermissionResult::Unavailable,
        ]
        .into_iter()
        .enumerate()
        {
            let mut request = Box::pin(gate.request(7, 99));
            pending(request.as_mut());
            fake.reply(index).complete(result);
            assert_eq!(request.await, result);
        }
        assert_eq!(fake.calls(), 3);
    }
    #[tokio::test]
    async fn exact_operation_cancellation_and_arbitrary_queue_completion() {
        let (gate, fake) = fixture();
        let mut request = Box::pin(gate.request(7, 42));
        pending(request.as_mut());
        gate.cancel_request(7, 41);
        gate.cancel_request(6, 42);
        assert!(fake.reply(0).is_current());
        gate.cancel_request(7, 42);
        assert_eq!(request.await, PermissionResult::Cancelled);
        let reply = fake.reply(0);
        std::thread::spawn(move || reply.complete(PermissionResult::Granted))
            .join()
            .unwrap();
        let mut request = Box::pin(gate.request(7, 43));
        pending(request.as_mut());
        gate.cancel_request(7, 42);
        let reply = fake.reply(1);
        std::thread::spawn(move || reply.complete(PermissionResult::Granted))
            .join()
            .unwrap();
        assert_eq!(request.await, PermissionResult::Granted);
    }

    #[tokio::test]
    async fn generation_replacement_retains_cancelled_dialog_identity() {
        let (gate, fake) = fixture();
        let mut old = Box::pin(gate.request(7, 1));
        pending(old.as_mut());
        let old_reply = fake.reply(0);
        gate.invalidate(8);
        assert_eq!(old.await, PermissionResult::Cancelled);
        assert_eq!(gate.request(7, 2).await, PermissionResult::Cancelled);
        assert_eq!(gate.request(8, 2).await, PermissionResult::Busy);
        assert_eq!(fake.calls(), 1);
        assert!(!old_reply.is_current());
        old_reply.complete(PermissionResult::Granted);
        let mut new = Box::pin(gate.request(8, 2));
        pending(new.as_mut());
        old_reply.complete(PermissionResult::Granted);
        pending(new.as_mut());
        fake.reply(1).complete(PermissionResult::Denied);
        assert_eq!(new.await, PermissionResult::Denied);
    }
    #[tokio::test]
    async fn pause_resume_never_resurrects_grant() {
        let (gate, fake) = fixture();
        let mut request = Box::pin(gate.request(7, 1));
        pending(request.as_mut());
        gate.pause();
        assert_eq!(request.await, PermissionResult::Cancelled);
        assert_eq!(gate.request(7, 2).await, PermissionResult::Cancelled);
        gate.resume();
        assert_eq!(gate.request(7, 2).await, PermissionResult::Busy);
        fake.reply(0).complete(PermissionResult::Granted);
        assert_eq!(fake.calls(), 1);
        assert_eq!(lock(&gate.state).epoch, 1);
        let mut explicit = Box::pin(gate.request(7, 2));
        pending(explicit.as_mut());
        fake.reply(1).complete(PermissionResult::Granted);
        assert_eq!(explicit.await, PermissionResult::Granted);
    }
    #[tokio::test]
    async fn dropping_future_cancels_waiter_without_reusing_dialog() {
        let (gate, fake) = fixture();
        let mut request = Box::pin(gate.request(7, 1));
        pending(request.as_mut());
        drop(request);
        assert!(!fake.reply(0).is_current());
        assert_eq!(gate.request(7, 2).await, PermissionResult::Busy);
        assert_eq!(fake.calls(), 1);
        fake.reply(0).complete(PermissionResult::Granted);
        assert!(lock(&gate.state).dialog.is_none());
    }
    #[tokio::test]
    async fn dispose_overlap_and_counter_exhaustion_fail_closed() {
        let (gate, fake) = fixture();
        let mut request = Box::pin(gate.request(7, 1));
        pending(request.as_mut());
        for operation in 2..100 {
            assert_eq!(gate.request(7, operation).await, PermissionResult::Busy);
        }
        assert_eq!(fake.calls(), 1);
        gate.dispose();
        assert_eq!(request.await, PermissionResult::Cancelled);
        gate.resume();
        fake.reply(0).complete(PermissionResult::Granted);
        assert_eq!(gate.request(7, 100).await, PermissionResult::Cancelled);
        let (gate, fake) = fixture();
        lock(&gate.state).next_ticket = i64::MAX as u64;
        assert_eq!(gate.request(7, 1).await, PermissionResult::Unavailable);
        assert_eq!(fake.calls(), 0);
    }
    #[test]
    fn drop_owner_and_duplicate_callbacks_are_safe() {
        let (gate, fake) = fixture();
        let mut request = Box::pin(gate.request(7, 1));
        pending(request.as_mut());
        drop(request);
        drop(gate);
        fake.reply(0).complete(PermissionResult::Granted);
        fake.reply(0).complete(PermissionResult::Granted);
        assert!(!fake.reply(0).is_current());
        assert_eq!(
            format!("{:?}", fake.reply(0)),
            "PermissionReply(<redacted>)"
        );
    }
    #[test]
    fn packaging_is_audio_only_and_lifecycle_precedes_remote_preservation() {
        let info = include_str!("../Info.plist");
        let entitlements = include_str!("../Entitlements.plist");
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert!(info.contains("NSMicrophoneUsageDescription"));
        assert_eq!(info.matches("<key>").count(), 1);
        assert!(entitlements.contains("com.apple.security.device.audio-input"));
        assert_eq!(entitlements.matches("<key>").count(), 1);
        assert_eq!(config["bundle"]["macOS"]["infoPlist"], "Info.plist");
        assert_eq!(
            config["bundle"]["macOS"]["entitlements"],
            "Entitlements.plist"
        );
        assert_eq!(config["bundle"]["macOS"]["minimumSystemVersion"], "11.0");
        let manifest = include_str!("../gen/android/app/src/main/AndroidManifest.xml");
        assert_eq!(
            manifest.matches("android.permission.RECORD_AUDIO").count(),
            1
        );
        for forbidden in [
            "FOREGROUND_SERVICE_MICROPHONE",
            "android.permission.CAMERA",
            "foregroundServiceType=\"microphone",
        ] {
            assert!(!manifest.contains(forbidden));
        }
        let activity =
            include_str!("../gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt");
        assert!(
            activity.contains("nativeRecordAudioPermissionResult(ticket: Long, granted: Boolean)")
        );
        assert!(activity.contains("nativeRecordAudioPermissionCurrent(ticket: Long): Boolean"));
        assert!(
            activity.contains("recordAudioPermission.launch(Manifest.permission.RECORD_AUDIO)")
        );
        assert!(activity.contains("if (audioTicket != null)"));
        assert!(activity.contains("savedInstanceState.getLong(\"waltr.audioPermissionTicket\")"));
        assert!(activity.contains(
            "savedInstanceState.getString(\"waltr.audioPermissionProcess\") == audioProcessIdentity"
        ));
        assert!(activity.contains("} else -1L"));
        let rust = include_str!("lib.rs");
        let suspend = rust.split("pub fn nativeClientSuspend").nth(1).unwrap();
        assert!(
            suspend
                .find("audio_permissions::android_foreground(false)")
                .unwrap()
                < suspend.find("if lifecycle.bridge.is_remote()").unwrap()
        );
    }
}
