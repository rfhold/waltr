//! Build Apps assets and, with `web`, the bundled web client.

use std::{env, fmt::Write as _, fs, io, path::Path};

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WEB");
    println!("cargo:rerun-if-env-changed=WALTR_BUILD_REVISION");
    println!("cargo:rerun-if-env-changed=VITE_WALTR_MCP_APP_CONTAINER_URL");

    if env::var_os("CARGO_FEATURE_WEB").is_none() {
        write_apps(None);
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
        web_root.join("app-container").display()
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

    run_package(repo_root, "@rfhold/waltr-web", "build", true);

    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR should be set");
    write_assets(&dist, Path::new(&out_dir));
    write_apps(Some(&dist.join("app-container")));
}

fn run_package(repo_root: &Path, package: &str, script: &str, wasm: bool) {
    for path in ["package.json", "pnpm-workspace.yaml", "pnpm-lock.yaml"] {
        println!("cargo:rerun-if-changed={}", repo_root.join(path).display());
    }
    let mut command = std::process::Command::new("pnpm");
    command
        .args(["--filter", package, script])
        .current_dir(repo_root);
    if wasm {
        command.env("CARGO_TARGET_DIR", repo_root.join("target/web-bundle"));
        // The nested WASM build cannot reuse the parent Cargo jobserver or Clippy driver.
        for key in [
            "CARGO_MAKEFLAGS",
            "CLIPPY_ARGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ] {
            command.env_remove(key);
        }
    }
    let status = command
        .status()
        .expect("pnpm and installed workspace packages are required to build assets");
    assert!(
        status.success(),
        "pnpm {package} {script} failed with {status}"
    );
}

fn write_apps(web_container: Option<&Path>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = root.parent().and_then(Path::parent).unwrap();
    let output = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=WALTR_APP_CONTAINER_DIST");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_APPS_HOST_ASSETS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_APPS_SERVED_ASSETS");
    let host = env::var_os("CARGO_FEATURE_APPS_HOST_ASSETS").is_some();
    let served = env::var_os("CARGO_FEATURE_APPS_SERVED_ASSETS").is_some();
    let override_root = env::var_os("WALTR_APP_CONTAINER_DIST").map(std::path::PathBuf::from);
    let web_root = repo_root.join("crates/waltr-web");
    let container_dist = web_root.join("dist/app-container");
    if host && override_root.is_none() && web_container.is_none() {
        for path in [
            "src/mcp-apps",
            "app-container",
            "vite.config.ts",
            "package.json",
        ] {
            println!("cargo:rerun-if-changed={}", web_root.join(path).display());
        }
        println!(
            "cargo:rerun-if-changed={}",
            repo_root.join("pnpm-lock.yaml").display()
        );
        run_package(repo_root, "@rfhold/waltr-web", "build:app-container", false);
    }
    let host_input = override_root
        .as_deref()
        .or(web_container)
        .unwrap_or(&container_dist);
    if host {
        assert_container_assets(host_input);
    }
    let entries = include_str!("scripts/apps.entries")
        .lines()
        .map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            assert_eq!(
                fields.len(),
                2,
                "static app entries require name and source root"
            );
            let name = fields[0];
            assert!(
                name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                    && name.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                "invalid static app name"
            );
            (name, fields[1])
        })
        .collect::<Vec<_>>();
    let mut names = std::collections::HashSet::new();
    assert!(
        entries.iter().all(|(name, _)| names.insert(*name)),
        "duplicate static app name"
    );
    if served {
        for (_, input) in &entries {
            println!("cargo:rerun-if-changed={}", root.join(input).display());
        }
        for path in ["scripts", "package.json", "tsconfig.json", "vite.config.ts"] {
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
        println!(
            "cargo:rerun-if-changed={}",
            repo_root.join("pnpm-lock.yaml").display()
        );
        run_package(repo_root, "@rfhold/waltr-server", "build", false);
    }
    let mut served_source = String::new();
    for (name, _) in entries {
        let constant = format!("{}_HTML", name.to_ascii_uppercase());
        if !served {
            writeln!(
                served_source,
                "pub(super) const {constant}: Option<&str> = None;"
            )
            .unwrap();
            continue;
        }
        let dist = root.join("dist").join(name);
        let files = fs::read_dir(&dist)
            .expect("app dist must be readable")
            .map(|entry| entry.expect("app asset must be readable").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            ["index.html"],
            "app dist must contain only the self-contained HTML"
        );
        let input = dist.join("index.html");
        let metadata = fs::metadata(&input).expect("app HTML must be readable");
        assert!(
            metadata.is_file() && metadata.len() > 0 && metadata.len() <= 1024 * 1024,
            "app HTML must be a nonempty file of at most 1 MiB"
        );
        let html = fs::read_to_string(input).expect("app HTML must be UTF-8");
        assert!(
            !html.is_empty() && html.len() <= 1024 * 1024,
            "app HTML must be nonempty and at most 1 MiB"
        );
        assert!(
            html.contains("<script type=\"module\""),
            "app HTML must be built by Vite"
        );
        let bundled = output.join(format!("{name}.html"));
        fs::write(&bundled, html).expect("app HTML must be copyable");
        writeln!(
            served_source,
            "pub(super) const {constant}: Option<&str> = Some(include_str!({bundled:?}));"
        )
        .unwrap();
    }
    fs::write(output.join("served_app_assets.rs"), served_source).unwrap();
    let mut source = String::new();
    for (name, file) in [
        ("CONTAINER_HTML", "container.html"),
        ("CONTAINER_JS", "container.js"),
    ] {
        if host {
            let bundled = output.join(file);
            let input = host_input.join(file);
            if override_root.is_some() {
                println!("cargo:rerun-if-changed={}", input.display());
            }
            let bytes = fs::read(&input).unwrap_or_else(|error| {
                panic!(
                    "MCP Apps container {} cannot be read: {error}",
                    input.display()
                )
            });
            assert!(
                bytes.len() <= 1024 * 1024,
                "MCP Apps container asset exceeds 1 MiB"
            );
            std::str::from_utf8(&bytes).expect("MCP Apps assets must be UTF-8");
            fs::write(&bundled, bytes).unwrap();
            writeln!(
                source,
                "pub(crate) const {name}: Option<&str> = Some(include_str!({:?}));",
                bundled
            )
            .unwrap();
        } else {
            writeln!(source, "pub(crate) const {name}: Option<&str> = None;").unwrap();
        }
    }
    fs::write(output.join("apps_assets.rs"), source).unwrap();
}

fn assert_container_assets(root: &Path) {
    let mut files = fs::read_dir(root)
        .expect("container dist must be readable")
        .map(|entry| entry.expect("container asset must be readable").file_name())
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(
        files,
        ["container.html", "container.js"],
        "container dist must contain only the fixed, self-contained assets"
    );
}

fn write_assets(dist: &Path, out_dir: &Path) {
    let generated = out_dir.join("web_assets.rs");
    let bundled_assets = out_dir.join("web-assets");
    if bundled_assets.exists() {
        fs::remove_dir_all(&bundled_assets).expect("stale bundled web assets should be removable");
    }
    let mut assets = Vec::new();
    collect_assets(dist, dist, &mut assets).expect("web dist assets should be readable");
    assets.sort_by(|left, right| left.0.cmp(&right.0));

    let mut source = String::new();
    source.push_str("static WEB_ASSETS: &[WebAsset] = &[\n");
    for (path, file) in assets {
        let bundled_file = bundled_assets.join(path.trim_start_matches('/'));
        fs::create_dir_all(
            bundled_file
                .parent()
                .expect("bundled asset should have a parent directory"),
        )
        .expect("bundled web asset directory should be creatable");
        fs::copy(&file, &bundled_file).expect("bundled web asset should be copyable");
        let include_path = bundled_file.to_string_lossy();
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
            if route.starts_with("/apps/") || route.starts_with("/app-container/") {
                continue;
            }
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
