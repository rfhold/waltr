//! Tauri shell and IPC surface for the Waltr native application.

mod bridge;
mod credential_store;
mod observability;
mod policy;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(target_os = "android")]
use std::sync::{Mutex, OnceLock};

use bridge::{BridgeEvent, BridgePayload, BridgeRecovery, EventFeed, RuntimeBridge};
#[cfg(target_os = "android")]
use waltr_client::NativeAuthorizationCallback;
use waltr_client::{ClientCommand, NativeAccessAction};
use serde::Serialize;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::webview::NewWindowResponse;
use tauri::webview::PageLoadEvent;
use tauri::{Emitter, Manager, State, WebviewUrl};
use tauri_plugin_opener::OpenerExt;
use tracing::Instrument as _;

const BRIDGE_EVENT: &str = "waltr://bridge";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStarted {
    generation: u64,
}

#[tauri::command]
async fn start_runtime(
    app: tauri::AppHandle,
    bridge: State<'_, Arc<RuntimeBridge>>,
) -> Result<RuntimeStarted, String> {
    let span = observability::operation_span("runtime.start", "ipc");
    let result = (|| {
        let feed = bridge.start()?;
        let generation = feed.generation();
        deliver_events(app, Arc::clone(bridge.inner()), feed);
        Ok(RuntimeStarted { generation })
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
async fn access_start(bridge: State<'_, Arc<RuntimeBridge>>) -> Result<(), String> {
    observe_access_ipc(bridge.inner(), NativeAccessAction::Start, "access.start").await
}

#[tauri::command]
async fn access_cancel(bridge: State<'_, Arc<RuntimeBridge>>) -> Result<(), String> {
    observe_access_ipc(bridge.inner(), NativeAccessAction::Cancel, "access.cancel").await
}

#[tauri::command]
async fn access_logout(
    bridge: State<'_, Arc<RuntimeBridge>>,
) -> Result<Option<BridgeEvent<BridgePayload>>, String> {
    let span = observability::operation_span("access.logout", "ipc");
    let result = bridge
        .access(NativeAccessAction::Logout)
        .instrument(span.clone())
        .await;
    observability::record_result(&span, &result);
    result
}

async fn observe_access_ipc(
    bridge: &RuntimeBridge,
    action: NativeAccessAction,
    operation: &'static str,
) -> Result<(), String> {
    let span = observability::operation_span(operation, "ipc");
    let result = bridge
        .access(action)
        .instrument(span.clone())
        .await
        .map(drop);
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
    bridge.stop();
    observability::record_outcome(&span, "ok");
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
                .map_err(|error| error.to_string())
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

fn start_and_deliver(app: tauri::AppHandle, bridge: Arc<RuntimeBridge>) -> Result<u64, String> {
    let feed = bridge.start()?;
    let generation = feed.generation();
    deliver_events(app, bridge, feed);
    Ok(generation)
}

#[cfg(target_os = "android")]
struct MobileLifecycle {
    app: tauri::AppHandle,
    bridge: Arc<RuntimeBridge>,
    suspended: bool,
    preserved: bool,
}

#[cfg(target_os = "android")]
static MOBILE_LIFECYCLE: OnceLock<Mutex<MobileLifecycle>> = OnceLock::new();
#[cfg(target_os = "android")]
static MOBILE_CALLBACK_BRIDGE: OnceLock<Arc<RuntimeBridge>> = OnceLock::new();

#[cfg(target_os = "android")]
fn install_mobile_lifecycle(app: tauri::AppHandle, bridge: Arc<RuntimeBridge>) {
    drop(MOBILE_CALLBACK_BRIDGE.set(Arc::clone(&bridge)));
    drop(MOBILE_LIFECYCLE.set(Mutex::new(MobileLifecycle {
        app,
        bridge,
        suspended: false,
        preserved: false,
    })));
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.MainActivity")]
/// Read one opaque Android intent callback into the Rust-only handoff lane.
pub fn nativeReceiveIntent(
    mut env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    intent: jni::objects::JObject<'_>,
) {
    if intent.is_null() {
        return;
    }
    let Ok(value) = env.call_method(intent, "getDataString", "()Ljava/lang/String;", &[]) else {
        return;
    };
    let Ok(value) = value.l() else {
        return;
    };
    if value.is_null() {
        return;
    }
    let value = jni::objects::JString::from(value);
    let Ok(value) = env.get_string(&value) else {
        return;
    };
    let Some(bridge) = MOBILE_CALLBACK_BRIDGE.get() else {
        return;
    };
    bridge.authorization_callback(NativeAuthorizationCallback::new(value.into()));
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.MainActivity")]
/// Stop native runtime work and clear credentials before Android suspension.
pub fn nativeSuspend(_env: jni::JNIEnv<'_>, _class: jni::objects::JClass<'_>) {
    let span = observability::operation_span("android.suspend", "lifecycle");
    let Some(lifecycle) = MOBILE_LIFECYCLE.get() else {
        observability::record_outcome(&span, "unavailable");
        return;
    };
    let mut lifecycle = lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !lifecycle.suspended {
        lifecycle.suspended = true;
        lifecycle.preserved = lifecycle.bridge.suspend_for_browser_handoff();
        if !lifecycle.preserved {
            let bridge = Arc::clone(&lifecycle.bridge);
            drop(lifecycle);
            bridge.stop();
            observability::record_outcome(&span, "stopped");
        } else {
            observability::record_outcome(&span, "preserved");
        }
    } else {
        observability::record_outcome(&span, "unchanged");
    }
}

#[cfg(target_os = "android")]
#[jni_fn::jni_fn("dev.waltr.app.MainActivity")]
/// Start a fresh runtime generation after Android resumes from suspension.
pub fn nativeResume(_env: jni::JNIEnv<'_>, _class: jni::objects::JClass<'_>) {
    let span = observability::operation_span("android.resume", "lifecycle");
    let Some(lifecycle) = MOBILE_LIFECYCLE.get() else {
        observability::record_outcome(&span, "unavailable");
        return;
    };
    let mut lifecycle = lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if lifecycle.suspended {
        lifecycle.suspended = false;
        let preserved = std::mem::take(&mut lifecycle.preserved);
        if preserved && lifecycle.bridge.resume_browser_handoff() {
            observability::record_outcome(&span, "preserved");
            return;
        }
        lifecycle.bridge.stop();
        if start_and_deliver(lifecycle.app.clone(), Arc::clone(&lifecycle.bridge)).is_err() {
            lifecycle.suspended = true;
            observability::record_outcome(&span, "error");
        } else {
            observability::record_outcome(&span, "restarted");
        }
    } else {
        observability::record_outcome(&span, "unchanged");
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
    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    let app = builder
        .setup(|app| {
            let setup = observability::operation_span("app.setup", "lifecycle");
            let _setup_entered = setup.enter();
            let opener = app.handle().clone();
            let bridge = Arc::new(RuntimeBridge::new(move |url| {
                opener.opener().open_url(url, None::<&str>).is_ok()
            }));
            app.manage(Arc::clone(&bridge));

            #[cfg(target_os = "android")]
            install_mobile_lifecycle(app.handle().clone(), Arc::clone(&bridge));

            let first_page_load = Arc::new(AtomicBool::new(true));
            let page_load_bridge = Arc::clone(&bridge);
            let window =
                tauri::WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                    .on_navigation(policy::allows_packaged_navigation)
                    .on_page_load(move |_, payload| {
                        if payload.event() == PageLoadEvent::Started
                            && !first_page_load.swap(false, Ordering::AcqRel)
                        {
                            observability::lifecycle_event(
                                "webview.page_load",
                                "lifecycle",
                                "runtime_stopped",
                            );
                            page_load_bridge.stop();
                        }
                    });
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let window = window
                .title("Waltr")
                .inner_size(1180.0, 780.0)
                .min_inner_size(720.0, 540.0)
                .on_new_window(|_, _| NewWindowResponse::Deny);
            let webview = observability::operation_span("webview.build", "startup");
            let build_result = window.build();
            observability::record_result(&webview, &build_result);
            build_result?;

            let result = start_and_deliver(app.handle().clone(), bridge)
                .map(drop)
                .map_err(std::io::Error::other);
            observability::record_result(&setup, &result);
            result?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_runtime,
            client_dispatch,
            access_start,
            access_cancel,
            access_logout,
            client_state,
            dispose_runtime,
            open_external_url,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Waltr native application");
    observability::record_outcome(&startup, "ok");
    drop(startup_entered);
    drop(startup);
    app.run(|app, event| match event {
        tauri::RunEvent::Exit => {
            observability::lifecycle_event("app.shutdown", "lifecycle", "exit");
            app.state::<Arc<RuntimeBridge>>().stop();
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Destroyed,
            ..
        } if label == "main" => {
            observability::lifecycle_event("webview.destroyed", "lifecycle", "runtime_stopped");
            app.state::<Arc<RuntimeBridge>>().stop();
        }
        _ => {}
    });
}

#[cfg(test)]
mod configuration_tests {
    #[test]
    fn config_and_capabilities_are_local_and_minimal() {
        let config = include_str!("../tauri.conf.json");
        let capability = include_str!("../capabilities/main.json");
        let android_manifest = include_str!("../gen/android/app/src/main/AndroidManifest.xml");
        let backup_rules = include_str!("../gen/android/app/src/main/res/xml/backup_rules.xml");
        let data_extraction_rules =
            include_str!("../gen/android/app/src/main/res/xml/data_extraction_rules.xml");
        let android_plugins = include_str!("../gen/android/app/build.gradle.kts");
        let android_activity =
            include_str!("../gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt");
        let manifest = include_str!("../Cargo.toml");
        let rust_source = include_str!("lib.rs");
        let removed_plugin = ["tauri", "plugin", "deep", "link"].join("-");
        assert!(config.contains("\"windows\": []"));
        assert!(include_str!("../scripts/stage-web.mjs").contains("waltr-web/dist"));
        assert!(capability.contains("\"local\": true"));
        assert!(capability.contains("\"windows\": [\"main\"]"));
        assert!(!manifest.contains(&removed_plugin));
        assert!(!capability.contains("deep-link:"));
        assert!(android_manifest.contains("android:scheme=\"dev.waltr.app\""));
        assert!(android_manifest.contains("android:sspPrefix=\"/oauth2redirect?\""));
        assert!(android_manifest.contains("android.intent.category.BROWSABLE"));
        assert_eq!(android_manifest.matches("<uses-permission").count(), 1);
        assert!(android_manifest.contains("@xml/backup_rules"));
        assert!(android_manifest.contains("@xml/data_extraction_rules"));
        assert!(backup_rules.contains("keyring-waltr-refresh.xml"));
        assert_eq!(
            data_extraction_rules
                .matches("keyring-waltr-refresh.xml")
                .count(),
            2
        );
        assert!(!android_plugins.contains(&removed_plugin));
        assert!(rust_source.contains("getDataString"));
        assert!(android_activity.contains("override fun onCreate"));
        assert!(android_activity.contains("override fun onNewIntent"));
        assert_eq!(
            android_activity
                .matches("nativeReceiveIntent(intent)")
                .count(),
            2
        );
        for forbidden in ["getDataString", "intent.data", "Log.", "println("] {
            assert!(!android_activity.contains(forbidden));
        }
        for forbidden in [
            "waltr-server",
            "updater",
            "shell:",
            "fs:",
            "opener:default",
            "http:default",
        ] {
            assert!(!format!("{config}\n{capability}\n{manifest}").contains(forbidden));
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
        for forbidden in [
            "onWebViewCreate",
            "installWebViewInsets",
            "setOnApplyWindowInsetsListener",
            "WindowInsetsCompat",
            "requestApplyInsets",
            "setPadding",
        ] {
            assert!(!activity.contains(forbidden));
        }
    }

    #[test]
    fn android_safe_area_keeps_intent_and_lifecycle_forwarding() {
        let activity =
            include_str!("../gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt");

        assert_eq!(activity.matches("nativeReceiveIntent(intent)").count(), 2);
        assert_eq!(activity.matches("nativeSuspend()").count(), 2);
        assert_eq!(activity.matches("nativeResume()").count(), 2);
        assert!(activity.contains("nativeSuspend()\n    super.onPause()"));
        assert!(activity.contains("super.onResume()\n    nativeResume()"));
    }

    #[test]
    fn csp_adds_only_the_exact_faro_collector_origin() {
        let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("valid Tauri configuration");
        let csp = config["app"]["security"]["csp"]
            .as_str()
            .expect("CSP string");
        assert_eq!(
            csp,
            "default-src 'self'; connect-src ipc: http://ipc.localhost https://faro.holdenitdown.net; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; object-src 'none'; base-uri 'self'; frame-src 'none'; form-action 'none'"
        );
        assert_eq!(csp.matches("https://faro.holdenitdown.net").count(), 1);
    }
}
