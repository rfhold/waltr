//! Build bundled web assets when the server `web` feature is enabled.

use std::{env, fmt::Write as _, fs, io, path::Path};

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WEB");
    println!("cargo:rerun-if-env-changed=WALTR_BUILD_REVISION");

    if env::var_os("CARGO_FEATURE_WEB").is_none() {
        return;
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("server crate should live under crates/");
    let web_root = repo_root.join("crates/waltr-web");
    let dist = web_root.join("dist");

    println!("cargo:rerun-if-changed={}", web_root.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        web_root.join("scripts").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_root.join("index.html").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_root.join("package.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_root.join("vite.config.ts").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_root.join("tsconfig.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("pnpm-lock.yaml").display()
    );

    let mut command = std::process::Command::new("pnpm");
    command
        .arg("--filter")
        .arg("@rfhold/waltr-web")
        .arg("build")
        .current_dir(repo_root)
        .env("CARGO_TARGET_DIR", repo_root.join("target/web-bundle"));
    // The nested WASM build cannot reuse the parent Cargo jobserver or Clippy driver.
    for key in [
        "CARGO_MAKEFLAGS",
        "CLIPPY_ARGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(key);
    }
    let status = command
        .status()
        .expect("pnpm should be available to build bundled web assets");

    assert!(status.success(), "pnpm web build failed with {status}");

    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR should be set");
    let generated = Path::new(&out_dir).join("web_assets.rs");
    let mut assets = Vec::new();
    collect_assets(&dist, &dist, &mut assets).expect("web dist assets should be readable");
    assets.sort_by(|left, right| left.0.cmp(&right.0));

    let mut source = String::new();
    source.push_str("static WEB_ASSETS: &[WebAsset] = &[\n");
    for (path, file) in assets {
        let include_path = file.to_string_lossy();
        writeln!(
            source,
            "    WebAsset {{ path: {path:?}, content_type: {:?}, bytes: include_bytes!({include_path:?}) }},",
            content_type(&file)
        )
        .expect("writing generated asset source should succeed");
    }
    source.push_str("];\n");

    fs::write(generated, source).expect("generated web asset source should be writable");
}

fn collect_assets(
    root: &Path,
    dir: &Path,
    assets: &mut Vec<(String, std::path::PathBuf)>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            collect_assets(root, &path, assets)?;
            continue;
        }

        if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("asset should be under dist root");
            let route = format!("/{}", relative.to_string_lossy().replace('\\', "/"));
            assets.push((route, path));
        }
    }

    Ok(())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("wasm") => "application/wasm",
        Some("webmanifest") => "application/manifest+json; charset=utf-8",
        Some("webp") => "image/webp",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
