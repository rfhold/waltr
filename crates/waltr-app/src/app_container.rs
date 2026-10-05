//! Desktop IPC confinement, independent of Apps availability.

/// Replace only the stock IPC transport; Tauri still installs its ordinary core API.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows", test))]
pub fn confined_invoke_script() -> String {
    include_str!("../scripts/confined-ipc.js")
        .replace("__WALTR_OS_NAME__", &format!("\"{}\"", std::env::consts::OS))
        .replace(
            "__WALTR_PROCESS_IPC_MESSAGE_FN__",
            include_str!("../scripts/process-ipc-message-fn.js"),
        )
}

pub(crate) fn configure_apps_csp(config: &mut tauri::Config) {
    configure_apps_csp_for_target(
        config,
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(feature = "native-apps-test"),
    );
}

fn configure_apps_csp_for_target(
    config: &mut tauri::Config,
    os: &str,
    arch: &str,
    diagnostic: bool,
) {
    use std::collections::HashMap;
    use tauri::utils::config::{Csp, CspDirectiveSources};

    let sources = if os == "macos" && arch == "aarch64" {
        if diagnostic {
            "http://127.0.0.1:*/container.html http://127.0.0.1:*/probe/container.html"
        } else {
            "http://127.0.0.1:*/container.html"
        }
    } else {
        "'none'"
    };
    // Tauri generates nonce/hash metadata while embedding assets, then builds the actual
    // response CSP from Context.config. Change only frame-src before that config is owned.
    for csp in [
        &mut config.app.security.csp,
        &mut config.app.security.dev_csp,
    ]
    .into_iter()
    .flatten()
    {
        let mut directives: HashMap<String, CspDirectiveSources> = csp.clone().into();
        directives.insert(
            "frame-src".to_owned(),
            CspDirectiveSources::Inline(sources.to_owned()),
        );
        *csp = Csp::DirectiveMap(directives);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_leaves_only_tauri_key_substitution() {
        let script = confined_invoke_script();
        assert_eq!(script.matches("__INVOKE_KEY__").count(), 1);
        assert!(!script.contains("__WALTR_"));
        let guard = script
            .find("if (window.top !== window || !packaged) return")
            .unwrap();
        let key = script.find("const __TAURI_INVOKE_KEY__ =").unwrap();
        assert!(guard < key);
    }

    #[test]
    fn non_macos_apps_remain_disabled_by_csp() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let csp = config["app"]["security"]["csp"].as_str().unwrap();
        assert!(
            csp.split(';')
                .any(|directive| directive.trim() == "frame-src 'none'")
        );
        assert!(config["build"].get("devUrl").is_none());
    }

    #[test]
    fn architecture_policy_changes_only_frame_src_and_never_enables_unsupported_targets() {
        use std::collections::HashMap;
        use tauri::utils::config::{Csp, CspDirectiveSources};
        let base: tauri::Config = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let original: HashMap<String, CspDirectiveSources> =
            base.app.security.csp.clone().unwrap().into();
        for os in ["macos", "linux", "windows", "android", "ios", "unknown"] {
            for arch in ["aarch64", "x86_64", "x86", "arm", "unknown"] {
                for diagnostic in [false, true] {
                    let mut config = base.clone();
                    config.app.security.dev_csp = config.app.security.csp.clone();
                    configure_apps_csp_for_target(&mut config, os, arch, diagnostic);
                    let expected = match (os, arch, diagnostic) {
                        ("macos", "aarch64", false) => "http://127.0.0.1:*/container.html",
                        ("macos", "aarch64", true) => {
                            "http://127.0.0.1:*/container.html http://127.0.0.1:*/probe/container.html"
                        }
                        _ => "'none'",
                    };
                    for csp in [config.app.security.csp, config.app.security.dev_csp] {
                        let mut directives: HashMap<String, CspDirectiveSources> =
                            csp.unwrap().into();
                        let frames: Vec<String> = directives.remove("frame-src").unwrap().into();
                        assert_eq!(frames.join(" "), expected, "{os}/{arch}/{diagnostic}");
                        directives.insert("frame-src".to_owned(), original["frame-src"].clone());
                        assert_eq!(
                            Csp::DirectiveMap(directives),
                            Csp::DirectiveMap(original.clone())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_app_command_has_explicit_main_permission() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
        let permissions = capability["permissions"].as_array().unwrap();
        for command in [
            "start_runtime",
            "client_dispatch",
            "client_state",
            "dispose_runtime",
            "open_external_url",
            "open_authorization_attempt",
        ] {
            assert!(include_str!("../build.rs").contains(&format!("\"{command}\"")));
            assert!(permissions.iter().any(|permission| permission.as_str()
                == Some(&format!("allow-{}", command.replace('_', "-")))));
        }
        assert!(capability.get("remote").is_none());
    }

    #[test]
    fn native_gate_rejects_missing_or_guessed_key_before_dispatch_and_channel_fetch() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let dispatched = Arc::new(AtomicUsize::new(0));
        let handler_count = Arc::clone(&dispatched);
        let app = tauri::test::mock_builder()
            .invoke_system(confined_invoke_script())
            .invoke_handler(move |invoke| {
                handler_count.fetch_add(1, Ordering::SeqCst);
                invoke.resolver.resolve(());
                true
            })
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        for command in [
            "client_state",
            "plugin:core|bogus",
            "plugin:__TAURI_CHANNEL__|fetch",
        ] {
            for key in ["", "guessed"] {
                let responses = Arc::new(AtomicUsize::new(0));
                let response_count = Arc::clone(&responses);
                window.as_ref().clone().on_message(
                    tauri::webview::InvokeRequest {
                        cmd: command.to_owned(),
                        callback: tauri::ipc::CallbackFn(0),
                        error: tauri::ipc::CallbackFn(1),
                        // Deliberately claim the containing main origin: URL is not a frame proof.
                        url: "tauri://localhost/".parse().unwrap(),
                        body: tauri::ipc::InvokeBody::default(),
                        headers: Default::default(),
                        invoke_key: key.to_owned(),
                    },
                    Box::new(move |_, _, _, _, _| {
                        response_count.fetch_add(1, Ordering::SeqCst);
                    }),
                );
                assert_eq!(responses.load(Ordering::SeqCst), 0);
                assert_eq!(dispatched.load(Ordering::SeqCst), 0);
            }
        }
        tauri::test::assert_ipc_response(
            &window,
            tauri::webview::InvokeRequest {
                cmd: "client_state".to_owned(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: "tauri://localhost/".parse().unwrap(),
                body: tauri::ipc::InvokeBody::default(),
                headers: Default::default(),
                invoke_key: tauri::test::INVOKE_KEY.to_owned(),
            },
            Ok::<(), ()>(()),
        );
        assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    }
}
