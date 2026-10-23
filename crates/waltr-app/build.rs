//! Tauri build-time configuration generation.

fn main() {
    println!("cargo:rerun-if-env-changed=WALTR_APP_REVISION");
    let revision = std::env::var("WALTR_APP_REVISION")
        .ok()
        .and_then(|value| sanitize_revision(&value))
        .or_else(git_revision)
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=WALTR_APP_REVISION={revision}");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=WALTR_APP_TARGET={target}");

    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    std::fs::create_dir_all(std::path::PathBuf::from(manifest).join("frontend-dist"))
        .expect("create generated frontend staging directory");
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "start_runtime",
            "dictation_start",
            "dictation_finish",
            "dictation_status",
            "dictation_cancel",
            "client_dispatch",
            "client_state",
            "dispose_runtime",
            "open_external_url",
            "open_authorization_attempt",
        ]),
    ))
    .expect("generate Tauri app command permissions");
}

fn git_revision() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .and_then(|value| sanitize_revision(&value))
}

fn sanitize_revision(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() >= 7 && value.len() <= 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}
