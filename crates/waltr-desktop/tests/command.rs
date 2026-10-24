//! Local ephemeral process fixtures; no clipboard or live services.
#![cfg(unix)]
use tokio_util::sync::CancellationToken;
use waltr_desktop::command::{self, CommandError, CommandSpec, Stream};

fn fixture(script: &str, arguments: &[&str], stdin: &str) -> CommandSpec {
    // Only static test fixture scripts are executed by the shell. Template
    // values travel as literal positional arguments or stdin, never source.
    let mut argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        script.into(),
        "fixture".into(),
    ];
    argv.extend(arguments.iter().map(|value| (*value).into()));
    CommandSpec {
        argv,
        stdin: stdin.into(),
        timeout_seconds: 1,
    }
}

#[tokio::test]
async fn literal_stdin_arguments_and_nonrecursive_templates() {
    let text = "$(touch should-not-exist); `false` | & \" ' $HOME ~ {{ invocationId }}\n";
    let cancel = CancellationToken::new();
    let spec = fixture(
        "printf '%s' \"$1\"; cat",
        &["{{ text }}"],
        "{{ invocationId }}:{{ text }}",
    );
    let output = command::run(&spec, text, "inv-1", &cancel).await.unwrap();
    assert_eq!(output.stdout, format!("{text}inv-1:{text}").as_bytes());
    assert!(output.stderr.is_empty());
    assert!(!format!("{output:?}").contains("touch"));
}

#[tokio::test]
async fn pre_spawn_bounds_and_cancel() {
    let spec = fixture("exit 99", &[], "{{ text }}{{ text }}x");
    let cancel = CancellationToken::new();
    assert_eq!(
        command::run(
            &spec,
            &"x".repeat(command::MAX_TEXT_BYTES + 1),
            "id",
            &cancel
        )
        .await
        .unwrap_err(),
        CommandError::InputLimit
    );
    assert_eq!(
        command::run(&spec, &"x".repeat(command::MAX_TEXT_BYTES), "id", &cancel)
            .await
            .unwrap_err(),
        CommandError::ExpandedLimit
    );
    cancel.cancel();
    assert_eq!(
        command::run(&spec, "", "id", &cancel).await.unwrap_err(),
        CommandError::Cancelled
    );
    let spec = fixture("exit 99", &["{{ text }}"], "");
    assert!(matches!(
        command::run(&spec, "\0", "id", &CancellationToken::new()).await,
        Err(CommandError::Invalid(_))
    ));
}

#[tokio::test]
async fn concurrent_full_pipes() {
    let spec = fixture(
        "i=0; while [ \"$i\" -lt 6000 ]; do printf 0123456789; printf 0123456789 >&2; i=$((i+1)); done; cat >/dev/null",
        &[],
        "{{ text }}",
    );
    let result = command::run(
        &spec,
        &"x".repeat(command::MAX_TEXT_BYTES),
        "id",
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.stdout.len(), 60_000);
    assert_eq!(result.stderr.len(), 60_000);
}

#[tokio::test]
async fn output_limits_and_exit_errors_are_redacted() {
    for (script, stream) in [
        (
            "while :; do printf 012345678901234567890123456789; done",
            Stream::Stdout,
        ),
        (
            "while :; do printf 012345678901234567890123456789 >&2; done",
            Stream::Stderr,
        ),
    ] {
        assert_eq!(
            command::run(
                &fixture(script, &[], ""),
                "",
                "id",
                &CancellationToken::new()
            )
            .await
            .unwrap_err(),
            CommandError::OutputLimit { stream }
        );
    }
    let error = command::run(
        &fixture("printf secret >&2; exit 42", &[], ""),
        "",
        "id",
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, CommandError::Exit { code: Some(42) });
    assert!(!format!("{error:?}").contains("secret"));
}

async fn assert_reaped(path: &std::path::Path) {
    let pid = tokio::fs::read_to_string(path).await.unwrap();
    let status = tokio::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(!status.success(), "child was not reaped");
}

#[tokio::test]
async fn timeout_cancels_blocked_stdin_and_reaps_child() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pid");
    let spec = fixture(
        "printf '%s' \"$$\" >\"$1\"; exec sleep 30",
        &[path.to_str().unwrap()],
        "{{ text }}",
    );
    assert_eq!(
        command::run(
            &spec,
            &"x".repeat(command::MAX_TEXT_BYTES),
            "id",
            &CancellationToken::new()
        )
        .await
        .unwrap_err(),
        CommandError::Timeout
    );
    assert_reaped(&path).await;
}

#[tokio::test]
async fn cancellation_shutdown_kills_and_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pid");
    let spec = fixture(
        "printf '%s' \"$$\" >\"$1\"; exec sleep 30",
        &[path.to_str().unwrap()],
        "{{ text }}",
    );
    let cancel = CancellationToken::new();
    let signal = async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
    };
    let text = "x".repeat(command::MAX_TEXT_BYTES);
    let (result, ()) = tokio::join!(command::run(&spec, &text, "id", &cancel), signal);
    assert_eq!(result.unwrap_err(), CommandError::Cancelled);
    assert_reaped(&path).await;
}
