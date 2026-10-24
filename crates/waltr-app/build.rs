//! Tauri build-time configuration generation.

mod build_revision;

fn main() {
    println!("cargo:rerun-if-env-changed=WALTR_APP_REVISION");
    println!("cargo:rerun-if-changed=build_revision.rs");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let manifest = std::path::PathBuf::from(manifest);
    for path in build_revision::watch_paths(&manifest) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let revision = build_revision::revision(
        &manifest,
        std::env::var("WALTR_APP_REVISION").ok().as_deref(),
    );
    println!("cargo:rustc-env=WALTR_APP_REVISION={revision}");
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=WALTR_APP_TARGET={target}");

    std::fs::create_dir_all(manifest.join("frontend-dist"))
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
