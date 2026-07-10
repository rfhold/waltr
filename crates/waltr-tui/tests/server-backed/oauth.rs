use std::{path::PathBuf, process::Command};

#[test]
fn oauth_login_delegate_returns_redirect_url_when_enabled() {
    if std::env::var("WALTR_OAUTH_AUTH_URL").is_err() {
        eprintln!("skipping OAuth login delegate test: set WALTR_OAUTH_AUTH_URL");
        return;
    }

    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipping OAuth login delegate test: node is not available");
        return;
    }

    if Command::new("docker").arg("--version").output().is_err() {
        eprintln!("skipping OAuth login delegate test: docker is not available");
        return;
    }

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root should resolve");
    let script = repo_root.join("scripts/oauth-login-k6.mjs");

    if !script.exists() {
        eprintln!(
            "skipping OAuth login delegate test: {} is missing",
            script.display()
        );
        return;
    }

    let output = Command::new("node")
        .arg(script)
        .current_dir(repo_root)
        .output()
        .expect("OAuth login delegate should launch");

    if output.status.code() == Some(77) {
        eprintln!("skipping OAuth login delegate test: delegate reported missing prerequisites");
        return;
    }

    assert!(
        output.status.success(),
        "OAuth login delegate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("code="),
        "OAuth login delegate should print a redirect URL with code"
    );
}
