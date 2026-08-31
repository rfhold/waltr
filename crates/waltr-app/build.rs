//! Tauri build-time configuration generation.

fn main() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    std::fs::create_dir_all(std::path::PathBuf::from(manifest).join("frontend-dist"))
        .expect("create generated frontend staging directory");
    tauri_build::build();
}
