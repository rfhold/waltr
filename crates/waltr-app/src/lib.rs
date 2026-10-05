//! Tauri shell and IPC surface for the Waltr native application.
#![cfg_attr(
    not(all(target_os = "macos", target_arch = "aarch64")),
    forbid(unsafe_code)
)]

mod app_container;
mod bridge;
mod observability;
mod policy;
mod server_controller;

#[cfg(all(
    feature = "native-apps-test",
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
compile_error!("native-apps-test is a disposable Apple Silicon macOS diagnostic build");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native_apps;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native_apps_navigation;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(target_os = "android")]
use std::sync::{Mutex, OnceLock};

use bridge::{BridgeEvent, BridgePayload, BridgeRecovery, EventFeed, RuntimeBridge};
use waltr_client::ClientCommand;
use serde::Serialize;
#[cfg(not(target_os = "android"))]
use server_controller::AppPrivateRoots;
use server_controller::EmbeddedServerController;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::webview::NewWindowResponse;
use tauri::webview::PageLoadEvent;
use tauri::{Emitter, Manager, State, WebviewUrl};
use tauri_plugin_opener::OpenerExt as _;
use tracing::Instrument as _;

const BRIDGE_EVENT: &str = "waltr://bridge";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStarted {
    generation: u64,
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    native_apps: native_apps::Bootstrap,
    #[cfg(all(
        feature = "native-apps-test",
        target_os = "macos",
        target_arch = "aarch64"
    ))]
    native_apps_test: native_apps::Bootstrap,
}

#[cfg(any(target_os = "android", test))]
#[derive(Debug, Default)]
struct MobileStartCoordination {
    requested: bool,
    suspended: bool,
    next_ticket: u64,
    active_start: Option<u64>,
    active_generation: Option<u64>,
}

#[cfg(any(target_os = "android", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MobileStartDisposition {
    Deliver,
    Suspended,
    Cancelled,
}

#[cfg(any(target_os = "android", test))]
impl MobileStartCoordination {
    fn begin_request(&mut self) -> (u64, Option<u64>) {
        self.requested = true;
        let displaced = self.active_generation.take();
        (self.begin_start(), displaced)
    }

    fn begin_start(&mut self) -> u64 {
        self.next_ticket = self.next_ticket.saturating_add(1);
        self.active_start = Some(self.next_ticket);
        self.next_ticket
    }

    fn suspend(&mut self) -> (Option<u64>, Option<u64>) {
        if self.suspended {
            return (None, None);
        }
        self.suspended = true;
        let start = self.active_start.take();
        (start, self.active_generation.take())
    }

    fn resume(&mut self) -> Option<u64> {
        let was_suspended = self.suspended;
        self.suspended = false;
        (was_suspended && self.requested && self.active_start.is_none()).then(|| self.begin_start())
    }

    fn finish(&mut self, ticket: u64, generation: u64) -> MobileStartDisposition {
        if self.active_start != Some(ticket) {
            return MobileStartDisposition::Cancelled;
        }
        self.active_start = None;
        if !self.requested {
            MobileStartDisposition::Cancelled
        } else if self.suspended {
            MobileStartDisposition::Suspended
        } else {
            self.active_generation = Some(generation);
            MobileStartDisposition::Deliver
        }
    }

    fn fail(&mut self, ticket: u64) {
        if self.active_start == Some(ticket) {
            self.active_start = None;
            self.requested = false;
        }
    }

    fn cancel(&mut self) -> (Option<u64>, Option<u64>) {
        self.requested = false;
        (self.active_start.take(), self.active_generation.take())
    }
}

#[tauri::command]
async fn start_runtime(
    app: tauri::AppHandle,
    bridge: State<'_, Arc<RuntimeBridge>>,
) -> Result<RuntimeStarted, String> {
    let span = observability::operation_span("runtime.start", "ipc");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let native_apps = {
        let bootstrap = app
            .state::<EmbeddedServerController>()
            .connection()
            .and_then(|connection| connection.native_apps)
            .map(|bootstrap| *bootstrap)
            .ok_or_else(|| "native Apps listener is unavailable".to_owned())?;
        if let Err(error) = native_apps_navigation::install(&app, &bootstrap).await {
            app.state::<EmbeddedServerController>().stop();
            app.exit(1);
            return Err(error);
        }
        bootstrap
    };
    let result = (|| {
        #[cfg(target_os = "android")]
        let (ticket, displaced_generation) = begin_mobile_client_request()?;
        #[cfg(target_os = "android")]
        bridge.authorize_start(ticket);
        #[cfg(target_os = "android")]
        if let Some(generation) = displaced_generation {
            bridge.stop_generation_nonblocking(generation);
        }
        #[cfg(target_os = "android")]
        let feed = match bridge.start_nonblocking(ticket) {
            Ok(feed) => feed,
            Err(error) => {
                fail_mobile_client_start(ticket);
                return Err(error);
            }
        };
        #[cfg(not(target_os = "android"))]
        let feed = bridge.start()?;
        let generation = feed.generation();
        #[cfg(target_os = "android")]
        match finish_mobile_client_start(ticket, generation) {
            MobileStartDisposition::Deliver => {
                deliver_events(app, Arc::clone(bridge.inner()), feed);
            }
            MobileStartDisposition::Suspended => {
                bridge.stop_generation_nonblocking(generation);
            }
            MobileStartDisposition::Cancelled => {
                bridge.stop_generation_nonblocking(generation);
                return Err("native client start was cancelled".to_owned());
            }
        }
        #[cfg(not(target_os = "android"))]
        deliver_events(app.clone(), Arc::clone(bridge.inner()), feed);
        #[cfg(all(
            feature = "native-apps-test",
            target_os = "macos",
            target_arch = "aarch64"
        ))]
        eprintln!("WALTR_NATIVE_PROBE positive-control");
        Ok(RuntimeStarted {
            generation,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            native_apps: native_apps.clone(),
            #[cfg(all(
                feature = "native-apps-test",
                target_os = "macos",
                target_arch = "aarch64"
            ))]
            native_apps_test: native_apps,
        })
    })();
    observability::record_result(&span, &result);
    result
}

#[tauri::command]
async fn client_dispatch(
    command: ClientCommand,
    bridge: State<'_, Arc<RuntimeBridge>>,
) -> Result<BridgeEvent<BridgePayload>, String> {
    let span = observability::operation_span("client.dispatch", "ipc");
    let result = bridge.dispatch(command).instrument(span.clone()).await;
    observability::record_result(&span, &result);
    result
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command injection owns State"
)]
fn client_state(
    after_sequence: Option<u64>,
    bridge: State<'_, Arc<RuntimeBridge>>,
) -> Result<BridgeRecovery<BridgePayload>, String> {
    let span = observability::operation_span("client.state", "ipc");
    let result = bridge.recover(after_sequence);
    observability::record_result(&span, &result);
    result
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command injection owns State"
)]
fn dispose_runtime(bridge: State<'_, Arc<RuntimeBridge>>) {
    let span = observability::operation_span("runtime.dispose", "ipc");
    #[cfg(target_os = "android")]
    let (ticket, generation) = clear_mobile_client_requested();
    #[cfg(target_os = "android")]
    if let Some(ticket) = ticket {
        bridge.invalidate_start(ticket);
    }
    #[cfg(target_os = "android")]
    if let Some(generation) = generation {
        bridge.stop_generation_nonblocking(generation);
    }
    #[cfg(target_os = "android")]
    bridge.clear_authorization_state();
    #[cfg(not(target_os = "android"))]
    stop_client_bridge(bridge.inner());
    observability::record_outcome(&span, "ok");
}

#[cfg(target_os = "android")]
fn stop_client_bridge(bridge: &RuntimeBridge) {
    bridge.stop_nonblocking();
}

#[cfg(not(target_os = "android"))]
fn stop_client_bridge(bridge: &RuntimeBridge) {
    bridge.stop();
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command injection owns AppHandle and deserialized arguments"
)]
fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let span = observability::operation_span("external.open", "ipc");
    let result = policy::external_https_url(&url)
        .map_err(str::to_owned)
        .and_then(|url| {
            app.opener()
                .open_url(url.as_str(), None::<&str>)
                .map_err(|_| "external URL could not be opened".to_owned())
        });
    observability::record_result(&span, &result);
    result
}

#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri command injection owns AppHandle and deserialized arguments"
)]
fn open_authorization_attempt(
    app: tauri::AppHandle,
    attempt_id: String,
    bridge: State<'_, Arc<RuntimeBridge>>,
) -> Result<(), String> {
    let span = observability::operation_span("authorization.open", "ipc");
    let result = bridge
        .authorization_attempt_url(&attempt_id)
        .and_then(|url| {
            policy::external_https_url(&url)
                .map_err(str::to_owned)
                .map(|validated| validated.to_string())
        })
        .and_then(|url| {
            app.opener()
                .open_url(url, None::<&str>)
                .map_err(|_| "authorization attempt could not be opened".to_owned())
        });
    observability::record_result(&span, &result);
    result
}

fn deliver_events(
    app: tauri::AppHandle,
    bridge: Arc<RuntimeBridge>,
    mut feed: EventFeed<BridgePayload>,
) {
    let span = observability::operation_span("bridge.deliver", "native_task");
    let task_span = span.clone();
    tauri::async_runtime::spawn(
        async move {
            let Some(initial) = feed.initial().await else {
                observability::record_outcome(&task_span, "stopped");
                return;
            };
            if !bridge.is_current(initial.generation) {
                observability::record_outcome(&task_span, "stale");
                return;
            }
            if app.emit(BRIDGE_EVENT, &initial).is_err() {
                observability::error_event("bridge.emit", "native_task", "error");
            }

            while let Some(update) = feed.next_update().await {
                if !bridge.is_current(update.generation) {
                    observability::record_outcome(&task_span, "stale");
                    return;
                }
                if app.emit(BRIDGE_EVENT, &update).is_err() {
                    observability::error_event("bridge.emit", "native_task", "error");
                }
            }
            observability::record_outcome(&task_span, "stopped");
        }
        .instrument(span),
    );
}

#[cfg(target_os = "android")]
fn start_and_deliver(
    app: tauri::AppHandle,
    bridge: Arc<RuntimeBridge>,
    ticket: u64,
) -> Result<u64, String> {
    bridge.authorize_start(ticket);
    let feed = match bridge.start_nonblocking(ticket) {
        Ok(feed) => feed,
        Err(error) => {
            fail_mobile_client_start(ticket);
            return Err(error);
        }
    };
    let generation = feed.generation();
    match finish_mobile_client_start(ticket, generation) {
        MobileStartDisposition::Deliver => {
            deliver_events(app, bridge, feed);
            Ok(generation)
        }
        MobileStartDisposition::Suspended => {
            bridge.stop_generation_nonblocking(generation);
            Ok(generation)
        }
        MobileStartDisposition::Cancelled => {
            bridge.stop_generation_nonblocking(generation);
            Err("native client start was cancelled".to_owned())
        }
    }
}

#[cfg(target_os = "android")]
struct MobileLifecycle {
    app: tauri::AppHandle,
    bridge: Arc<RuntimeBridge>,
    coordination: MobileStartCoordination,
}

#[cfg(target_os = "android")]
static MOBILE_LIFECYCLE: OnceLock<Mutex<MobileLifecycle>> = OnceLock::new();
#[cfg(target_os = "android")]
static ANDROID_SERVER: OnceLock<EmbeddedServerController> = OnceLock::new();

#[cfg(target_os = "android")]
fn android_server() -> &'static EmbeddedServerController {
    ANDROID_SERVER.get_or_init(EmbeddedServerController::default)
}

#[cfg(target_os = "android")]
fn install_mobile_lifecycle(app: tauri::AppHandle, bridge: Arc<RuntimeBridge>) {
    drop(MOBILE_LIFECYCLE.set(Mutex::new(MobileLifecycle {
        app,
        bridge,
        coordination: MobileStartCoordination::default(),
    })));
}

#[cfg(target_os = "android")]
fn begin_mobile_client_request() -> Result<(u64, Option<u64>), String> {
    MOBILE_LIFECYCLE
        .get()
        .ok_or_else(|| "native mobile lifecycle is unavailable".to_owned())
        .map(|lifecycle| {
            lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .coordination
                .begin_request()
        })
}

#[cfg(target_os = "android")]
fn finish_mobile_client_start(ticket: u64, generation: u64) -> MobileStartDisposition {
    MOBILE_LIFECYCLE
        .get()
        .map_or(MobileStartDisposition::Cancelled, |lifecycle| {
            lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .coordination
                .finish(ticket, generation)
        })
}

#[cfg(target_os = "android")]
fn fail_mobile_client_start(ticket: u64) {
    if let Some(lifecycle) = MOBILE_LIFECYCLE.get() {
        lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .coordination
            .fail(ticket);
    }
}

#[cfg(target_os = "android")]
fn clear_mobile_client_requested() -> (Option<u64>, Option<u64>) {
    MOBILE_LIFECYCLE.get().map_or((None, None), |lifecycle| {
        lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .coordination
            .cancel()
    })
}

#[cfg(target_os = "android")]
fn java_path(
    env: &mut jni::Env<'_>,
    value: jni::objects::JString<'_>,
) -> Option<std::path::PathBuf> {
    value
        .mutf8_chars(env)
        .ok()
        .map(|value| std::path::PathBuf::from(value.to_str().into_owned()))
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.WaltrForegroundService")]
/// Start or join the process-owned mobile embedded server generation.
pub fn nativeStartServer(
    mut env: jni::EnvUnowned<'_>,
    service: jni::objects::JObject<'_>,
    data_root: jni::objects::JString<'_>,
    config_root: jni::objects::JString<'_>,
) -> jni::sys::jint {
    let span = observability::operation_span("android.server.start", "lifecycle");
    let result = env
        .with_env(|env| -> jni::errors::Result<_> {
            rustls_platform_verifier::android::init_with_env(env, service)?;
            Ok(java_path(env, data_root)
                .zip(java_path(env, config_root))
                .ok_or(server_controller::ServerStartError::Paths)
                .and_then(|(data, config)| server_controller::android_roots(&data, &config))
                .and_then(|roots| android_server().start(roots)))
        })
        .into_outcome();
    let succeeded = matches!(result, jni::Outcome::Ok(Ok(_)));
    match &result {
        jni::Outcome::Ok(Err(error)) => observability::record_android_startup_failure(
            &span,
            observability::AndroidStartupFailure::Server(*error),
        ),
        jni::Outcome::Err(_) => observability::record_android_startup_failure(
            &span,
            observability::AndroidStartupFailure::Jni,
        ),
        jni::Outcome::Panic(_) => observability::record_android_startup_panic(&span),
        jni::Outcome::Ok(Ok(_)) => observability::record_outcome(&span, "ok"),
    }
    i32::from(succeeded)
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.WaltrForegroundService")]
/// Stop the process-owned mobile embedded server and clear its local credential.
pub fn nativeStopServer(_env: jni::EnvUnowned<'_>, _service: jni::objects::JObject<'_>) {
    if let Some(lifecycle) = MOBILE_LIFECYCLE.get() {
        lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bridge
            .clear_authorization_state();
    }
    android_server().stop_nonblocking();
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.WaltrForegroundService")]
/// Return only the fixed embedded server lifecycle phase code.
pub fn nativeServerStatus(
    _env: jni::EnvUnowned<'_>,
    _service: jni::objects::JObject<'_>,
) -> jni::sys::jint {
    use server_controller::ServerPhase;
    match android_server().phase() {
        ServerPhase::Stopped => 0,
        ServerPhase::Starting => 1,
        ServerPhase::Running => 2,
        ServerPhase::Stopping => 3,
        ServerPhase::Failed => 4,
    }
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.MainActivity")]
/// Record the notification gate without starting server work.
pub fn nativeNotificationPermissionRequired(
    _env: jni::EnvUnowned<'_>,
    _activity: jni::objects::JObject<'_>,
    required: jni::sys::jboolean,
) {
    if let Some(lifecycle) = MOBILE_LIFECYCLE.get() {
        lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bridge
            .notification_permission_required(required);
    }
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.MainActivity")]
/// Stop only the activity-owned client generation during backgrounding.
pub fn nativeClientSuspend(_env: jni::EnvUnowned<'_>, _activity: jni::objects::JObject<'_>) {
    let Some(lifecycle) = MOBILE_LIFECYCLE.get() else {
        return;
    };
    let stop = {
        let mut lifecycle = lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            Arc::clone(&lifecycle.bridge),
            lifecycle.coordination.suspend(),
        )
    };
    let (bridge, (ticket, generation)) = stop;
    if let Some(ticket) = ticket {
        bridge.invalidate_start(ticket);
    }
    if let Some(generation) = generation {
        bridge.suspend_generation_nonblocking(generation);
    }
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.MainActivity")]
/// Start a fresh client generation only after an already-active activity resumes.
pub fn nativeClientResume(_env: jni::EnvUnowned<'_>, _activity: jni::objects::JObject<'_>) {
    let Some(lifecycle) = MOBILE_LIFECYCLE.get() else {
        return;
    };
    let start = {
        let mut lifecycle = lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle
            .coordination
            .resume()
            .map(|ticket| (lifecycle.app.clone(), Arc::clone(&lifecycle.bridge), ticket))
    };
    if let Some((app, bridge, ticket)) = start {
        drop(start_and_deliver(app, bridge, ticket));
    }
}

/// Run the desktop or Android Tauri application.
///
/// # Panics
///
/// Panics when Tauri cannot initialize or its application event loop fails.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _observability = observability::init();
    let startup = observability::operation_span("app.run", "lifecycle");
    let startup_entered = startup.enter();
    tracing::info!(event.name = "app.startup", outcome = "started");
    let mut context = tauri::generate_context!();
    app_container::configure_apps_csp(context.config_mut());
    let builder = tauri::Builder::default();
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    let builder = builder.invoke_system(app_container::confined_invoke_script());
    let app = builder
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let setup = observability::operation_span("app.setup", "lifecycle");
            let _setup_entered = setup.enter();

            #[cfg(all(
                feature = "native-apps-test",
                target_os = "macos",
                target_arch = "aarch64"
            ))]
            native_apps::validate_profile(app.config())?;

            #[cfg(target_os = "android")]
            let server = android_server().clone();
            #[cfg(not(target_os = "android"))]
            let server = {
                let data = app.path().app_data_dir().map_err(std::io::Error::other)?;
                let config = app.path().app_config_dir().map_err(std::io::Error::other)?;
                let roots = AppPrivateRoots::new(data, config).map_err(std::io::Error::other)?;
                let server = EmbeddedServerController::default();
                server.start(roots).map_err(std::io::Error::other)?;
                server
            };
            let bridge = Arc::new(RuntimeBridge::new(server.clone()));
            app.manage(Arc::clone(&bridge));
            app.manage(server);

            #[cfg(target_os = "android")]
            install_mobile_lifecycle(app.handle().clone(), Arc::clone(&bridge));

            let first_page_load = Arc::new(AtomicBool::new(true));
            let page_load_bridge = Arc::clone(&bridge);
            let page = if cfg!(all(
                feature = "native-apps-test",
                target_os = "macos",
                target_arch = "aarch64"
            )) {
                "native-apps-test.html"
            } else {
                "index.html"
            };
            let window =
                tauri::WebviewWindowBuilder::new(app, "main", WebviewUrl::App(page.into()))
                    .on_navigation(policy::allows_packaged_navigation)
                    .on_page_load(move |_, payload| {
                        if payload.event() == PageLoadEvent::Started
                            && !first_page_load.swap(false, Ordering::AcqRel)
                        {
                            observability::lifecycle_event(
                                "webview.page_load",
                                "lifecycle",
                                "client_stopped",
                            );
                            stop_client_bridge(&page_load_bridge);
                        }
                    });
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let window = window
                .title("Waltr")
                .inner_size(1180.0, 780.0)
                .min_inner_size(720.0, 540.0)
                .on_new_window(|_, _| {
                    #[cfg(feature = "native-apps-test")]
                    eprintln!("WALTR_NATIVE_PROBE navigation-denied.popup");
                    NewWindowResponse::Deny
                });
            let result = window.build();
            observability::record_result(&setup, &result);
            result?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_runtime,
            client_dispatch,
            client_state,
            dispose_runtime,
            open_external_url,
            open_authorization_attempt,
        ])
        .build(context)
        .expect("failed to build Waltr native application");
    observability::record_outcome(&startup, "ok");
    drop(startup_entered);
    drop(startup);
    app.run(|app, event| match event {
        tauri::RunEvent::Exit => {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            native_apps_navigation::detach("main");
            observability::lifecycle_event("app.shutdown", "lifecycle", "exit");
            stop_client_bridge(app.state::<Arc<RuntimeBridge>>().inner());
            #[cfg(not(target_os = "android"))]
            app.state::<EmbeddedServerController>().stop();
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Destroyed,
            ..
        } if label == "main" => {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            native_apps_navigation::detach("main");
            observability::lifecycle_event("webview.destroyed", "lifecycle", "client_stopped");
            stop_client_bridge(app.state::<Arc<RuntimeBridge>>().inner());
        }
        _ => {}
    });
}

#[cfg(test)]
mod configuration_tests {
    use super::{MobileStartCoordination, MobileStartDisposition};

    #[test]
    fn mobile_start_suspend_resume_invalidates_original_ticket() {
        let mut coordination = MobileStartCoordination::default();
        let (original, displaced) = coordination.begin_request();
        assert!(displaced.is_none());
        assert_eq!(coordination.suspend(), (Some(original), None));
        let replacement = coordination.resume().expect("replacement start ticket");
        assert_eq!(
            coordination.finish(original, 11),
            MobileStartDisposition::Cancelled
        );
        assert_eq!(
            coordination.finish(replacement, 12),
            MobileStartDisposition::Deliver
        );
        assert_eq!(coordination.active_generation, Some(12));
    }

    #[test]
    fn overlapping_start_tickets_cannot_claim_newer_generation() {
        let mut coordination = MobileStartCoordination::default();
        let (old, _) = coordination.begin_request();
        let (new, _) = coordination.begin_request();
        assert_eq!(
            coordination.finish(new, 22),
            MobileStartDisposition::Deliver
        );
        assert_eq!(
            coordination.finish(old, 21),
            MobileStartDisposition::Cancelled
        );
        assert_eq!(coordination.active_generation, Some(22));
    }

    #[test]
    fn mobile_start_rollback_fences_cancelled_ownership() {
        let mut coordination = MobileStartCoordination::default();
        let (cancelled_ticket, _) = coordination.begin_request();
        assert_eq!(coordination.cancel(), (Some(cancelled_ticket), None));
        assert_eq!(
            coordination.finish(cancelled_ticket, 31),
            MobileStartDisposition::Cancelled
        );

        let (failed_ticket, _) = coordination.begin_request();
        coordination.fail(failed_ticket);
        assert!(!coordination.requested);
        assert!(coordination.active_start.is_none());
    }

    #[test]
    fn android_embedded_service_contract_is_private_and_permission_gated() {
        let manifest = include_str!("../gen/android/app/src/main/AndroidManifest.xml");
        let activity =
            include_str!("../gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt");
        let service = include_str!(
            "../gen/android/app/src/main/java/dev/waltr/app/WaltrForegroundService.kt"
        );
        let strings = include_str!("../gen/android/app/src/main/res/values/strings.xml");
        let stop_icon = include_str!("../gen/android/app/src/main/res/drawable/ic_stop_server.xml");
        let gradle = include_str!("../gen/android/app/build.gradle.kts");
        let proguard = include_str!("../gen/android/app/proguard-rules.pro");
        let rust = include_str!("lib.rs");
        for permission in [
            "android.permission.FOREGROUND_SERVICE",
            "android.permission.FOREGROUND_SERVICE_SPECIAL_USE",
            "android.permission.POST_NOTIFICATIONS",
        ] {
            assert!(manifest.contains(permission));
        }
        assert!(manifest.contains("android:name=\".WaltrForegroundService\""));
        assert!(manifest.contains("android:exported=\"false\""));
        assert!(manifest.contains("android:stopWithTask=\"false\""));
        assert!(manifest.contains("android:foregroundServiceType=\"specialUse\""));
        assert!(manifest.contains("PROPERTY_SPECIAL_USE_FGS_SUBTYPE"));
        assert!(!manifest.contains("android.intent.action.VIEW"));
        assert!(activity.contains("Manifest.permission.POST_NOTIFICATIONS"));
        assert!(activity.contains("nativeNotificationPermissionRequired"));
        assert!(service.contains("startForeground("));
        assert!(service.contains("START_STICKY"));
        assert!(
            service
                .contains("private const val STOP_ACTION = \"dev.waltr.app.action.STOP_SERVER\"")
        );
        assert!(
            service
                .contains("Intent(this, WaltrForegroundService::class.java).setAction(STOP_ACTION)")
        );
        assert!(service.contains("PendingIntent.getService("));
        assert!(service.contains("private const val STOP_REQUEST_CODE = 14334"));
        assert!(
            service.contains("PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT")
        );
        assert!(service.contains(
            ".addAction(\n        R.drawable.ic_stop_server,\n        getString(R.string.server_notification_stop),\n        stopPendingIntent\n      )"
        ));
        assert!(service.contains(
            "if (intent?.action == STOP_ACTION) {\n      nativeStopServer()\n      stopForeground(STOP_FOREGROUND_REMOVE)\n      stopSelf()\n      return START_NOT_STICKY\n    }"
        ));
        let stop_dispatch = service
            .find("if (intent?.action == STOP_ACTION)")
            .expect("stop action dispatch");
        let ordinary_start = service[stop_dispatch..]
            .find("promote()")
            .map(|offset| stop_dispatch + offset)
            .expect("ordinary server-start path");
        assert!(stop_dispatch < ordinary_start);
        assert!(strings.contains("<string name=\"server_notification_stop\">Stop server</string>"));
        assert!(stop_icon.contains("android:width=\"24dp\""));
        assert!(stop_icon.contains("android:height=\"24dp\""));
        assert!(stop_icon.contains("android:fillColor=\"#FFFFFFFF\""));
        assert!(stop_icon.contains("android:pathData=\"M6,6h12v12H6z\""));
        assert!(service.contains("System.loadLibrary(\"waltr_app_lib\")"));
        assert!(service.contains(
            "try {\n          nativeStartServer(filesDir.absolutePath, File(filesDir, \"config\").absolutePath)\n        } finally {\n          workerStarted.set(false)\n        }"
        ));
        let foreground = service.find("promote()").expect("foreground promotion");
        let native_load = service
            .find("loadNativeLibrary()")
            .expect("native library load");
        assert!(foreground < native_load);
        assert!(!service.contains("companion object {\n    init"));
        assert!(
            activity.contains("ensureNotificationPermissionAndServer()\n    nativeClientResume()")
        );
        assert!(proguard.contains("WaltrForegroundService"));
        assert!(proguard.contains("org.rustls.platformverifier.**"));
        assert!(gradle.contains("cargo\",\n            \"metadata"));
        assert!(gradle.contains("rustls-platform-verifier-android"));
        assert!(gradle.contains("rustls:rustls-platform-verifier:"));
        let verifier_init = rust
            .find("rustls_platform_verifier::android::init_with_env")
            .expect("Android platform verifier initialization");
        let server_start = rust
            .find("android_server().start(roots)")
            .expect("Android embedded server start");
        assert!(verifier_init < server_start);
        assert!(rust.contains("record_android_startup_failure("));
        assert!(rust.contains("record_android_startup_panic(&span)"));
        for forbidden in ["endpoint", "bearer", "token", "127.0.0.1"] {
            assert!(
                !format!("{activity}\n{service}")
                    .to_ascii_lowercase()
                    .contains(forbidden)
            );
        }
    }

    #[test]
    fn source_contract_removes_preview_access_and_keyring() {
        let cargo = include_str!("../Cargo.toml");
        let bridge = include_str!("bridge.rs");
        let source = include_str!("lib.rs");
        let forbidden = [
            ["preview", "-waltr.holdenitdown.net"].concat(),
            ["NativeAccess", "Coordinator"].concat(),
            ["NativeRefresh", "TokenStore"].concat(),
            ["access", "_start"].concat(),
            ["access", "_cancel"].concat(),
            ["access", "_logout"].concat(),
            ["keyring", "-core"].concat(),
            ["android-native", "-keyring-store"].concat(),
            ["zbus-secret-service", "-keyring-store"].concat(),
        ];
        for forbidden in forbidden {
            assert!(!format!("{cargo}\n{bridge}\n{source}").contains(&forbidden));
        }
        assert!(cargo.contains("waltr-server"));
        assert!(bridge.contains("NativeGrpcTransport::with_credential_provider"));
        assert!(source.contains("fn open_authorization_attempt("));
        assert!(source.contains("attempt_id: String"));
        assert!(source.contains(".authorization_attempt_url(&attempt_id)"));
    }

    #[test]
    fn backup_policy_excludes_all_private_embedded_state() {
        let legacy = include_str!("../gen/android/app/src/main/res/xml/backup_rules.xml");
        let modern = include_str!("../gen/android/app/src/main/res/xml/data_extraction_rules.xml");
        for domain in ["file", "database", "sharedpref", "root"] {
            assert!(legacy.contains(&format!("domain=\"{domain}\" path=\".\"")));
            assert_eq!(
                modern
                    .matches(&format!("domain=\"{domain}\" path=\".\""))
                    .count(),
                2
            );
        }
    }

    #[test]
    fn csp_and_capabilities_remain_local_and_minimal() {
        let config = include_str!("../tauri.conf.json");
        let capability = include_str!("../capabilities/main.json");
        assert!(config.contains("\"windows\": []"));
        assert!(capability.contains("\"local\": true"));
        assert!(capability.contains("\"windows\": [\"main\"]"));
        for forbidden in ["shell:", "fs:", "opener:default", "http:default"] {
            assert!(!format!("{config}\n{capability}").contains(forbidden));
        }
    }

    #[test]
    fn android_webview_leaves_safe_area_insets_to_css() {
        let activity =
            include_str!("../gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt");
        let edge_to_edge = activity
            .find("enableEdgeToEdge()")
            .expect("edge-to-edge call");
        let super_create = activity
            .find("super.onCreate(savedInstanceState)")
            .expect("super onCreate call");
        assert!(edge_to_edge < super_create);
        for forbidden in ["onWebViewCreate", "WindowInsetsCompat", "setPadding"] {
            assert!(!activity.contains(forbidden));
        }
    }
}
